//! Dyktowanie do dowolnego pola — port `Dictation.swift` z macOS (a ten
//! z whistlera/OpenWhispr): skrót → mikrofon → whisper-server → schowek +
//! Ctrl+V → stary schowek wraca.
//!
//! Tryby jak tam: `push` — trzymasz skrót i mówisz; `toggle` — naciśnięcie
//! zaczyna, kolejne kończy. Skrót rejestruje `RegisterHotKey` (wtyczka
//! global-shortcut), więc Ctrl+Shift+J nie dociera przy okazji do aplikacji,
//! w której piszesz.

use crate::audio::{AudioSource, Capture};
use crate::model_downloader;
use crate::recorder::{Check, Recorder};
use crate::settings::Settings;
use crate::whisper_client::{Quality, WhisperClient};
use crate::whisper_server::{self, WhisperServer};
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

pub const PILL: &str = "dictation-pill";
const SAMPLE_RATE: usize = 16_000;
/// Krótsze nagranie to przypadkowe dotknięcie — whisper zmyśliłby zdanie.
const MIN_SAMPLES: usize = SAMPLE_RATE * 4 / 10;
/// Tyle czekamy, zanim przywrócimy schowek (jak whistler).
const RESTORE_DELAY: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "message")]
pub enum Phase {
    Idle,
    Recording,
    Transcribing,
    Pasted,
    Error(String),
}

/// Serwer dyktowania. Rozgrzewany od razu po włączeniu — zimny start (model
/// + shadery Vulkan) to sekundy, których nie może zjeść pierwsze dyktowanie.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "message")]
pub enum ServerState {
    Off,
    Starting(String),
    Ready,
    Failed(String),
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub enabled: bool,
    pub phase: Phase,
    pub server: ServerState,
    pub hotkey: String,
    pub mode: String,
    /// Skrót wciśnięty teraz — lampka w oknie dyktowania.
    pub hotkey_held: bool,
    pub hotkey_error: Option<String>,
    pub capturing: bool,
    pub testing: bool,
    pub test_result: Option<String>,
    pub mute_system: bool,
}

struct Inner {
    phase: Phase,
    server: ServerState,
    held: bool,
    hotkey_error: Option<String>,
    capturing: bool,
    testing: bool,
    test_result: Option<String>,
    capture: Option<Capture>,
    registered: Option<Shortcut>,
    /// Model i port, na których stoi nasz serwer — zmiana = restart.
    server_for: Option<(String, u16)>,
    /// Wyjście wyciszone przez nas (tylko takie odciszamy).
    muted_output: bool,
    /// Podbijane przy każdej zmianie fazy — opóźnione chowanie pigułki
    /// nie może schować nowego nagrania.
    generation: u64,
}

pub struct Dictation {
    app: AppHandle,
    inner: Mutex<Inner>,
    samples: Arc<Mutex<Vec<f32>>>,
    server: tokio::sync::Mutex<WhisperServer>,
}

impl Dictation {
    pub fn new(app: AppHandle, vendor: std::path::PathBuf) -> Arc<Self> {
        Arc::new(Self {
            app,
            inner: Mutex::new(Inner {
                phase: Phase::Idle,
                server: ServerState::Off,
                held: false,
                hotkey_error: None,
                capturing: false,
                testing: false,
                test_result: None,
                capture: None,
                registered: None,
                server_for: None,
                muted_output: false,
                generation: 0,
            }),
            samples: Arc::new(Mutex::new(Vec::new())),
            server: tokio::sync::Mutex::new(WhisperServer::new(vendor)),
        })
    }

    fn settings(&self) -> Settings {
        self.app.state::<Arc<Recorder>>().settings()
    }

    /// Zapis ustawień z poziomu dyktowania (skrót, ikona w pigułce).
    fn update_settings(&self, change: impl FnOnce(&mut Settings)) {
        let rec = self.app.state::<Arc<Recorder>>();
        let settings = {
            let mut guard = rec.settings.lock().unwrap();
            change(&mut guard);
            guard.clone()
        };
        let _ = settings.save();
        let _ = self.app.emit("settings", &settings);
    }

    pub fn view(&self) -> View {
        let settings = self.settings();
        let inner = self.inner.lock().unwrap();
        View {
            enabled: settings.dictation_enabled,
            phase: inner.phase.clone(),
            server: inner.server.clone(),
            hotkey: settings.dictation_hotkey.clone(),
            mode: settings.dictation_mode.clone(),
            hotkey_held: inner.held,
            hotkey_error: inner.hotkey_error.clone(),
            capturing: inner.capturing,
            testing: inner.testing,
            test_result: inner.test_result.clone(),
            mute_system: settings.dictation_mute_system,
        }
    }

    fn publish(&self) {
        let _ = self.app.emit("dictation", self.view());
    }

    // MARK: - cykl życia

    /// Po starcie i po każdym zapisie ustawień: skrót, serwer, model.
    pub fn sync(self: &Arc<Self>) {
        let settings = self.settings();
        self.register_hotkey(&settings);
        if settings.dictation_enabled {
            let wanted = (settings.dictation_model.clone(), dictation_port(&settings));
            let stale = {
                let inner = self.inner.lock().unwrap();
                inner.server_for.as_ref().is_some_and(|s| *s != wanted)
            };
            if stale {
                // Inny model albo port — stary serwer odpowiadałby dalej starym.
                let this = self.clone();
                tauri::async_runtime::spawn(async move {
                    this.server.lock().await.stop();
                    this.set_server(ServerState::Off);
                    this.inner.lock().unwrap().server_for = None;
                    this.prewarm();
                });
            } else {
                self.prewarm();
            }
        } else {
            if self.inner.lock().unwrap().phase == Phase::Recording {
                self.cancel_recording();
            }
            self.restore_output();
            // Model dyktowania to 1,6 GB pamięci — wyłączone dyktowanie ją oddaje.
            let this = self.clone();
            tauri::async_runtime::spawn(async move {
                this.server.lock().await.stop();
                this.inner.lock().unwrap().server_for = None;
                this.set_server(ServerState::Off);
            });
        }
        self.publish();
    }

    /// Wyjście z aplikacji nie może zostawić wyciszonego systemu.
    pub fn shutdown(&self) {
        self.restore_output();
    }

    // MARK: - skrót

    fn register_hotkey(self: &Arc<Self>, settings: &Settings) {
        let shortcuts = self.app.global_shortcut();
        let previous = self.inner.lock().unwrap().registered.take();
        if let Some(previous) = previous {
            let _ = shortcuts.unregister(previous);
        }
        let mut error = None;
        let capturing = self.inner.lock().unwrap().capturing;
        if settings.dictation_enabled && !capturing {
            match settings.dictation_hotkey.parse::<Shortcut>() {
                Ok(shortcut) => {
                    let result = shortcuts.on_shortcut(shortcut, |app, _shortcut, event| {
                        let dictation = app.state::<Arc<Dictation>>().inner().clone();
                        match event.state() {
                            ShortcutState::Pressed => dictation.pressed(),
                            ShortcutState::Released => dictation.released(),
                        }
                    });
                    match result {
                        Ok(()) => self.inner.lock().unwrap().registered = Some(shortcut),
                        Err(_) => {
                            error = Some(format!(
                                "Skrót {} jest zajęty przez system albo inną aplikację. Wybierz inny.",
                                settings.dictation_hotkey
                            ))
                        }
                    }
                }
                Err(_) => error = Some(format!("Nie rozumiem skrótu „{}”.", settings.dictation_hotkey)),
            }
        }
        self.inner.lock().unwrap().hotkey_error = error;
    }

    fn pressed(self: &Arc<Self>) {
        {
            let mut inner = self.inner.lock().unwrap();
            // RegisterHotKey bez powtórzeń, ale strzeżonego Pan Bóg strzeże.
            if inner.capturing || inner.held {
                return;
            }
            inner.held = true;
        }
        if self.settings().dictation_mode == "toggle" {
            self.toggle();
        } else {
            self.begin();
        }
        self.publish();
    }

    fn released(self: &Arc<Self>) {
        {
            let mut inner = self.inner.lock().unwrap();
            if !inner.held {
                return;
            }
            inner.held = false;
        }
        if self.settings().dictation_mode != "toggle" {
            self.finish();
        }
        self.publish();
    }

    fn toggle(self: &Arc<Self>) {
        if self.inner.lock().unwrap().phase == Phase::Recording {
            self.finish();
        } else {
            self.begin();
        }
    }

    /// Okno nagrywa nowy skrót: obecny nie może w tym czasie działać (i tak
    /// połknąłby naciśnięcie, zanim zobaczy je okno).
    pub fn set_capturing(self: &Arc<Self>, capturing: bool) {
        self.inner.lock().unwrap().capturing = capturing;
        let settings = self.settings();
        self.register_hotkey(&settings);
        self.publish();
    }

    pub fn set_hotkey(self: &Arc<Self>, hotkey: String) -> Result<(), String> {
        hotkey
            .parse::<Shortcut>()
            .map_err(|_| format!("Nie rozumiem skrótu „{hotkey}”."))?;
        self.update_settings(|s| s.dictation_hotkey = hotkey);
        self.inner.lock().unwrap().capturing = false;
        let settings = self.settings();
        self.register_hotkey(&settings);
        self.publish();
        Ok(())
    }

    /// Ikona głośnika w pigułce: przełącza opcję i od razu działa
    /// w trwającym nagraniu.
    pub fn toggle_mute(self: &Arc<Self>) {
        self.update_settings(|s| s.dictation_mute_system = !s.dictation_mute_system);
        if self.inner.lock().unwrap().phase == Phase::Recording {
            if self.settings().dictation_mute_system {
                self.mute_output();
            } else {
                self.restore_output();
            }
        }
        self.publish();
    }

    // MARK: - serwer

    fn set_server(&self, state: ServerState) {
        self.inner.lock().unwrap().server = state;
        self.publish();
    }

    pub fn prewarm(self: &Arc<Self>) {
        {
            let inner = self.inner.lock().unwrap();
            if matches!(inner.server, ServerState::Starting(_) | ServerState::Ready) {
                return;
            }
        }
        self.set_server(ServerState::Starting("Uruchamiam i wczytuję model…".into()));
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            match this.ensure_server().await {
                Ok(client) => {
                    // Pierwsze zapytanie po starcie przygotowuje GPU — niech to
                    // będzie sekunda ciszy teraz, a nie pierwsze dyktowanie.
                    let _ = client.transcribe(&vec![0.0; SAMPLE_RATE], "", Quality::FAST).await;
                    this.set_server(ServerState::Ready);
                }
                Err(err) => this.set_server(ServerState::Failed(err)),
            }
        });
    }

    pub async fn download_model(self: &Arc<Self>) {
        let model = self.settings().dictation_model;
        let this = self.clone();
        let result = model_downloader::download(&model, move |p| {
            this.inner.lock().unwrap().server =
                ServerState::Starting(format!("Pobieram model… {:.0}%", p.fraction * 100.0));
            this.publish();
        })
        .await;
        match result {
            Ok(()) => {
                self.set_server(ServerState::Off);
                self.prewarm();
            }
            Err(err) => self.set_server(ServerState::Failed(err.to_string())),
        }
    }

    /// Ten sam model co w rozmowie = ten sam serwer; inny = osobny port.
    async fn ensure_server(&self) -> Result<WhisperClient, String> {
        let settings = self.settings();
        let port = dictation_port(&settings);
        let client = WhisperClient::new(port, &settings.language_code(), 30);
        if client.health().await {
            return Ok(client);
        }
        if !model_downloader::is_installed(&settings.dictation_model) {
            return Err(format!(
                "Brak modelu {} — pobierz go w oknie Dyktowanie.",
                settings.dictation_model
            ));
        }
        let config = whisper_server::Config {
            model: settings.dictation_model.clone(),
            port,
            language: settings.language_code(),
            threads: std::thread::available_parallelism()
                .map(|n| n.get().min(8))
                .unwrap_or(4),
        };
        self.server
            .lock()
            .await
            .ensure_running(&config)
            .await
            .map_err(|e| e.to_string())?;
        self.inner.lock().unwrap().server_for = Some((config.model, port));
        Ok(client)
    }

    async fn transcribe(&self, pcm: &[f32]) -> Result<String, String> {
        let client = self.ensure_server().await?;
        if self.inner.lock().unwrap().server != ServerState::Ready {
            self.set_server(ServerState::Ready);
        }
        let vocabulary = self.settings().whisper_vocabulary;
        client
            .transcribe(pcm, &vocabulary, Quality::ACCURATE)
            .await
            .map_err(|e| e.to_string())
    }

    // MARK: - nagranie

    fn set_phase(self: &Arc<Self>, phase: Phase) -> u64 {
        let generation = {
            let mut inner = self.inner.lock().unwrap();
            inner.phase = phase.clone();
            inner.generation += 1;
            inner.generation
        };
        self.publish();
        if phase == Phase::Idle {
            pill::hide(&self.app);
        } else {
            pill::show(&self.app);
        }
        generation
    }

    /// Chowa pigułkę po chwili — chyba że w międzyczasie zaczęło się coś nowego.
    fn idle_after(self: &Arc<Self>, generation: u64, delay: Duration) {
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(delay).await;
            if this.inner.lock().unwrap().generation == generation {
                this.set_phase(Phase::Idle);
            }
        });
    }

    fn begin(self: &Arc<Self>) {
        {
            let inner = self.inner.lock().unwrap();
            if matches!(inner.phase, Phase::Recording | Phase::Transcribing) || inner.testing {
                return;
            }
        }
        self.samples.lock().unwrap().clear();
        let settings = self.settings();
        self.set_phase(Phase::Recording);
        if settings.dictation_mute_system {
            self.mute_output();
        }
        let this = self.clone();
        let samples = self.samples.clone();
        // Start strumienia czeka na wątek audio — nie na wątku zdarzeń.
        std::thread::spawn(move || {
            let errors = this.clone();
            let capture = Capture::start(
                AudioSource::Microphone,
                &settings.mic_device_id,
                now_ms(),
                move |chunk| samples.lock().unwrap().extend_from_slice(&chunk.samples),
                move |err| errors.fail(err),
            );
            match capture {
                Ok(capture) => {
                    let mut inner = this.inner.lock().unwrap();
                    // Skrót puszczony, zanim mikrofon wystartował — sprzątamy.
                    if inner.phase == Phase::Recording {
                        inner.capture = Some(capture);
                    }
                }
                Err(err) => this.fail(format!("Mikrofon: {err}")),
            }
        });
    }

    fn stop_capture(&self) {
        let capture = self.inner.lock().unwrap().capture.take();
        drop(capture);
    }

    fn cancel_recording(self: &Arc<Self>) {
        self.stop_capture();
        self.restore_output();
        self.set_phase(Phase::Idle);
    }

    fn finish(self: &Arc<Self>) {
        if self.inner.lock().unwrap().phase != Phase::Recording {
            return;
        }
        self.stop_capture();
        self.restore_output();
        let pcm = std::mem::take(&mut *self.samples.lock().unwrap());
        if pcm.len() < MIN_SAMPLES {
            self.set_phase(Phase::Idle);
            return;
        }
        self.set_phase(Phase::Transcribing);
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            match this.transcribe(&pcm).await {
                Ok(text) if text.trim().is_empty() => {
                    this.set_phase(Phase::Idle);
                }
                Ok(text) => {
                    let pasted = paste(&this.app, &text).await;
                    let generation = this.set_phase(if pasted {
                        Phase::Pasted
                    } else {
                        Phase::Error("Tekst jest w schowku — wklej go Ctrl+V".into())
                    });
                    let delay = if pasted { 900 } else { 3000 };
                    this.idle_after(generation, Duration::from_millis(delay));
                }
                Err(err) => this.fail(err),
            }
        });
    }

    fn fail(self: &Arc<Self>, message: String) {
        self.stop_capture();
        self.restore_output();
        let generation = self.set_phase(Phase::Error(message));
        self.idle_after(generation, Duration::from_secs(3));
    }

    // MARK: - próba

    /// 3 s z mikrofonu → tekst w oknie dyktowania, bez wklejania. Sprawdza
    /// naraz mikrofon, czy cokolwiek słychać, silnik i model.
    pub fn run_test(self: &Arc<Self>) {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.testing || inner.phase != Phase::Idle {
                return;
            }
            inner.testing = true;
            inner.test_result = None;
        }
        self.publish();
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            let result = this.test().await;
            {
                let mut inner = this.inner.lock().unwrap();
                inner.testing = false;
                inner.test_result = Some(result);
            }
            this.publish();
        });
    }

    async fn test(self: &Arc<Self>) -> String {
        let settings = self.settings();
        let samples: Arc<Mutex<Vec<f32>>> = Arc::default();
        let sink = samples.clone();
        let capture = Capture::start(
            AudioSource::Microphone,
            &settings.mic_device_id,
            now_ms(),
            move |chunk| sink.lock().unwrap().extend_from_slice(&chunk.samples),
            |_| {},
        );
        let capture = match capture {
            Ok(capture) => capture,
            Err(err) => return format!("✗ Mikrofon: {err}"),
        };
        if settings.dictation_mute_system {
            self.mute_output();
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(capture);
        self.restore_output();
        let pcm = std::mem::take(&mut *samples.lock().unwrap());
        let peak = pcm.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        if pcm.len() < MIN_SAMPLES || peak < 0.01 {
            return "✗ Mikrofon nic nie słyszy. Sprawdź: Ustawienia Windows → Prywatność → Mikrofon → „Zezwalaj aplikacjom klasycznym”, i wybrany mikrofon.".into();
        }
        match self.transcribe(&pcm).await {
            Ok(text) if text.trim().is_empty() => {
                "✓ Mikrofon słyszy, ale nic nie rozpoznano. Powiedz coś dłuższego.".into()
            }
            Ok(text) => format!("✓ {text}"),
            Err(err) => format!("✗ {err}"),
        }
    }

    // MARK: - gotowość

    pub fn checks(&self) -> Vec<Check> {
        let settings = self.settings();
        let inner_server = self.inner.lock().unwrap().server.clone();
        let mut checks = Vec::new();
        let mics = crate::audio::input_devices();
        checks.push(Check {
            id: "mic",
            title: "Mikrofon".into(),
            ok: !mics.is_empty(),
            detail: if mics.is_empty() {
                "Nie wykryto żadnego mikrofonu.".into()
            } else {
                "Wykryty. Czy coś słyszy, pokaże próba poniżej.".into()
            },
            action: None,
            action_label: None,
        });
        let engine = self
            .server
            .try_lock()
            .map(|s| s.binary_available())
            .unwrap_or(true);
        checks.push(Check {
            id: "engine",
            title: "Silnik mowy (whisper.cpp)".into(),
            ok: engine,
            detail: if engine {
                "Wbudowany.".into()
            } else {
                "Brak w instalacji — zainstaluj aplikację ponownie.".into()
            },
            action: None,
            action_label: None,
        });
        let model = settings.dictation_model.clone();
        let installed = model_downloader::is_installed(&model);
        let size = model_downloader::KNOWN
            .iter()
            .find(|m| m.id == model)
            .map(|m| m.bytes / 1_000_000)
            .unwrap_or(0);
        checks.push(Check {
            id: "model",
            title: format!("Model dyktowania ({model})"),
            ok: installed,
            detail: if installed {
                "Pobrany.".into()
            } else {
                format!("Do pobrania ({size} MB).")
            },
            action: (!installed).then(|| "download".into()),
            action_label: (!installed).then(|| "Pobierz".into()),
        });
        let (ok, detail) = match &inner_server {
            ServerState::Off if settings.dictation_enabled => (false, "Nie działa.".to_string()),
            ServerState::Off => (false, "Startuje po włączeniu dyktowania.".to_string()),
            ServerState::Starting(text) => (false, text.clone()),
            ServerState::Ready => (true, "Gotowy — model w pamięci, dyktowanie bez czekania.".into()),
            ServerState::Failed(err) => (false, err.clone()),
        };
        let idle = matches!(inner_server, ServerState::Off | ServerState::Failed(_));
        checks.push(Check {
            id: "server",
            title: "Serwer dyktowania".into(),
            ok,
            detail,
            action: (idle && installed).then(|| "prewarm".into()),
            action_label: (idle && installed).then(|| "Uruchom".into()),
        });
        checks.push(Check {
            id: "enabled",
            title: "Dyktowanie".into(),
            ok: settings.dictation_enabled,
            detail: if settings.dictation_enabled {
                format!("Włączone — skrót {}.", settings.dictation_hotkey)
            } else {
                "Wyłączone — włącz przełącznikiem na górze.".into()
            },
            action: None,
            action_label: None,
        });
        checks
    }

    // MARK: - wyciszanie

    fn mute_output(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.muted_output {
            return;
        }
        inner.muted_output = output::mute();
    }

    fn restore_output(&self) {
        let mut inner = self.inner.lock().unwrap();
        if std::mem::take(&mut inner.muted_output) {
            output::unmute();
        }
    }
}

fn dictation_port(settings: &Settings) -> u16 {
    if settings.dictation_model == settings.whisper_model {
        settings.whisper_port
    } else {
        settings.whisper_port + 1
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// Schowek + Ctrl+V, potem przywrócenie poprzedniej zawartości — chyba że
/// w międzyczasie ktoś skopiował coś nowego. `false` = tekst został tylko
/// w schowku.
async fn paste(app: &AppHandle, text: &str) -> bool {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    let clipboard = app.clipboard();
    let old_text = clipboard.read_text().ok();
    let old_image = if old_text.is_none() {
        clipboard.read_image().ok().map(|i| i.to_owned())
    } else {
        None
    };
    if clipboard.write_text(text.to_string()).is_err() {
        return false;
    }
    let ours = input::clipboard_sequence();
    // Skrót mógł zostawić wciśnięty Alt albo Shift (Ctrl+Shift+D) —
    // z nimi Ctrl+V byłoby innym skrótem.
    input::wait_for_modifiers_up(Duration::from_millis(800)).await;
    let sent = input::send_ctrl_v();

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(RESTORE_DELAY).await;
        if input::clipboard_sequence() != ours {
            return;
        }
        let clipboard = app.clipboard();
        if let Some(old) = old_text {
            let _ = clipboard.write_text(old);
        } else if let Some(image) = old_image {
            let _ = clipboard.write_image(&image);
        }
    });
    sent
}

/// Pigułka u dołu ekranu: „Słucham…" / „Przepisuję…" / „Wklejono".
/// `focusable(false)` = WS_EX_NOACTIVATE: klik w ikonę głośnika nie zabiera
/// fokusu, więc wklejanie trafia tam, gdzie był kursor.
mod pill {
    use super::*;

    const WIDTH: f64 = 360.0;
    const HEIGHT: f64 = 48.0;

    pub fn show(app: &AppHandle) {
        if let Some(window) = app.get_webview_window(PILL) {
            let _ = window.show();
            return;
        }
        let (x, y) = app
            .primary_monitor()
            .ok()
            .flatten()
            .map(|m| {
                let size = m.size().to_logical::<f64>(m.scale_factor());
                ((size.width - WIDTH) / 2.0, size.height - HEIGHT - 72.0)
            })
            .unwrap_or((400.0, 600.0));
        let _ = WebviewWindowBuilder::new(app, PILL, WebviewUrl::App("pill.html".into()))
            .title("brqs-whisper — dyktowanie")
            .inner_size(WIDTH, HEIGHT)
            .position(x, y)
            .always_on_top(true)
            .decorations(false)
            .transparent(true)
            .shadow(false)
            .resizable(false)
            .skip_taskbar(true)
            .focusable(false)
            .focused(false)
            .build();
    }

    pub fn hide(app: &AppHandle) {
        if let Some(window) = app.get_webview_window(PILL) {
            let _ = window.hide();
        }
    }
}

#[cfg(windows)]
mod input {
    use std::time::{Duration, Instant};
    use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

    pub fn clipboard_sequence() -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }

    fn down(vk: VIRTUAL_KEY) -> bool {
        unsafe { (GetAsyncKeyState(vk as i32) as u16) & 0x8000 != 0 }
    }

    pub async fn wait_for_modifiers_up(limit: Duration) {
        let started = Instant::now();
        while started.elapsed() < limit && [VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN].into_iter().any(down) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// Ctrl+V jako zdarzenie systemowe. Nie zadziała w oknie uruchomionym
    /// jako administrator (UIPI) — wtedy tekst zostaje w schowku.
    pub fn send_ctrl_v() -> bool {
        let v = b'V' as VIRTUAL_KEY;
        let inputs = [key(VK_CONTROL, false), key(v, false), key(v, true), key(VK_CONTROL, true)];
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        sent == inputs.len() as u32
    }
}

#[cfg(not(windows))]
mod input {
    use std::time::Duration;
    pub fn clipboard_sequence() -> u32 {
        0
    }
    pub async fn wait_for_modifiers_up(_limit: Duration) {}
    pub fn send_ctrl_v() -> bool {
        false
    }
}

/// Wyciszenie wyjścia przez `IAudioEndpointVolume` domyślnego głośnika.
/// Odciszamy tylko to, co sami wyciszyliśmy — dźwięk wyciszony wcześniej
/// przez użytkownika zostaje wyciszony. COM we własnym wątku (MTA), żeby nie
/// zależeć od modelu wątku, z którego przychodzi skrót.
#[cfg(windows)]
mod output {
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    fn with_endpoint<T: Send + 'static>(
        f: impl FnOnce(&IAudioEndpointVolume) -> windows::core::Result<T> + Send + 'static,
    ) -> Option<T> {
        std::thread::spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let result = (|| {
                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
                let volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None)?;
                f(&volume)
            })();
            CoUninitialize();
            result.ok()
        })
        .join()
        .ok()
        .flatten()
    }

    /// `true` = wyciszyliśmy my (było słychać).
    pub fn mute() -> bool {
        with_endpoint(|volume| unsafe {
            if volume.GetMute()?.as_bool() {
                return Ok(false);
            }
            volume.SetMute(true, std::ptr::null())?;
            Ok(true)
        })
        .unwrap_or(false)
    }

    pub fn unmute() {
        with_endpoint(|volume| unsafe { volume.SetMute(false, std::ptr::null()) });
    }
}

#[cfg(not(windows))]
mod output {
    pub fn mute() -> bool {
        false
    }
    pub fn unmute() {}
}

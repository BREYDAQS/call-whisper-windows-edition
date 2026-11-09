//! Spina wszystko w całość: dźwięk -> diaryzacja + ASR -> transkrypt ->
//! interfejs. Port `Recorder.swift`. Interfejs dostaje pełny stan zdarzeniem
//! `state` przy każdej zmianie.

use crate::assistant::{self, ClaudeBridge, Image};
use crate::audio::{AudioSource, Capture};
use crate::diarization::{self, WavFileWriter};
use crate::media_import;
use crate::model_downloader;
use crate::obs::{self, ObsLink};
use crate::pipeline::{self, Event, Pipeline};
use crate::settings::{AssistantBackend, Settings};
use crate::whisper_client::WhisperClient;
use crate::whisper_server::{self, WhisperServer};
use cw_core::assistant::{
    build_prompt, AssistantItem, AssistantSession, Found, QuestionWatcher, DEFAULT_CONTEXT_SEGMENTS,
};
use cw_core::markdown::{Markdown, MarkdownOptions, Session, SessionMeta};
use cw_core::speaker_turns::SpeakerTurns;
use cw_core::time::{now_ms, TimeFormat};
use cw_core::transcript_store::{Segment, TranscriptStore};
use cw_core::vocabulary::Vocabulary;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppState {
    pub segments: Vec<Segment>,
    pub answers: Vec<AssistantItem>,
    pub is_running: bool,
    pub is_processing: bool,
    pub status: String,
    pub last_error: Option<String>,
    pub speaker_count: usize,
    pub started_at: Option<f64>,
    pub title: Option<String>,
    pub overlay_visible: bool,
    pub assistant_label: String,
    pub meeting: Option<String>,
    pub update: Option<String>,
    pub readiness_ok: bool,
    pub file_base: String,
    pub media_extensions: Vec<&'static str>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub id: &'static str,
    pub title: String,
    pub ok: bool,
    pub detail: String,
    pub action: Option<String>,
    pub action_label: Option<String>,
}

struct Inner {
    store: TranscriptStore,
    session: AssistantSession,
    watcher: Option<QuestionWatcher>,
    /// Jedno pytanie automatyczne na raz: przy monologu wykrywacz trafia co
    /// kilka sekund, a odpowiedź trwa 1,5-5 s — bez tego kolejka rosła.
    asking: Option<JoinHandle<()>>,
    /// Najświeższe pytanie czekające, aż zwolni się miejsce.
    pending_auto: Option<Found>,
    bridge_started: bool,
    /// Sesja związana z nagraniem w OBS — przy stopie kończymy i jego.
    obs_session: bool,
    /// Dźwięk komputera tej sesji na dysku — dla rozpoznawania głosów po rozmowie.
    tape: Option<Arc<WavFileWriter>>,
    /// Metadane sesji z importu; `None` = rozmowa nagrana na żywo.
    imported_meta: Option<SessionMeta>,
    is_running: bool,
    is_processing: bool,
    status: String,
    last_error: Option<String>,
    started_at: Option<f64>,
    pipelines: Vec<Pipeline>,
    captures: Vec<Capture>,
    sources: Vec<AudioSource>,
    tasks: Vec<JoinHandle<()>>,
    processing: Option<JoinHandle<()>>,
    overlay_visible: bool,
    meeting: Option<String>,
    update: Option<String>,
}

pub struct Recorder {
    app: AppHandle,
    bridge: Arc<ClaudeBridge>,
    pub obs: Arc<ObsLink>,
    pub vendor: PathBuf,
    inner: Mutex<Inner>,
    pub settings: Mutex<Settings>,
    server: tokio::sync::Mutex<WhisperServer>,
}

pub const MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "m4a", "mp3", "wav", "aac", "flac", "ogg", "webm", "mkv", "m4v",
];

impl Recorder {
    pub fn new(
        app: AppHandle,
        vendor: PathBuf,
        obs_events: mpsc::UnboundedSender<obs::Event>,
    ) -> Arc<Self> {
        Arc::new(Self {
            app,
            bridge: ClaudeBridge::new(),
            obs: ObsLink::new(obs_events),
            server: tokio::sync::Mutex::new(WhisperServer::new(vendor.clone())),
            vendor,
            settings: Mutex::new(Settings::load()),
            inner: Mutex::new(Inner {
                store: TranscriptStore::new(now_ms()),
                session: AssistantSession::new(30),
                watcher: None,
                asking: None,
                pending_auto: None,
                bridge_started: false,
                obs_session: false,
                tape: None,
                imported_meta: None,
                is_running: false,
                is_processing: false,
                status: "Gotowy".into(),
                last_error: None,
                started_at: None,
                pipelines: Vec::new(),
                captures: Vec::new(),
                sources: Vec::new(),
                tasks: Vec::new(),
                processing: None,
                overlay_visible: false,
                meeting: None,
                update: None,
            }),
        })
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    pub fn is_busy(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.is_running || inner.is_processing
    }

    // --- stan dla interfejsu ---

    pub fn state(&self) -> AppState {
        let settings = self.settings();
        let inner = self.inner.lock().unwrap();
        let file_base = inner
            .imported_meta
            .as_ref()
            .map(|m| m.title.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "rozmowa-{}",
                    TimeFormat::filename_stamp(inner.started_at.unwrap_or_else(now_ms))
                )
            });
        AppState {
            segments: inner.store.segments(),
            answers: inner.session.all(),
            is_running: inner.is_running,
            is_processing: inner.is_processing,
            status: inner.status.clone(),
            last_error: inner.last_error.clone(),
            speaker_count: inner.pipelines.iter().map(|p| p.speaker_count()).sum(),
            started_at: inner.started_at,
            title: inner.imported_meta.as_ref().map(|m| m.title.clone()),
            overlay_visible: inner.overlay_visible,
            assistant_label: assistant_label(&settings),
            meeting: inner.meeting.clone(),
            update: inner.update.clone(),
            readiness_ok: self.readiness().iter().all(|c| c.ok),
            file_base,
            media_extensions: MEDIA_EXTENSIONS.to_vec(),
        }
    }

    pub fn publish(&self) {
        let _ = self.app.emit("state", self.state());
    }

    fn set_status(&self, status: impl Into<String>) {
        self.inner.lock().unwrap().status = status.into();
        self.publish();
    }

    fn set_error(&self, error: impl Into<String>) {
        self.inner.lock().unwrap().last_error = Some(error.into());
        self.publish();
    }

    pub fn set_update(&self, update: Option<String>) {
        self.inner.lock().unwrap().update = update;
        self.publish();
    }

    pub fn set_overlay_visible(&self, visible: bool) {
        self.inner.lock().unwrap().overlay_visible = visible;
        self.publish();
    }

    pub fn set_meeting(&self, meeting: Option<String>) {
        self.inner.lock().unwrap().meeting = meeting;
        self.publish();
    }

    // --- gotowość ---

    /// Czego aplikacja potrzebuje, żeby w ogóle zadziałać — i co z tego
    /// potrafi załatwić sama. Na Windows nie ma zgody na nagrywanie ekranu:
    /// loopback WASAPI jej nie wymaga.
    pub fn readiness(&self) -> Vec<Check> {
        let settings = self.settings();
        let mut checks = Vec::new();
        let has_engine = self
            .server
            .try_lock()
            .map(|s| s.binary_available())
            .unwrap_or(true);
        checks.push(Check {
            id: "whisper",
            title: "Silnik mowy (whisper.cpp)".into(),
            ok: has_engine,
            detail: if has_engine {
                "Wbudowany (GPU przez Vulkan, zapasowo CPU).".into()
            } else {
                "Brak w instalacji — zainstaluj aplikację ponownie.".into()
            },
            action: None,
            action_label: None,
        });
        let model = settings.whisper_model.clone();
        let has_model = model_downloader::is_installed(&model);
        let size = model_downloader::KNOWN
            .iter()
            .find(|m| m.id == model)
            .map(|m| m.bytes / 1_000_000)
            .unwrap_or(0);
        checks.push(Check {
            id: "model",
            title: format!("Model mowy ({model})"),
            ok: has_model,
            detail: if has_model {
                "Pobrany.".into()
            } else {
                format!("Pobierze się sam przy pierwszym nasłuchu ({size} MB) albo teraz.")
            },
            action: (!has_model).then(|| "downloadModel".into()),
            action_label: (!has_model).then(|| "Pobierz".into()),
        });
        let mics = crate::audio::input_devices();
        checks.push(Check {
            id: "mic",
            title: "Mikrofon".into(),
            ok: !settings.use_microphone || !mics.is_empty(),
            detail: if mics.is_empty() {
                "Nie wykryto żadnego mikrofonu.".into()
            } else {
                format!("Dostępne: {}", mics.len())
            },
            action: None,
            action_label: None,
        });
        if settings.assistant_enabled && settings.assistant_backend == AssistantBackend::Codex {
            let found = crate::codex::locate_cli(&settings.codex_path);
            checks.push(Check {
                id: "codex",
                title: "Codex CLI (ChatGPT)".into(),
                ok: found.is_some(),
                detail: match &found {
                    Some(cli) => format!(
                        "Znaleziony: {}. Przy błędzie logowania: w terminalu `codex login`.",
                        cli.describe()
                    ),
                    None => crate::codex::MISSING.into(),
                },
                action: None,
                action_label: None,
            });
        }
        if settings.assistant_enabled && settings.assistant_backend == AssistantBackend::Api {
            checks.push(Check {
                id: "key",
                title: "Klucz API".into(),
                ok: !settings.api_key.is_empty(),
                detail: "Potrzebny do podpowiedzi przez API. Wpisz go w Ustawieniach.".into(),
                action: None,
                action_label: None,
            });
        }
        checks
    }

    pub async fn readiness_action(self: &Arc<Self>, action: &str) {
        if action == "downloadModel" {
            let model = self.settings().whisper_model;
            self.inner.lock().unwrap().is_processing = true;
            let _ = self.download_model(&model).await;
            self.inner.lock().unwrap().is_processing = false;
            self.set_status("Gotowy");
        }
    }

    async fn download_model(self: &Arc<Self>, model: &str) -> bool {
        self.set_status(format!("Pobieram model {model}…"));
        let this = self.clone();
        let name = model.to_string();
        let result = model_downloader::download(model, move |p| {
            this.set_status(format!(
                "Pobieram model {name} — {:.0}% ({:.0}/{:.0} MB)",
                p.fraction * 100.0,
                p.received_bytes as f64 / 1e6,
                p.total_bytes as f64 / 1e6
            ));
        })
        .await;
        match result {
            Ok(()) => true,
            Err(err) => {
                self.set_error(format!("Nie udało się pobrać modelu: {err}"));
                self.set_status("Gotowy");
                false
            }
        }
    }

    // --- cykl życia ---

    /// Podnosi `whisper-server`, jeśli trzeba. `false`, gdy się nie da.
    async fn prepare_whisper(self: &Arc<Self>) -> bool {
        let settings = self.settings();
        if !model_downloader::is_installed(&settings.whisper_model)
            && !self.download_model(&settings.whisper_model).await
        {
            return false;
        }
        let client = WhisperClient::new(settings.whisper_port, &settings.language_code(), 15);
        if client.health().await {
            self.set_status("whisper-server działa");
            return true;
        }
        if !settings.auto_start_whisper {
            self.set_error(
                "whisper-server nie działa. Włącz automatyczne uruchamianie w Ustawieniach.",
            );
            return false;
        }
        self.set_status(format!(
            "Uruchamiam whisper-server ({})…",
            settings.whisper_model
        ));
        let config = whisper_server::Config {
            model: settings.whisper_model.clone(),
            port: settings.whisper_port,
            language: settings.language_code(),
            threads: std::thread::available_parallelism()
                .map(|n| n.get().min(8))
                .unwrap_or(4),
        };
        match self.server.lock().await.ensure_running(&config).await {
            Ok(_) => true,
            Err(err) => {
                self.set_error(err.to_string());
                self.set_status("Gotowy");
                false
            }
        }
    }

    /// `origin` (ms epoki) to chwila, od której liczą się znaczniki czasu —
    /// start nagrania w OBS, żeby 00:01:05 w transkrypcie znaczyło 00:01:05
    /// w filmie. `Some` = OBS już nagrywa (włączony ręcznie).
    pub async fn start(self: &Arc<Self>, origin: Option<f64>) {
        let settings = self.settings();
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.is_running || inner.is_processing {
                return;
            }
            if !settings.use_system_audio && !settings.use_microphone {
                inner.last_error = Some("Wybierz źródło dźwięku: komputer albo mikrofon".into());
                drop(inner);
                self.publish();
                return;
            }
            inner.last_error = None;
            inner.imported_meta = None;
            inner.is_processing = true;
        }
        self.publish();

        let ready = self.prepare_whisper().await;
        if !ready {
            self.inner.lock().unwrap().is_processing = false;
            self.publish();
            return;
        }

        // Nagranie w OBS dopiero po przygotowaniu silnika, żeby film
        // i transkrypt zaczynały się możliwie razem.
        let mut obs_session = origin.is_some();
        let mut origin = origin;
        let mut obs_note = None;
        if origin.is_none() && settings.follow_obs {
            self.set_status("Włączam nagrywanie w OBS…");
            match self.obs.start_recording().await {
                Ok(at) => {
                    origin = Some(at);
                    obs_session = true;
                }
                Err(err) => obs_note = Some(err),
            }
        }

        let now = origin.unwrap_or_else(now_ms);
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let mut sources = Vec::new();
        if settings.use_system_audio {
            sources.push(AudioSource::System);
        }
        if settings.use_microphone {
            sources.push(AudioSource::Microphone);
        }
        let project_context = settings.project_context();
        let vocabulary = Vocabulary::merge(
            &settings.whisper_vocabulary,
            &project_context,
            Vocabulary::MAX_CHARS,
        );

        // Zapis na dysk tylko wtedy, gdy będzie z czego korzystać.
        let tape = (settings.diarize_after && sources.contains(&AudioSource::System))
            .then(|| {
                let name = format!("call-whisper-{}.wav", TimeFormat::filename_stamp(now));
                WavFileWriter::create(std::env::temp_dir().join(name))
                    .ok()
                    .map(Arc::new)
            })
            .flatten();

        let mut pipelines = Vec::new();
        let mut captures = Vec::new();
        let mut failure = None;
        for &source in &sources {
            let pipeline = Pipeline::start(
                pipeline::Config {
                    source,
                    whisper_port: settings.whisper_port,
                    language_code: settings.language_code(),
                    identify_speakers: settings.identify_speakers,
                    vocabulary: vocabulary.clone(),
                    live_preview: settings.live_preview,
                },
                events_tx.clone(),
            );
            let errors = events_tx.clone();
            let device = if source == AudioSource::Microphone {
                settings.mic_device_id.as_str()
            } else {
                ""
            };
            let send = pipeline.sender();
            let tee = tape.clone().filter(|_| source == AudioSource::System);
            let on_chunk = move |chunk: crate::audio::PcmChunk| {
                if let Some(tape) = &tee {
                    tape.append(&chunk.samples, chunk.start_ms);
                }
                send(chunk);
            };
            match Capture::start(source, device, now, on_chunk, move |err| {
                let _ = errors.send(Event::Error(format!("Przechwytywanie przerwane: {err}")));
            }) {
                Ok(capture) => captures.push(capture),
                Err(err) => {
                    failure = Some(err.to_string());
                    pipelines.push(pipeline);
                    break;
                }
            }
            pipelines.push(pipeline);
        }

        if let Some(err) = failure {
            for p in pipelines {
                p.cancel();
            }
            let mut inner = self.inner.lock().unwrap();
            inner.is_processing = false;
            inner.last_error = Some(err);
            inner.status = "Gotowy".into();
            drop(inner);
            self.publish();
            return;
        }

        let heard = sources
            .iter()
            .map(|s| {
                if *s == AudioSource::System {
                    "system"
                } else {
                    "mikrofon"
                }
            })
            .collect::<Vec<_>>()
            .join(" + ");
        {
            let mut inner = self.inner.lock().unwrap();
            inner.store = TranscriptStore::new(now);
            inner.session = AssistantSession::new(30);
            inner.watcher = Some(QuestionWatcher::new(
                settings.min_confidence,
                500,
                Box::new(|_| {}),
            ));
            inner.pending_auto = None;
            inner.started_at = Some(now);
            inner.pipelines = pipelines;
            inner.captures = captures;
            inner.sources = sources;
            inner.is_running = true;
            inner.is_processing = false;
            inner.status = format!(
                "Słucham: {heard} · whisper {} · {}",
                settings.whisper_model,
                settings.language_code()
            );
            if obs_session {
                inner.status.push_str(" · OBS nagrywa");
            }
            inner.obs_session = obs_session;
            inner.tape = tape;
            inner.last_error = obs_note;
            inner.tasks.push(self.spawn_events(events_rx));
            inner.tasks.push(self.spawn_ticker());
        }
        self.publish();
        crate::windows::sync_topbar(&self.app, settings.top_bar);
        self.prepare_assistant().await;
    }

    /// Podnosi most i rozgrzewa go w tle przy starcie nasłuchu: zimny start
    /// CLI to ~3,9 s do pierwszego tokenu, ciepły ~1,8 s.
    async fn prepare_assistant(self: &Arc<Self>) {
        let settings = self.settings();
        // Codex nie ma czego rozgrzewać (proces na pytanie), ale brak CLI
        // lepiej zgłosić przy starcie niż przy pierwszym pytaniu.
        if settings.assistant_enabled
            && settings.assistant_backend == AssistantBackend::Codex
            && crate::codex::locate_cli(&settings.codex_path).is_none()
        {
            self.set_error(crate::codex::MISSING);
            return;
        }
        if !settings.assistant_enabled || settings.assistant_backend != AssistantBackend::ClaudeCode
        {
            return;
        }
        match self.bridge.start(&settings.claude_model).await {
            Ok(()) => {
                self.inner.lock().unwrap().bridge_started = true;
                self.bridge.warmup();
            }
            Err(err) => self.set_error(err),
        }
    }

    /// Aktualizacje z łańcuchów trafiają do `TranscriptStore`.
    fn spawn_events(self: &Arc<Self>, mut rx: mpsc::UnboundedReceiver<Event>) -> JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    Event::Update(update) => this.apply(update),
                    Event::Error(err) => {
                        let mut inner = this.inner.lock().unwrap();
                        // Ten sam błąd potrafi się powtórzyć co rundę — w interfejsie raz.
                        if inner.last_error.as_deref() != Some(err.as_str()) {
                            inner.last_error = Some(err);
                        }
                    }
                }
                this.publish();
            }
        })
    }

    /// Co 250 ms odświeżamy interfejs (liczba mówców, czasy).
    fn spawn_ticker(self: &Arc<Self>) -> JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            loop {
                interval.tick().await;
                let settings = this.settings();
                let found = if settings.assistant_enabled && settings.auto_ask {
                    let mut inner = this.inner.lock().unwrap();
                    let segments = inner.store.segments();
                    inner
                        .watcher
                        .as_mut()
                        .map(|w| w.scan(&segments))
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                for question in found {
                    this.handle_detected(question);
                }
                this.publish();
            }
        })
    }

    fn apply(&self, update: pipeline::Update) {
        let mut inner = self.inner.lock().unwrap();
        let now = now_ms();
        if update.discard {
            // Wypowiedź bez tekstu — usuwamy segment zamiast pustego wiersza.
            inner.store.discard(&update.key);
            inner.store.seal(&update.key, now);
            return;
        }
        // Cokolwiek dojechało, znaczy że łańcuch działa.
        inner.last_error = None;
        let at = inner.started_at.unwrap_or(now) + update.start_ms;
        // `replace: true`: whisper podaje pełną treść przy każdej rundzie.
        inner
            .store
            .upsert(&update.key, Some(&update.speaker), &update.text, at, true);
        if update.is_final {
            // Pieczętujemy klucz: ten sam wynik potrafi przyjść ponownie.
            inner.store.seal(&update.key, now);
        }
    }

    /// Najpierw gasimy stan widoczny w interfejsie, potem sprzątamy.
    /// `recording_path` podaje OBS, gdy to on zakończył nagranie — wtedy
    /// nie prosimy go o stop drugi raz.
    pub async fn stop(self: &Arc<Self>, recording_path: Option<String>) {
        let (pipelines, captures, tasks) = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.is_running && inner.pipelines.is_empty() {
                return;
            }
            inner.is_running = false;
            inner.status = "Zatrzymuję…".into();
            (
                std::mem::take(&mut inner.pipelines),
                std::mem::take(&mut inner.captures),
                std::mem::take(&mut inner.tasks),
            )
        };
        self.publish();
        drop(captures);
        for p in pipelines {
            p.cancel();
        }
        for t in tasks {
            t.abort();
        }
        // Odpowiedź na rozmowę, która się skończyła, nie jest nikomu potrzebna.
        let bridge_started = {
            let mut inner = self.inner.lock().unwrap();
            if let Some(task) = inner.asking.take() {
                task.abort();
            }
            inner.pending_auto = None;
            std::mem::take(&mut inner.bridge_started)
        };
        if bridge_started {
            self.bridge.stop().await;
        }
        // Zatrzymujemy tylko serwer, który sami podnieśliśmy.
        self.server.lock().await.stop();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.store.finalize_all(now_ms());
            inner.status = if inner.store.is_empty() {
                "Gotowy".into()
            } else {
                "Zatrzymane".into()
            };
        }
        self.publish();
        crate::windows::sync_topbar(&self.app, false);

        let tape = self.inner.lock().unwrap().tape.take();
        if let Some(tape) = tape {
            tape.finish();
            if self.segments().is_empty() {
                let _ = std::fs::remove_file(&tape.path);
            } else {
                self.diarize_session(tape.path.clone());
            }
        }

        let obs_session = std::mem::take(&mut self.inner.lock().unwrap().obs_session);
        let mut video = recording_path;
        if obs_session && video.is_none() {
            self.set_status("Kończę nagranie w OBS…");
            video = self.obs.stop_recording().await;
            self.set_status(if self.segments().is_empty() {
                "Gotowy"
            } else {
                "Zatrzymane"
            });
        }
        if let Some(video) = video.filter(|_| !self.segments().is_empty()) {
            self.save_next_to(&video);
        }
    }

    /// Rozpoznawanie głosów nagranej rozmowy. Na żywo zostają pewne etykiety
    /// „Ty" / „Rozmówcy"; dopiero po rozmowie, gdy klastrowanie widzi całe
    /// nagranie, „Rozmówcy" rozpadają się na „Rozmówca 1", „Rozmówca 2"…
    fn diarize_session(self: &Arc<Self>, wav: PathBuf) {
        let Some(engine) = diarization::Engine::locate(&self.vendor) else {
            let _ = std::fs::remove_file(&wav);
            self.set_error("Brak silnika rozpoznawania głosów w instalacji.");
            return;
        };
        self.inner.lock().unwrap().is_processing = true;
        self.set_status("Rozpoznaję głosy…");
        let this = self.clone();
        let task = tokio::spawn(async move {
            let path = wav.clone();
            let result = tokio::task::spawn_blocking(move || engine.run(&path, None)).await;
            let _ = std::fs::remove_file(&wav);
            {
                let mut inner = this.inner.lock().unwrap();
                match result {
                    Ok(Ok(turns)) => {
                        let segments = inner.store.segments();
                        let relabeled = SpeakerTurns::relabel(
                            &segments,
                            &turns,
                            SpeakerTurns::is_system_label,
                            "Rozmówca",
                        );
                        let voices: std::collections::HashSet<&str> = relabeled
                            .iter()
                            .map(|s| s.speaker.as_str())
                            .filter(|s| s.starts_with("Rozmówca "))
                            .collect();
                        let count = voices.len();
                        inner.store = rebuilt(inner.store.started_at(), &relabeled);
                        inner.status = if count > 1 {
                            format!("Zatrzymane · rozpoznano {count} głosy rozmówców")
                        } else {
                            "Zatrzymane · jeden głos po drugiej stronie".into()
                        };
                    }
                    Ok(Err(err)) => {
                        inner.last_error = Some(err.to_string());
                        inner.status = "Zatrzymane".into();
                    }
                    Err(_) => inner.status = "Zatrzymane".into(),
                }
                inner.is_processing = false;
            }
            this.publish();
        });
        self.inner.lock().unwrap().processing = Some(task);
    }

    /// Transkrypt obok pliku wideo z OBS, z tą samą nazwą i czasami
    /// względnymi — tylko te pokrywają się z osią filmu.
    fn save_next_to(&self, video: &str) {
        let path = cw_core::obs::Obs::transcript_path(video);
        match std::fs::write(&path, self.markdown(Some(false))) {
            Ok(()) => self.set_status(format!(
                "Zapisano obok nagrania: {}",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            )),
            Err(err) => self.set_error(format!(
                "Nie udało się zapisać transkryptu obok nagrania: {err}"
            )),
        }
    }

    /// OBS włączany i wyłączany z ustawień bez restartu.
    pub fn sync_obs(&self) {
        if self.settings().follow_obs {
            self.obs.start();
        } else {
            self.obs.stop();
        }
    }

    pub fn is_obs_session(&self) -> bool {
        self.inner.lock().unwrap().obs_session
    }

    /// Import nagrania albo wideo. Wynik ląduje w tym samym transkrypcie co
    /// rozmowa na żywo, więc eksport, kopiowanie i pytania działają bez zmian.
    pub fn import_file(self: &Arc<Self>, path: PathBuf) {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.is_running || inner.is_processing {
                return;
            }
            inner.is_processing = true;
            inner.last_error = None;
        }
        self.publish();
        let this = self.clone();
        let task = tokio::spawn(async move {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if this.prepare_whisper().await {
                let settings = this.settings();
                let ffmpeg = this.ffmpeg();
                let progress = this.clone();
                let result = media_import::run(
                    &ffmpeg,
                    &path,
                    settings.whisper_port,
                    &settings.language_code(),
                    settings
                        .diarize_after
                        .then(|| diarization::Engine::locate(&this.vendor))
                        .flatten(),
                    move |m| progress.set_status(m),
                )
                .await;
                {
                    let mut inner = this.inner.lock().unwrap();
                    match result {
                        Ok(result) => {
                            let count = result.segments.len();
                            let started = result.meta.started_at.unwrap_or_else(now_ms);
                            inner.store = rebuilt(started, &result.segments);
                            inner.session = AssistantSession::new(30);
                            inner.started_at = Some(started);
                            inner.imported_meta = Some(result.meta);
                            inner.status = if count == 0 {
                                format!("W {name} nie rozpoznano mowy")
                            } else {
                                format!("Zaimportowano {name} · {count} wypowiedzi")
                            };
                            inner.last_error = result
                                .diarization_note
                                .map(|n| format!("Bez podziału na głosy: {n}"));
                        }
                        Err(err) => {
                            inner.last_error = Some(err.to_string());
                            inner.status = "Gotowy".into();
                        }
                    }
                }
                this.server.lock().await.stop();
            }
            this.inner.lock().unwrap().is_processing = false;
            this.publish();
        });
        self.inner.lock().unwrap().processing = Some(task);
    }

    /// ffmpeg z instalatora; zapasowo z PATH (budowanie lokalne).
    fn ffmpeg(&self) -> PathBuf {
        let bundled = self.vendor.join("ffmpeg").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if bundled.is_file() {
            bundled
        } else {
            PathBuf::from("ffmpeg")
        }
    }

    pub fn cancel_processing(&self) {
        if let Some(task) = self.inner.lock().unwrap().processing.take() {
            task.abort();
        }
        let mut inner = self.inner.lock().unwrap();
        inner.is_processing = false;
        inner.status = "Przerwano".into();
        drop(inner);
        self.publish();
    }

    // --- asystent ---

    /// Pytanie wykryte w transkrypcji.
    fn handle_detected(self: &Arc<Self>, found: Found) {
        // Własne pytania nie są do podpowiadania: sufler odpowiadałby sam sobie.
        if found.speaker == "Ty" {
            return;
        }
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.asking.as_ref().is_some_and(|t| !t.is_finished()) {
                // Nowsze wygrywa: odpowiedź na to, co padło przed chwilą, jest
                // warta więcej niż na to sprzed kilkunastu sekund.
                inner.pending_auto = Some(found);
                return;
            }
        }
        self.ask(&found.question, Some(found.speaker.clone()), true, None);
    }

    /// Po tylu ms pytanie przestaje być warte odpowiedzi.
    const AUTO_QUESTION_TTL: f64 = 20_000.0;

    fn start_pending(self: &Arc<Self>) {
        let pending = {
            let mut inner = self.inner.lock().unwrap();
            if inner.asking.as_ref().is_some_and(|t| !t.is_finished()) {
                return;
            }
            inner.pending_auto.take()
        };
        if let Some(found) = pending.filter(|f| now_ms() - f.at < Self::AUTO_QUESTION_TTL) {
            self.ask(&found.question, Some(found.speaker.clone()), true, None);
        }
    }

    /// Pytanie: wykryte albo zadane ręcznie (z obrazkiem lub bez). Ręczne
    /// omija wykrywanie, ale dostaje ten sam kontekst z transkryptu.
    pub fn ask(
        self: &Arc<Self>,
        question: &str,
        speaker: Option<String>,
        auto: bool,
        image: Option<Image>,
    ) {
        let settings = self.settings();
        if !settings.assistant_enabled {
            return;
        }
        // Z obrazkiem samo pytanie może być puste — reszta jest na zrzucie.
        let asked = if question.is_empty() && image.is_some() {
            "Co widzisz na tym obrazku?"
        } else {
            question
        };
        let segments = self.segments();
        let Some(prompt) = build_prompt(
            asked,
            &segments,
            &settings.title,
            &settings.project_context(),
            DEFAULT_CONTEXT_SEGMENTS,
        ) else {
            return;
        };
        let backend = settings.assistant_backend;
        if backend == AssistantBackend::Api && settings.api_key.is_empty() {
            self.set_error("Brak klucza API. Ustawienia → Podpowiedzi → Klucz API.");
            return;
        }
        let item = {
            let mut inner = self.inner.lock().unwrap();
            inner
                .session
                .add(asked, speaker.as_deref(), now_ms(), auto, image.is_some())
        };
        self.publish();

        let this = self.clone();
        let id = item.id.clone();
        let task = tokio::spawn(async move {
            let started = std::time::Instant::now();
            let result = match backend {
                AssistantBackend::ClaudeCode => {
                    if !this.inner.lock().unwrap().bridge_started {
                        if let Err(err) = this.bridge.start(&settings.claude_model).await {
                            this.fail_answer(&id, err);
                            this.start_pending();
                            return;
                        }
                        this.inner.lock().unwrap().bridge_started = true;
                    }
                    // Ze zrzutem ekranu model potrzebuje więcej czasu.
                    let timeout = Duration::from_secs(if image.is_some() { 120 } else { 60 });
                    let sink = this.clone();
                    let sink_id = id.clone();
                    this.bridge
                        .ask(&prompt, image.as_ref(), timeout, move |delta| {
                            let ttft = started.elapsed().as_secs_f64() * 1000.0;
                            sink.inner
                                .lock()
                                .unwrap()
                                .session
                                .append(&sink_id, delta, Some(ttft));
                            sink.publish();
                        })
                        .await
                        .map(|_| ())
                }
                AssistantBackend::Codex => {
                    // Codex startuje proces na każde pytanie i bywa wolniejszy.
                    let timeout = Duration::from_secs(if image.is_some() { 150 } else { 90 });
                    let sink = this.clone();
                    let sink_id = id.clone();
                    crate::codex::ask(
                        &prompt,
                        &settings.codex_path,
                        &settings.codex_model,
                        &settings.codex_effort,
                        image.as_ref(),
                        timeout,
                        move |delta| {
                            let ttft = started.elapsed().as_secs_f64() * 1000.0;
                            sink.inner
                                .lock()
                                .unwrap()
                                .session
                                .append(&sink_id, delta, Some(ttft));
                            sink.publish();
                        },
                    )
                    .await
                    .map(|_| ())
                }
                AssistantBackend::Api => {
                    let sink = this.clone();
                    let sink_id = id.clone();
                    assistant::ask_api(
                        &prompt,
                        &settings.api_key,
                        &settings.model_id,
                        image.as_ref(),
                        move |delta, ttft| {
                            sink.inner
                                .lock()
                                .unwrap()
                                .session
                                .append(&sink_id, delta, ttft);
                            sink.publish();
                        },
                    )
                    .await
                }
            };
            match result {
                Ok(()) => {
                    let ms = started.elapsed().as_secs_f64() * 1000.0;
                    this.inner
                        .lock()
                        .unwrap()
                        .session
                        .complete(&id, None, Some(ms));
                    this.publish();
                }
                Err(err) => this.fail_answer(&id, err),
            }
            this.start_pending();
        });
        if auto {
            self.inner.lock().unwrap().asking = Some(task);
        }
    }

    fn fail_answer(&self, id: &str, error: String) {
        self.inner.lock().unwrap().session.fail(id, &error);
        self.publish();
    }

    /// Notatka z podsumowaniem do wklejenia w Claude. Trwa kilka sekund,
    /// bo model pisze podsumowanie.
    pub async fn claude_note(self: &Arc<Self>) -> Result<String, String> {
        let settings = self.settings();
        let transcript = self.markdown(None);
        let segments = self.segments();
        let options = MarkdownOptions {
            frontmatter: false,
            stats: false,
            ..MarkdownOptions::default()
        };
        let body = Markdown::render(
            &Session {
                meta: SessionMeta::default(),
                segments,
            },
            &options,
        );
        let prompt = format!(
            "Poniżej jest transkrypt rozmowy z oznaczeniem osób i znacznikami czasu [HH:MM:SS].\n\
             Napisz zwięzłe podsumowanie w Markdownie: 2-4 zdania ogólnie, potem krótka lista\n\
             najważniejszych punktów, decyzji i zadań, każdy ze znacznikiem [HH:MM:SS].\n\
             Odpowiedz w języku transkryptu. Wypisz tylko podsumowanie, bez wstępu.\n\n{}",
            body.chars().take(12_000).collect::<String>()
        );
        let summary = match settings.assistant_backend {
            AssistantBackend::ClaudeCode => {
                if !self.inner.lock().unwrap().bridge_started {
                    self.bridge.start(&settings.claude_model).await?;
                    self.inner.lock().unwrap().bridge_started = true;
                }
                self.bridge
                    .ask(&prompt, None, Duration::from_secs(180), |_| {})
                    .await?
            }
            AssistantBackend::Codex => {
                crate::codex::ask(
                    &prompt,
                    &settings.codex_path,
                    &settings.codex_model,
                    &settings.codex_effort,
                    None,
                    Duration::from_secs(180),
                    |_| {},
                )
                .await?
            }
            AssistantBackend::Api => {
                let mut text = String::new();
                assistant::ask_api(
                    &prompt,
                    &settings.api_key,
                    &settings.model_id,
                    None,
                    |d, _| text.push_str(d),
                )
                .await?;
                text
            }
        };
        // Podsumowanie pod nagłówkiem dokumentu, przed tabelą osób.
        let heading = format!("## Podsumowanie\n\n{}\n\n", summary.trim());
        Ok(match transcript.find("\n## ") {
            Some(at) => format!(
                "{}\n{}{}",
                &transcript[..at],
                &heading[..heading.len() - 1],
                &transcript[at..]
            ),
            None => heading + &transcript,
        })
    }

    // --- eksport ---

    pub fn segments(&self) -> Vec<Segment> {
        self.inner.lock().unwrap().store.segments()
    }

    pub fn chat_text(&self, ids: Option<Vec<String>>) -> String {
        let segments = self.segments();
        let chosen: Vec<Segment> = match ids {
            Some(ids) => segments
                .into_iter()
                .filter(|s| ids.contains(&s.id))
                .collect(),
            None => segments,
        };
        Markdown::chat_text(&chosen)
    }

    pub fn markdown(&self, absolute_timestamps: Option<bool>) -> String {
        let settings = self.settings();
        let inner = self.inner.lock().unwrap();
        let segments = inner.store.segments();
        let source = match inner.sources.as_slice() {
            [AudioSource::Microphone] => "microphone",
            [_, _] => "mixed",
            _ => "system-audio",
        };
        let meta = inner.imported_meta.clone().unwrap_or(SessionMeta {
            title: settings.title.clone(),
            source: source.into(),
            url: String::new(),
            started_at: inner.started_at,
            ended_at: segments.last().map(|s| s.ended_at),
        });
        let options = MarkdownOptions {
            locale: settings.markdown_locale.clone(),
            absolute_timestamps: absolute_timestamps.unwrap_or(settings.absolute_timestamps),
            ..MarkdownOptions::default()
        };
        Markdown::render(&Session { meta, segments }, &options)
    }

    pub fn json(&self) -> String {
        let inner = self.inner.lock().unwrap();
        let payload = serde_json::json!({
            "startedAt": inner.started_at.unwrap_or_else(now_ms),
            "segments": inner.store.segments(),
        });
        serde_json::to_string_pretty(&payload).unwrap_or_default()
    }
}

/// Skąd faktycznie idą podpowiedzi.
pub fn assistant_label(settings: &Settings) -> String {
    if !settings.assistant_enabled {
        return "wyłączone".into();
    }
    match settings.assistant_backend {
        AssistantBackend::ClaudeCode => {
            let model = if settings.claude_model.is_empty() {
                "domyślny"
            } else {
                &settings.claude_model
            };
            format!("Claude Code · {model}")
        }
        AssistantBackend::Codex => {
            let model = if settings.codex_model.is_empty() {
                "domyślny"
            } else {
                &settings.codex_model
            };
            format!("Codex · {model}")
        }
        AssistantBackend::Api => {
            if settings.model_id.is_empty() {
                "API".into()
            } else {
                settings.model_id.clone()
            }
        }
    }
}

/// Magazyn z gotowych, zamkniętych segmentów (import, nowe etykiety mówców).
fn rebuilt(started_at: f64, segments: &[Segment]) -> TranscriptStore {
    let mut store = TranscriptStore::new(started_at);
    for segment in segments {
        store.upsert(
            &segment.id,
            Some(&segment.speaker),
            &segment.text,
            segment.started_at,
            true,
        );
        store.seal(&segment.id, segment.ended_at);
    }
    store.finalize_all(now_ms());
    store
}

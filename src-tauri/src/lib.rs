mod assistant;
mod audio;
mod codex;
mod diarization;
mod dictation;
mod media_import;
mod meetings;
mod model_downloader;
mod obs;
mod pipeline;
mod recorder;
mod settings;
mod updater;
mod whisper_client;
mod whisper_server;
mod windows;

use recorder::{Check, Recorder};
use serde::Serialize;
use settings::Settings;
use std::sync::Arc;
use tauri::{Emitter, Manager, State};

type Rec<'a> = State<'a, Arc<Recorder>>;

#[tauri::command]
fn get_state(rec: Rec) -> recorder::AppState {
    rec.state()
}

#[tauri::command]
fn get_settings(rec: Rec) -> Settings {
    rec.settings()
}

#[tauri::command]
async fn save_settings(
    app: tauri::AppHandle,
    rec: Rec<'_>,
    settings: Settings,
) -> Result<Settings, String> {
    settings.save().map_err(|e| e.to_string())?;
    *rec.settings.lock().unwrap() = settings.clone();
    rec.sync_obs();
    app.state::<Arc<dictation::Dictation>>().sync();
    let _ = app.emit("settings", &settings);
    rec.publish();
    Ok(settings)
}

#[derive(Serialize)]
struct Mics {
    devices: Vec<audio::InputDevice>,
    default: Option<audio::InputDevice>,
}

#[tauri::command]
fn list_mics() -> Mics {
    Mics {
        devices: audio::input_devices(),
        default: audio::default_input_device(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelInfo {
    id: &'static str,
    note: &'static str,
    installed: bool,
}

#[tauri::command]
fn models() -> Vec<ModelInfo> {
    model_downloader::KNOWN
        .iter()
        .map(|m| ModelInfo {
            id: m.id,
            note: m.note,
            installed: model_downloader::is_installed(m.id),
        })
        .collect()
}

#[tauri::command]
fn api_models() -> Vec<(&'static str, &'static str)> {
    assistant::API_MODELS.to_vec()
}

#[tauri::command]
async fn start(rec: Rec<'_>) -> Result<(), String> {
    rec.inner().start(None).await;
    Ok(())
}

#[tauri::command]
async fn stop(rec: Rec<'_>) -> Result<(), String> {
    rec.inner().stop(None).await;
    Ok(())
}

#[tauri::command]
fn cancel_processing(rec: Rec) {
    rec.cancel_processing();
}

#[tauri::command]
fn chat_text(rec: Rec, ids: Option<Vec<String>>) -> String {
    rec.chat_text(ids)
}

#[tauri::command]
fn markdown(rec: Rec) -> String {
    rec.markdown(None)
}

#[tauri::command]
fn json_export(rec: Rec) -> String {
    rec.json()
}

#[tauri::command]
fn write_file(path: String, contents: String) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|e| e.to_string())
}

#[tauri::command]
fn readiness(rec: Rec) -> Vec<Check> {
    rec.readiness()
}

#[tauri::command]
async fn readiness_action(rec: Rec<'_>, action: String) -> Result<(), String> {
    rec.inner().readiness_action(&action).await;
    Ok(())
}

#[tauri::command]
async fn check_update(app: tauri::AppHandle, force: bool) -> Result<(), String> {
    updater::check(&app, force).await;
    Ok(())
}

// --- kolejne etapy ---

#[tauri::command]
async fn claude_note(rec: Rec<'_>) -> Result<String, String> {
    rec.inner().claude_note().await
}

#[tauri::command]
async fn import_file(rec: Rec<'_>, path: String) -> Result<(), String> {
    rec.inner().import_file(path.into());
    Ok(())
}

/// `image`: PNG w base64, już przeskalowany w interfejsie.
#[tauri::command]
async fn ask(rec: Rec<'_>, question: String, image: Option<String>) -> Result<(), String> {
    let image = image.map(|base64| assistant::Image {
        base64,
        media_type: "image/png".into(),
    });
    rec.inner().ask(&question, None, false, image);
    Ok(())
}

#[derive(Serialize)]
struct ClipboardImage {
    base64: String,
    size: String,
}

/// Zapasowa droga do zrzutu ze schowka, gdy wklejenie Ctrl+V nie trafi
/// w pole pytania.
#[tauri::command]
fn clipboard_image(app: tauri::AppHandle) -> Option<ClipboardImage> {
    use base64::Engine;
    use tauri_plugin_clipboard_manager::ClipboardExt;
    let image = app.clipboard().read_image().ok()?;
    let (width, height) = (image.width(), image.height());
    let mut png_data = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_data, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .ok()?
            .write_image_data(image.rgba())
            .ok()?;
    }
    Some(ClipboardImage {
        base64: base64::engine::general_purpose::STANDARD.encode(&png_data),
        size: format!("{width}×{height}"),
    })
}

#[tauri::command]
async fn toggle_overlay(app: tauri::AppHandle, rec: Rec<'_>) -> Result<bool, String> {
    let visible = windows::toggle_overlay(&app).map_err(|e| e.to_string())?;
    rec.set_overlay_visible(visible);
    Ok(visible)
}

// --- dyktowanie ---

type Dict<'a> = State<'a, Arc<dictation::Dictation>>;

#[tauri::command]
fn dictation_state(d: Dict) -> dictation::View {
    d.view()
}

#[tauri::command]
fn dictation_checks(d: Dict) -> Vec<Check> {
    d.checks()
}

#[tauri::command]
async fn dictation_action(d: Dict<'_>, action: String) -> Result<(), String> {
    match action.as_str() {
        "download" => d.inner().download_model().await,
        "prewarm" => d.inner().prewarm(),
        _ => {}
    }
    Ok(())
}

#[tauri::command]
fn dictation_capture(d: Dict, on: bool) {
    d.inner().set_capturing(on);
}

#[tauri::command]
fn dictation_set_hotkey(d: Dict, hotkey: String) -> Result<(), String> {
    d.inner().set_hotkey(hotkey)
}

#[tauri::command]
fn dictation_toggle_mute(d: Dict) {
    d.inner().toggle_mute();
}

#[tauri::command]
fn dictation_test(d: Dict) {
    d.inner().run_test();
}

#[tauri::command]
fn place_topbar(app: tauri::AppHandle, width: f64, height: f64) {
    windows::place_topbar(&app, width, height);
}

/// Nagranie włączone ręcznie w OBS włącza nasłuch; zatrzymane w OBS kończy
/// go, a transkrypt ląduje obok pliku wideo.
async fn follow_obs(
    rec: Arc<Recorder>,
    mut events: tokio::sync::mpsc::UnboundedReceiver<obs::Event>,
) {
    while let Some(event) = events.recv().await {
        match event {
            obs::Event::RecordStarted(origin) => {
                // „Sam dźwięk": OBS nagrywa po swojemu, nasłuch się nie wtrąca.
                if rec.settings().follow_obs && !rec.is_busy() {
                    rec.start(Some(origin)).await;
                }
            }
            obs::Event::RecordStopped(path) => {
                if rec.state().is_running && rec.is_obs_session() {
                    rec.stop(path).await;
                }
            }
        }
    }
}

/// Co 2 s: czy trwa rozmowa. Samo powiadomienie, chyba że w ustawieniach
/// włączono samodzielny start — wtedy nasłuch zaczyna się i kończy sam,
/// ale zatrzymujemy tylko nasłuch, który sami włączyliśmy.
fn spawn_meeting_watcher(app: tauri::AppHandle) {
    use cw_core::meeting_detection::MeetingChange;
    use tauri_plugin_notification::NotificationExt;
    tauri::async_runtime::spawn(async move {
        let mut watcher = meetings::MeetingWatcher::new();
        let mut auto_started = false;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let rec = app.state::<Arc<Recorder>>().inner().clone();
            let settings = rec.settings();
            if !settings.detect_meetings {
                continue;
            }
            // Odczyt rejestru trwa ułamek milisekundy — bez osobnego wątku.
            let Some(change) = watcher.poll() else {
                continue;
            };
            rec.set_meeting(watcher.active());
            match change {
                MeetingChange::Started(name) => {
                    if rec.is_busy() {
                        continue;
                    }
                    if settings.auto_start_on_meeting {
                        auto_started = true;
                        rec.start(None).await;
                        let _ = app
                            .notification()
                            .builder()
                            .title(format!("Słucham rozmowy w {name}"))
                            .body("Transkrypt zbiera się w brqs-whisper.")
                            .show();
                    } else {
                        let _ = app
                            .notification()
                            .builder()
                            .title(format!("Wykryto rozmowę w {name}"))
                            .body("Zapisać transkrypt? Kliknij „Słuchaj” w brqs-whisper.")
                            .show();
                    }
                }
                MeetingChange::Ended(_) => {
                    if std::mem::take(&mut auto_started) && rec.state().is_running {
                        rec.stop(None).await;
                    }
                }
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Druga kopia aplikacji ma tylko pokazać pierwszą: dwa nasłuchy naraz
        // biłyby się o port whispera.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            let vendor = app
                .path()
                .resource_dir()
                .map(|d| d.join("vendor"))
                .unwrap_or_else(|_| std::path::PathBuf::from("vendor"));
            let (obs_tx, obs_rx) = tokio::sync::mpsc::unbounded_channel();
            let rec = Recorder::new(app.handle().clone(), vendor.clone(), obs_tx);
            app.manage(rec.clone());
            let dictation = dictation::Dictation::new(app.handle().clone(), vendor);
            app.manage(dictation.clone());
            dictation.sync();
            tauri::async_runtime::spawn(async move {
                rec.sync_obs();
                follow_obs(rec, obs_rx).await;
            });
            updater::spawn(app.handle().clone());
            spawn_meeting_watcher(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_settings,
            save_settings,
            list_mics,
            models,
            api_models,
            start,
            stop,
            cancel_processing,
            chat_text,
            markdown,
            json_export,
            write_file,
            readiness,
            readiness_action,
            check_update,
            claude_note,
            import_file,
            ask,
            clipboard_image,
            toggle_overlay,
            place_topbar,
            dictation_state,
            dictation_checks,
            dictation_action,
            dictation_capture,
            dictation_set_hotkey,
            dictation_toggle_mute,
            dictation_test,
        ])
        .build(tauri::generate_context!())
        .expect("błąd uruchamiania brqs-whisper")
        .run(|app, event| {
            // Wyjście w trakcie dyktowania nie może zostawić wyciszonego systemu.
            if let tauri::RunEvent::Exit = event {
                if let Some(d) = app.try_state::<Arc<dictation::Dictation>>() {
                    d.shutdown();
                }
            }
        });
}

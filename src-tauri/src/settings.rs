//! Ustawienia: te same klucze i wartości domyślne co `AppSettings` na macOS,
//! zapisane jako JSON w %APPDATA%\call-whisper\settings.json.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AssistantBackend {
    /// Lokalne CLI Claude Code: bez klucza API, w ramach subskrypcji.
    ClaudeCode,
    /// API zgodne z OpenAI (Experiential Labs). Szybsze, ale płatne.
    Api,
    /// Lokalne Codex CLI (OpenAI): bez klucza, w ramach subskrypcji ChatGPT.
    Codex,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub language: String,
    pub model_id: String,
    pub assistant_enabled: bool,
    pub auto_ask: bool,
    /// Domyślnie tylko dźwięk komputera. Mikrofon przy głośnikach łapie echo
    /// i każda wypowiedź trafia do transkryptu dwa razy.
    pub use_microphone: bool,
    pub use_system_audio: bool,
    /// Nazwa urządzenia wejściowego; pusta = domyślne systemu.
    pub mic_device_id: String,
    pub assistant_backend: AssistantBackend,
    pub claude_model: String,
    /// Model Codex; pusty = domyślny z konfiguracji Codex CLI.
    pub codex_model: String,
    /// `model_reasoning_effort` Codexa. Sufler ma odpowiadać w sekundy.
    pub codex_effort: String,
    pub markdown_locale: String,
    pub absolute_timestamps: bool,
    pub min_confidence: f64,
    pub title: String,
    pub project_context_path: String,
    pub whisper_model: String,
    pub whisper_port: u16,
    pub auto_start_whisper: bool,
    /// Wyłączone: klastrowanie MFCC przy podobnych głosach rozsypuje jedną
    /// osobę na kilka etykiet.
    pub identify_speakers: bool,
    pub diarize_after: bool,
    pub detect_meetings: bool,
    pub auto_start_on_meeting: bool,
    pub follow_obs: bool,
    pub whisper_vocabulary: String,
    /// Odpowiednik wyspy w notchu: pasek u góry ekranu.
    pub top_bar: bool,
    pub auto_update: bool,
    pub api_key: String,
    /// Tekst wypowiedzi w trakcie mówienia. Domyślnie nie: tekst po końcu
    /// wypowiedzi, za to kilka razy mniej pracy dla CPU/GPU.
    pub live_preview: bool,
    /// Dyktowanie skrótem do dowolnego pola (port z macOS). Domyślnie wył.
    pub dictation_enabled: bool,
    /// `push` = trzymaj skrót i mów; `toggle` = naciśnij: start / stop.
    pub dictation_mode: String,
    /// Akcelerator jak „Ctrl+J”, „Ctrl+Shift+D”, „F9”.
    pub dictation_hotkey: String,
    /// Osobny od rozmowy: krótkie wypowiedzi, więc stać nas na dokładniejszy.
    pub dictation_model: String,
    /// Wyciszanie dźwięku systemu na czas nagrywania.
    pub dictation_mute_system: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: "pl-PL".into(),
            model_id: String::new(),
            assistant_enabled: true,
            auto_ask: true,
            use_microphone: false,
            use_system_audio: true,
            mic_device_id: String::new(),
            assistant_backend: AssistantBackend::ClaudeCode,
            claude_model: "sonnet".into(),
            codex_model: String::new(),
            codex_effort: "low".into(),
            markdown_locale: "pl".into(),
            absolute_timestamps: false,
            min_confidence: 0.35,
            title: String::new(),
            project_context_path: String::new(),
            whisper_model: "small".into(),
            whisper_port: 8899,
            auto_start_whisper: true,
            identify_speakers: false,
            diarize_after: false,
            detect_meetings: true,
            auto_start_on_meeting: false,
            follow_obs: true,
            whisper_vocabulary: String::new(),
            top_bar: true,
            auto_update: true,
            api_key: String::new(),
            live_preview: false,
            dictation_enabled: false,
            dictation_mode: "push".into(),
            dictation_hotkey: "Ctrl+J".into(),
            dictation_model: "large-v3-turbo".into(),
            dictation_mute_system: false,
        }
    }
}

impl Settings {
    pub fn language_code(&self) -> String {
        self.language.chars().take(2).collect()
    }

    /// Plik z opisem projektu; pusta ścieżka = domyślny w %APPDATA%.
    pub fn project_context_file(&self) -> PathBuf {
        if self.project_context_path.is_empty() {
            config_dir().join("project-context.md")
        } else {
            PathBuf::from(&self.project_context_path)
        }
    }

    /// Treść kontekstu projektu (trafia do każdego pytania). Tylko pliki
    /// tekstowe i najwyżej 8000 znaków — tyle obiecuje interfejs.
    pub fn project_context(&self) -> String {
        let path = self.project_context_file();
        let text_like = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "md" | "txt" | "markdown"));
        if !text_like {
            return String::new();
        }
        std::fs::read_to_string(path)
            .map(|t| t.chars().take(8000).collect())
            .unwrap_or_default()
    }

    pub fn load() -> Self {
        std::fs::read(settings_path())
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = settings_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

/// %APPDATA%\call-whisper
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("call-whisper")
}

/// %LOCALAPPDATA%\call-whisper — duże pliki (modele, logi, nagrania).
pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("call-whisper")
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

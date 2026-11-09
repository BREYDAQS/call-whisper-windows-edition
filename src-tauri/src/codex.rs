//! Podpowiedzi przez lokalne Codex CLI (OpenAI): bez klucza API, w ramach
//! subskrypcji ChatGPT — odpowiednik mostu do Claude Code.
//!
//! Codex nie ma trybu ciągłej sesji przez stdin jak `claude -p
//! --input-format stream-json`, więc każde pytanie to jedno `codex exec`.
//! Prompt idzie przez stdin (`-`), żeby nowe linie i cudzysłowy nie
//! przechodziły przez cmd.exe. Odpowiedź bierzemy ze zdarzeń `--json`,
//! a zapasowo z pliku `--output-last-message` — format zdarzeń zmieniał się
//! między wersjami CLI, plik nie.

use crate::assistant::{Image, SYSTEM_PROMPT};
use crate::whisper_server::job;
#[cfg(windows)]
use crate::whisper_server::CREATE_NO_WINDOW;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Sufler odpowiada, nie pracuje na plikach. Codex to agent, więc mówimy
/// to wprost — sandbox i tak jest tylko do odczytu, w pustym katalogu.
const NO_TOOLS: &str = "Nie uruchamiaj poleceń, nie czytaj plików i nie używaj narzędzi. \
Odpowiedz od razu samym tekstem.";

/// Jak uruchomić `codex`: natywny `codex.exe` albo `node codex.js` z npm
/// (instalacja z npm daje `codex.cmd`, którego nie uruchamiamy przez cmd.exe).
#[derive(Clone, Debug)]
pub struct Cli {
    program: PathBuf,
    prefix: Vec<String>,
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

pub fn locate_cli() -> Option<Cli> {
    let exe = if cfg!(windows) { "codex.exe" } else { "codex" };
    let native = |program: PathBuf| Cli {
        program,
        prefix: Vec::new(),
    };
    if let Some(found) = on_path(exe) {
        return Some(native(found));
    }
    let home = dirs::home_dir().unwrap_or_default();
    let mut candidates = vec![
        home.join(".codex").join("bin").join(exe),
        home.join(".local").join("bin").join(exe),
    ];
    if let Some(local) = dirs::data_local_dir() {
        candidates.push(local.join("Programs").join("codex").join(exe));
        candidates.push(local.join("Microsoft").join("WinGet").join("Links").join(exe));
    }
    if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
        return Some(native(found));
    }
    // npm: %APPDATA%\npm\node_modules\@openai\codex — w środku natywna
    // binarka (vendor\<target>\codex\codex.exe) i skrypt startowy dla node.
    let package = dirs::data_dir()?
        .join("npm")
        .join("node_modules")
        .join("@openai")
        .join("codex");
    let vendored = ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"]
        .iter()
        .map(|t| package.join("vendor").join(t).join("codex").join(exe))
        .find(|p| p.is_file());
    if let Some(found) = vendored {
        return Some(native(found));
    }
    let script = package.join("bin").join("codex.js");
    if script.is_file() {
        let node = on_path(if cfg!(windows) { "node.exe" } else { "node" })?;
        return Some(Cli {
            program: node,
            prefix: vec![script.to_string_lossy().into_owned()],
        });
    }
    None
}

pub const MISSING: &str = "Nie znalazłem Codex CLI. Zainstaluj: w terminalu `npm i -g @openai/codex`, \
potem `codex login` (konto ChatGPT). Albo wybierz Claude Code lub API.";

/// Katalog roboczy Codexa: pusty, żeby agent nie miał czego czytać.
fn workdir() -> PathBuf {
    let dir = std::env::temp_dir().join("call-whisper-codex");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Tekst ze zdarzenia `--json`. Obsługuje nowy format (`item.completed`
/// z `agent_message`) i starszy (`msg.type == agent_message[_delta]`).
enum Event {
    Delta(String),
    Message(String),
    Error(String),
    Other,
}

fn parse(line: &str) -> Event {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Event::Other;
    };
    let text = |v: &Value| v.as_str().map(str::to_string);
    match v["type"].as_str() {
        Some("item.completed") if v["item"]["type"] == "agent_message" => {
            return text(&v["item"]["text"]).map(Event::Message).unwrap_or(Event::Other)
        }
        Some("turn.failed") => {
            return Event::Error(text(&v["error"]["message"]).unwrap_or_else(|| "Codex zwrócił błąd.".into()))
        }
        Some("error") => {
            return Event::Error(text(&v["message"]).unwrap_or_else(|| "Codex zwrócił błąd.".into()))
        }
        _ => {}
    }
    let msg = &v["msg"];
    match msg["type"].as_str() {
        Some("agent_message_delta") => text(&msg["delta"]).map(Event::Delta).unwrap_or(Event::Other),
        Some("agent_message") => text(&msg["message"]).map(Event::Message).unwrap_or(Event::Other),
        Some("error") => Event::Error(text(&msg["message"]).unwrap_or_else(|| "Codex zwrócił błąd.".into())),
        _ => Event::Other,
    }
}

/// Komunikat dla człowieka z błędu CLI — najczęściej brak logowania.
fn explain(stderr: &str, fallback: String) -> String {
    let lower = stderr.to_lowercase();
    if lower.contains("login") || lower.contains("not logged") || lower.contains("unauthorized") || lower.contains("401") {
        return "Codex nie jest zalogowany. W terminalu: `codex login` (konto ChatGPT).".into();
    }
    let tail: String = stderr.trim().lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" ");
    if tail.is_empty() {
        fallback
    } else {
        format!("{fallback} {tail}")
    }
}

/// Jedno pytanie. `on_delta` dostaje tekst, gdy tylko się pojawi — przy
/// nowszych wersjach CLI cała odpowiedź naraz.
pub async fn ask(
    prompt: &str,
    model: &str,
    effort: &str,
    image: Option<&Image>,
    timeout: Duration,
    mut on_delta: impl FnMut(&str),
) -> Result<String, String> {
    let cli = locate_cli().ok_or(MISSING)?;
    let dir = workdir();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let last = dir.join(format!("answer-{stamp}.txt"));
    let image_path = match image {
        Some(img) => {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&img.base64)
                .map_err(|e| e.to_string())?;
            let path = dir.join(format!("image-{stamp}.png"));
            std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
            Some(path)
        }
        None => None,
    };

    let mut command = Command::new(&cli.program);
    command
        .args(&cli.prefix)
        .arg("exec")
        .args(["--skip-git-repo-check", "--sandbox", "read-only", "--color", "never", "--json"])
        .arg("--output-last-message")
        .arg(&last);
    if !model.is_empty() {
        command.args(["--model", model]);
    }
    if !effort.is_empty() {
        // Sufler ma odpowiadać w sekundy — długie rozumowanie tu szkodzi.
        command.args(["-c", &format!("model_reasoning_effort=\"{effort}\"")]);
    }
    if let Some(path) = &image_path {
        command.arg("--image").arg(path);
    }
    command
        .arg("-")
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);

    let result = async {
        let mut child = command
            .spawn()
            .map_err(|e| format!("Nie udało się uruchomić codex: {e}"))?;
        if let Some(id) = child.id() {
            job::assign_pid(id);
        }
        let mut stdin = child.stdin.take().ok_or("brak stdin")?;
        let full = format!("{SYSTEM_PROMPT}\n\n{NO_TOOLS}\n\n---\n\n{prompt}");
        stdin.write_all(full.as_bytes()).await.map_err(|e| e.to_string())?;
        drop(stdin);

        let mut stderr = child.stderr.take().ok_or("brak stderr")?;
        let errors = tokio::spawn(async move {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text).await;
            text
        });
        let stdout = child.stdout.take().ok_or("brak stdout")?;
        let mut lines = BufReader::new(stdout).lines();
        let mut text = String::new();
        let mut failure = None;
        let read = async {
            while let Ok(Some(line)) = lines.next_line().await {
                match parse(&line) {
                    Event::Delta(delta) => {
                        text.push_str(&delta);
                        on_delta(&delta);
                    }
                    // Pełna wiadomość: to, czego jeszcze nie pokazaliśmy.
                    Event::Message(message) => {
                        if let Some(rest) = message.strip_prefix(text.as_str()) {
                            if !rest.is_empty() {
                                on_delta(rest);
                            }
                        } else if text.is_empty() {
                            on_delta(&message);
                        }
                        text = message;
                    }
                    Event::Error(err) => failure = Some(err),
                    Event::Other => {}
                }
            }
        };
        if tokio::time::timeout(timeout, read).await.is_err() {
            let _ = child.kill().await;
            return Err(format!("Codex nie odpowiedział w {} s.", timeout.as_secs()));
        }
        let status = child.wait().await.map_err(|e| e.to_string())?;
        let stderr = errors.await.unwrap_or_default();
        let written = std::fs::read_to_string(&last).unwrap_or_default();
        let streamed = !text.trim().is_empty();
        let answer = if written.trim().is_empty() { text } else { written };
        if !answer.trim().is_empty() && status.success() {
            // Zdarzenia nic nie pokazały (inny format CLI) — cała odpowiedź z pliku.
            if !streamed {
                on_delta(answer.trim());
            }
            return Ok(answer.trim().to_string());
        }
        let fallback = failure.unwrap_or_else(|| format!("Codex zakończył się (kod {}).", status.code().unwrap_or(-1)));
        Err(explain(&stderr, fallback))
    }
    .await;

    let _ = std::fs::remove_file(&last);
    if let Some(path) = image_path {
        let _ = std::fs::remove_file(path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_new_event_format() {
        let line = r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"Gotowe."}}"#;
        assert!(matches!(parse(line), Event::Message(t) if t == "Gotowe."));
        let failed = r#"{"type":"turn.failed","error":{"message":"rate limit"}}"#;
        assert!(matches!(parse(failed), Event::Error(t) if t == "rate limit"));
        let reasoning = r#"{"type":"item.completed","item":{"type":"reasoning","text":"myślę"}}"#;
        assert!(matches!(parse(reasoning), Event::Other));
    }

    #[test]
    fn parses_old_event_format() {
        let delta = r#"{"id":"0","msg":{"type":"agent_message_delta","delta":"Go"}}"#;
        assert!(matches!(parse(delta), Event::Delta(t) if t == "Go"));
        let full = r#"{"id":"0","msg":{"type":"agent_message","message":"Gotowe."}}"#;
        assert!(matches!(parse(full), Event::Message(t) if t == "Gotowe."));
    }

    #[test]
    fn explains_missing_login() {
        assert!(explain("Error: Not logged in. Run codex login", "x".into()).contains("codex login"));
    }
}

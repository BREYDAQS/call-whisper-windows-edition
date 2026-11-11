//! Renderer Markdown. Port z `Markdown.swift` (a ten z `extension/src/core/markdown.js`)
//! — ten sam format wyjściowy, więc pliki z wersji webowej, macOS i Windows są
//! nieodróżnialne.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::text::Text;
use crate::time::{now_ms, TimeFormat};
use crate::transcript_store::Segment;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionMeta {
    pub title: String,
    pub source: String,
    pub url: String,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub meta: SessionMeta,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MarkdownOptions {
    pub locale: String,
    pub frontmatter: bool,
    pub absolute_timestamps: bool,
    pub stats: bool,
    pub escape: bool,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self { locale: "pl".into(), frontmatter: true, absolute_timestamps: false, stats: true, escape: true }
    }
}

struct Strings {
    transcript: &'static str,
    participants: &'static str,
    person: &'static str,
    utterances: &'static str,
    words: &'static str,
    share: &'static str,
    untitled: &'static str,
    empty: &'static str,
    live: &'static str,
}

const PL: Strings = Strings {
    transcript: "Transkrypt",
    participants: "Uczestnicy",
    person: "Osoba",
    utterances: "Wypowiedzi",
    words: "Słowa",
    share: "Udział",
    untitled: "Rozmowa",
    empty: "_Brak transkrypcji — nikt nie mówił albo nie było czego słuchać._",
    live: "w trakcie",
};

const EN: Strings = Strings {
    transcript: "Transcript",
    participants: "Participants",
    person: "Person",
    utterances: "Utterances",
    words: "Words",
    share: "Share",
    untitled: "Call",
    empty: "_No transcript — nobody spoke or there was nothing to listen to._",
    live: "live",
};

fn source_label(source: &str) -> &str {
    match source {
        "google-meet" => "Google Meet",
        "system-audio" => "Dźwięk systemowy",
        "microphone" => "Mikrofon",
        "mixed" => "Dźwięk systemowy + mikrofon",
        "file" => "Nagranie z pliku",
        other => other,
    }
}

pub struct Markdown;

impl Markdown {
    /// Chronimy tylko to, co realnie psuje render w środku akapitu.
    pub(crate) fn escape_inline(text: &str) -> String {
        let mut out = String::new();
        for ch in Text::normalize(text).chars() {
            if ch == '*' || ch == '_' || ch == '`' {
                out.push('\\');
            }
            out.push(ch);
        }
        out
    }

    pub(crate) fn yaml_string(value: &str) -> String {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }

    pub fn render(session: &Session, options: &MarkdownOptions) -> String {
        let t = match options.locale.as_str() {
            "en" => &EN,
            _ => &PL,
        };
        let meta = &session.meta;
        let mut segments = session.segments.clone();
        segments.sort_by(|a, b| a.started_at.partial_cmp(&b.started_at).unwrap_or(std::cmp::Ordering::Equal));

        let started_at = meta.started_at.or(segments.first().map(|s| s.started_at)).unwrap_or_else(now_ms);
        let ended_at = meta.ended_at.or(segments.last().map(|s| s.ended_at)).unwrap_or(started_at);
        let duration_ms = (ended_at - started_at).max(0.0);
        let title_raw = Text::normalize(&meta.title);
        let title = if title_raw.is_empty() { t.untitled.to_string() } else { title_raw };
        let source_label = source_label(&meta.source);

        let mut order: Vec<String> = Vec::new();
        let mut rows: HashMap<String, (usize, usize)> = HashMap::new();
        for s in &segments {
            let row = rows.entry(s.speaker.clone()).or_insert_with(|| {
                order.push(s.speaker.clone());
                (0, 0)
            });
            row.0 += 1;
            row.1 += Text::word_count(&s.text);
        }
        let total_words = order.iter().map(|n| rows[n].1).sum::<usize>().max(1);

        let mut out: Vec<String> = Vec::new();

        if options.frontmatter {
            out.push("---".into());
            out.push(format!("title: {}", Self::yaml_string(&title)));
            if !meta.source.is_empty() {
                out.push(format!("source: {}", meta.source));
            }
            if !meta.url.is_empty() {
                out.push(format!("url: {}", Self::yaml_string(&meta.url)));
            }
            out.push(format!("date: {}", TimeFormat::local_date(started_at)));
            out.push(format!("started: {}", Self::yaml_string(&TimeFormat::local_date_time(started_at, true))));
            out.push(format!("duration: {}", Self::yaml_string(&TimeFormat::offset(duration_ms))));
            let speakers: Vec<String> = order.iter().map(|n| Self::yaml_string(n)).collect();
            out.push(format!("speakers: [{}]", speakers.join(", ")));
            out.push("generator: call-whisper".into());
            out.push("---".into());
            out.push(String::new());
        }

        out.push(format!("# {title}"));
        out.push(String::new());

        let mut header_bits = vec![TimeFormat::local_date_time(started_at, false)];
        if duration_ms > 0.0 {
            header_bits.push(TimeFormat::duration(duration_ms));
        }
        if !source_label.is_empty() {
            header_bits.push(source_label.to_string());
        }
        out.push(header_bits.join(" · "));
        out.push(String::new());

        if options.stats && !order.is_empty() {
            out.push(format!("## {}", t.participants));
            out.push(String::new());
            out.push(format!("| {} | {} | {} | {} |", t.person, t.utterances, t.words, t.share));
            out.push("| --- | ---: | ---: | ---: |".into());
            for name in &order {
                let (utterances, words) = rows[name];
                let share = (words as f64 / total_words as f64 * 100.0).round() as i64;
                out.push(format!("| {name} | {utterances} | {words} | {share}% |"));
            }
            out.push(String::new());
        }

        out.push(format!("## {}", t.transcript));
        out.push(String::new());

        if segments.is_empty() {
            out.push(t.empty.into());
            out.push(String::new());
        }

        for s in &segments {
            let stamp = if options.absolute_timestamps {
                TimeFormat::local_time(s.started_at, true)
            } else {
                TimeFormat::offset(s.offset_ms)
            };
            let suffix = if s.is_final { String::new() } else { format!(" _({})_", t.live) };
            out.push(format!("**[{stamp}] {}**{suffix}", s.speaker));
            out.push(String::new());
            out.push(if options.escape { Self::escape_inline(&s.text) } else { Text::normalize(&s.text) });
            out.push(String::new());
        }

        let mut body = out.join("\n");
        while body.contains("\n\n\n") {
            body = body.replace("\n\n\n", "\n\n");
        }
        body.trim().to_string() + "\n"
    }

    /// Wypowiedzi jako zwykły tekst do wklejenia w czat: jedna linia na
    /// wypowiedź, z czasem i mówcą, bez nagłówków i tabel z eksportu.
    pub fn chat_text(segments: &[Segment]) -> String {
        segments
            .iter()
            .map(|s| (s, Text::normalize(&s.text)))
            .filter(|(_, text)| !text.is_empty())
            .map(|(s, text)| format!("[{}] {}: {}", TimeFormat::offset(s.offset_ms), s.speaker, text))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Krótki podgląd tekstowy (menu, powiadomienia). Swift: domyślnie `limit = 6`.
    pub fn preview(segments: &[Segment], limit: usize) -> String {
        segments[segments.len().saturating_sub(limit)..]
            .iter()
            .map(|s| format!("[{}] {}: {}", TimeFormat::offset(s.offset_ms), s.speaker, s.text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

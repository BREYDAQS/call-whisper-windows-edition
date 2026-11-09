//! Łańcuch przetwarzania jednego źródła dźwięku — port `SourcePipeline.swift`.
//!
//! Aktor na tokio: jedna pętla posiada cały stan (framer, diaryzator, bufor
//! kołowy) i dostaje wiadomości kanałem. Żądania do whispera idą jako osobne
//! zadania i odsyłają wynik tym samym kanałem, więc pętla nigdy nie czeka
//! na sieć — ramki lecą co 10 ms i nie mogą stać w kolejce za HTTP.

use crate::audio::{AudioSource, PcmChunk, ASR_SAMPLE_RATE};
use crate::whisper_client::{Quality, WhisperClient, WhisperError};
use cw_core::audio_ring::AudioRing;
use cw_core::diarizer::{Diarizer, Framer, SpeakerTracker, Turn};
use cw_core::dsp::{MfccExtractor, Vad};
use cw_core::text::Text;
use cw_core::transcript_store::UNKNOWN_SPEAKER;
use cw_core::vocabulary::Vocabulary;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Clone, Debug)]
pub struct Update {
    /// Klucz segmentu w `TranscriptStore` — stały przez całą wypowiedź.
    pub key: String,
    pub speaker: String,
    pub text: String,
    pub is_final: bool,
    pub start_ms: f64,
    /// Wypowiedź nie dała tekstu — segment ma zniknąć, a nie zostać pusty.
    pub discard: bool,
}

pub enum Event {
    Update(Update),
    Error(String),
}

pub struct Config {
    pub source: AudioSource,
    pub whisper_port: u16,
    pub language_code: String,
    pub identify_speakers: bool,
    pub vocabulary: String,
    /// Rundy przyrostowe (tekst w trakcie mówienia). Wyłączone = tylko wersja
    /// ostateczna po końcu wypowiedzi: jedno przejście modelu na wypowiedź
    /// zamiast jednego co 1,2 s (na macOS ~8 % rdzenia -> ~2 %).
    pub live_preview: bool,
}

/// Uchwyt do działającego łańcucha.
pub struct Pipeline {
    tx: mpsc::UnboundedSender<Msg>,
    task: JoinHandle<()>,
    speakers: Arc<AtomicUsize>,
}

impl Pipeline {
    pub fn start(config: Config, events: mpsc::UnboundedSender<Event>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let speakers = Arc::new(AtomicUsize::new(0));
        let state = State::new(config, tx.clone(), events, speakers.clone());
        let ticker_tx = tx.clone();
        let task = tokio::spawn(async move {
            // Co 1200 ms runda przyrostowa i pilnowanie ciszy w strumieniu.
            let ticker = tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(1200));
                loop {
                    interval.tick().await;
                    if ticker_tx.send(Msg::Tick).is_err() {
                        break;
                    }
                }
            });
            state.run(rx).await;
            ticker.abort();
        });
        Self { tx, task, speakers }
    }

    /// Wołane z wątku przechwytywania. Kanał zachowuje kolejność paczek —
    /// przestawione bloki rozjechałyby oś czasu diaryzatora względem ASR.
    pub fn sender(&self) -> impl Fn(PcmChunk) + Send + 'static {
        let tx = self.tx.clone();
        move |chunk| {
            let _ = tx.send(Msg::Chunk(chunk));
        }
    }

    pub fn speaker_count(&self) -> usize {
        self.speakers.load(Ordering::Relaxed)
    }

    /// Zatrzymanie natychmiastowe: bez dokańczania zaległości. Tekst, który
    /// już jest, i tak zostaje w transkrypcie.
    pub fn cancel(self) {
        let _ = self.tx.send(Msg::Cancel);
        self.task.abort();
    }
}

enum Msg {
    Chunk(PcmChunk),
    Tick,
    Partial {
        key: String,
        start_ms: f64,
        result: Result<String, String>,
    },
    Final {
        key: String,
        speaker: String,
        start_ms: f64,
        fallback: String,
        result: Result<String, String>,
    },
    Cancel,
}

enum DiarEvent {
    TurnStart(f64),
    Turn(Turn),
}

struct Utterance {
    key: String,
    start_ms: f64,
    speaker: Option<String>,
    last_text: String,
}

/// Co ile odświeżamy transkrypcję trwającej wypowiedzi — ticker wyżej.
/// Zanim to minie, nie ma czego transkrybować: na krótkim urywku whisper
/// chętnie zmyśla całe zdanie, a zmyślony tekst wchodzi potem do kontekstu.
const MIN_AUDIO_MS: f64 = 1400.0;
/// Twardy limit jednej wypowiedzi: przy mowie ciągłej (TTS, czytany tekst)
/// cisza nie nadchodzi i tura wisiałaby otwarta.
const MAX_UTTERANCE_MS: f64 = 25_000.0;
/// Margines przed początkiem tury: VAD ucina ciche początki głosek.
const PAD_MS: f64 = 200.0;
/// Krótsze tury odrzucamy bez pytania whispera — kaszlnięcia i trzaski.
const MIN_TURN_MS: f64 = 600.0;
const MAX_CONTEXT_CHARS: usize = 400;
/// WASAPI loopback, tak jak ScreenCaptureKit, przestaje wysyłać paczki, gdy
/// w systemie jest cisza. Po tylu ms bez dźwięku domykamy turę sami.
const AUDIO_STALL_MS: f64 = 800.0;
/// Ile jeszcze czekamy na przerwę po przekroczeniu limitu wypowiedzi.
const CUT_GRACE_MS: f64 = 8_000.0;

struct State {
    source: AudioSource,
    identify_speakers: bool,
    live_preview: bool,
    framer: Framer,
    diarizer: Diarizer,
    diar_rx: std::sync::mpsc::Receiver<DiarEvent>,
    ring: AudioRing,
    hop_size: usize,
    whisper: WhisperClient,
    /// Ostatnie zdania rozmowy jako `prompt` whispera: zmierzone 23,3 % ->
    /// 13,3 % WER kosztem 33 ms.
    context: String,
    /// Stałe słownictwo rozmowy doklejane przed kontekst.
    vocabulary: String,
    utterance: Option<Utterance>,
    in_flight: Option<JoinHandle<()>>,
    finals: Vec<JoinHandle<()>>,
    sequence: u64,
    last_frame_ms: f64,
    /// Czas próbek -> czas sesji. Bufor i diaryzator żyją w czasie próbek
    /// (przerwy w dostawie w nim nie istnieją), `TranscriptStore` w czasie
    /// sesji. Przesunięcie odświeżane przy każdej paczce.
    samples_ingested: usize,
    clock_offset_ms: f64,
    last_chunk_at: Instant,
    tx: mpsc::UnboundedSender<Msg>,
    events: mpsc::UnboundedSender<Event>,
    speakers: Arc<AtomicUsize>,
}

impl State {
    fn new(
        config: Config,
        tx: mpsc::UnboundedSender<Msg>,
        events: mpsc::UnboundedSender<Event>,
        speakers: Arc<AtomicUsize>,
    ) -> Self {
        let extractor = MfccExtractor::default();
        let hop_size = extractor.hop_size;
        let framer = Framer::new(extractor.frame_size, extractor.hop_size);
        let mut diarizer = Diarizer::new(
            extractor,
            Vad::default(),
            SpeakerTracker::default(),
            25,
            25,
            config.identify_speakers,
        );
        let (diar_tx, diar_rx) = std::sync::mpsc::channel();
        let start_tx = diar_tx.clone();
        diarizer.on_turn_start = Box::new(move |ms| {
            let _ = start_tx.send(DiarEvent::TurnStart(ms));
        });
        diarizer.on_turn = Box::new(move |turn| {
            let _ = diar_tx.send(DiarEvent::Turn(turn));
        });
        let vocabulary: String = Text::normalize(&config.vocabulary)
            .chars()
            .take(300)
            .collect();
        Self {
            source: config.source,
            identify_speakers: config.identify_speakers,
            live_preview: config.live_preview,
            framer,
            diarizer,
            diar_rx,
            ring: AudioRing::new(ASR_SAMPLE_RATE, 90.0, 0.0),
            hop_size,
            whisper: WhisperClient::new(config.whisper_port, &config.language_code, 15),
            context: String::new(),
            vocabulary,
            utterance: None,
            in_flight: None,
            finals: Vec::new(),
            sequence: 0,
            last_frame_ms: 0.0,
            samples_ingested: 0,
            clock_offset_ms: 0.0,
            last_chunk_at: Instant::now(),
            tx,
            events,
            speakers,
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::Chunk(chunk) => self.process(chunk),
                Msg::Tick => self.tick(),
                Msg::Partial {
                    key,
                    start_ms,
                    result,
                } => self.partial_done(key, start_ms, result),
                Msg::Final {
                    key,
                    speaker,
                    start_ms,
                    fallback,
                    result,
                } => self.final_done(key, speaker, start_ms, fallback, result),
                Msg::Cancel => break,
            }
            self.drain_diarizer();
            self.speakers
                .store(self.diarizer.speaker_count(), Ordering::Relaxed);
        }
        if let Some(task) = self.in_flight.take() {
            task.abort();
        }
        for task in self.finals.drain(..) {
            task.abort();
        }
    }

    fn session_ms(&self, sample_ms: f64) -> f64 {
        sample_ms + self.clock_offset_ms
    }

    fn whisper_prompt(&self) -> String {
        if self.vocabulary.is_empty() {
            self.context.clone()
        } else {
            format!("{} {}", self.vocabulary, self.context)
                .chars()
                .take(MAX_CONTEXT_CHARS + Vocabulary::MAX_CHARS)
                .collect()
        }
    }

    fn process(&mut self, chunk: PcmChunk) {
        self.last_chunk_at = Instant::now();
        self.clock_offset_ms =
            chunk.start_ms - self.samples_ingested as f64 / ASR_SAMPLE_RATE * 1000.0;
        self.samples_ingested += chunk.samples.len();

        for frame in self.framer.push(&chunk.samples) {
            let t_ms = frame.start_sample as f64 / ASR_SAMPLE_RATE * 1000.0;
            self.last_frame_ms = t_ms;
            // Do bufora tylko nowa część ramki — ramki nachodzą na siebie.
            let fresh = &frame.frame[frame.frame.len() - self.hop_size..];
            self.ring.write(fresh);
            self.diarizer.push_frame(&frame.frame, t_ms);
            self.drain_diarizer();

            // Mowa ciągła bez pauz: po limicie czekamy na najbliższą cichą
            // ramkę (przerwę między zdaniami), a nie tniemy w pół słowa.
            if let Some(open) = &self.utterance {
                let elapsed = t_ms - open.start_ms;
                if (elapsed >= MAX_UTTERANCE_MS && !self.diarizer.last_frame_loud())
                    || elapsed >= MAX_UTTERANCE_MS + CUT_GRACE_MS
                {
                    self.diarizer.flush(t_ms);
                    self.drain_diarizer();
                }
            }
        }
    }

    fn drain_diarizer(&mut self) {
        while let Ok(event) = self.diar_rx.try_recv() {
            match event {
                DiarEvent::TurnStart(ms) => self.open_utterance(ms),
                DiarEvent::Turn(turn) => self.close_utterance(turn),
            }
        }
    }

    fn open_utterance(&mut self, start_ms: f64) {
        self.sequence += 1;
        self.utterance = Some(Utterance {
            key: format!("{}#{}", self.source.raw(), self.sequence),
            start_ms,
            speaker: None,
            last_text: String::new(),
        });
    }

    fn tick(&mut self) {
        // Dźwięk ucichł i przestał przychodzić — domykamy turę sami.
        if self.utterance.is_some()
            && self.last_chunk_at.elapsed().as_secs_f64() * 1000.0 > AUDIO_STALL_MS
        {
            self.diarizer.flush(self.last_frame_ms);
            return;
        }
        let Some(u) = &self.utterance else { return };
        if self.ring.newest_ms() - u.start_ms >= MAX_UTTERANCE_MS {
            self.diarizer.flush(self.last_frame_ms);
            return;
        }
        if !self.live_preview {
            return;
        }
        if self.in_flight.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let now = self.ring.newest_ms();
        if now - u.start_ms < MIN_AUDIO_MS {
            return;
        }
        let to = now.min(u.start_ms + MAX_UTTERANCE_MS);
        let Some(pcm) = self
            .ring
            .read_range(u.start_ms - PAD_MS, to)
            .filter(|p| !p.is_empty())
        else {
            return;
        };

        // Runda przyrostowa: pełna transkrypcja wypowiedzi od początku.
        // Encoder whispera kosztuje tyle samo niezależnie od długości audio,
        // a tekst pojawia się W TRAKCIE mówienia zamiast po jego końcu.
        let (key, start_ms) = (u.key.clone(), u.start_ms);
        let whisper = self.whisper.clone();
        let prompt = self.whisper_prompt();
        let tx = self.tx.clone();
        self.in_flight = Some(tokio::spawn(async move {
            let result = whisper
                .transcribe(&pcm, &prompt, Quality::FAST)
                .await
                .map_err(describe);
            let _ = tx.send(Msg::Partial {
                key,
                start_ms,
                result,
            });
        }));
    }

    fn partial_done(&mut self, key: String, start_ms: f64, result: Result<String, String>) {
        let text = match result {
            Ok(text) => text,
            Err(err) => return self.error(err),
        };
        // Wypowiedź mogła się w międzyczasie domknąć — wynik dotyczy czegoś,
        // czego już nie ma.
        let label = self.current_label();
        let Some(u) = self.utterance.as_mut().filter(|u| u.key == key) else {
            return;
        };
        if text.is_empty() || text == u.last_text {
            return;
        }
        u.last_text = text.clone();
        u.speaker = Some(label.clone());
        let start = self.session_ms(start_ms);
        self.emit(Update {
            key,
            speaker: label,
            text,
            is_final: false,
            start_ms: start,
            discard: false,
        });
    }

    fn current_label(&self) -> String {
        if let Some(known) = self.utterance.as_ref().and_then(|u| u.speaker.clone()) {
            return known;
        }
        self.label(self.diarizer.current_speaker())
    }

    fn close_utterance(&mut self, turn: Turn) {
        // Wypowiedź przejmujemy od razu: następna tura może ruszyć, zanim
        // whisper odda wersję ostateczną tej.
        let Some(u) = self.utterance.take() else {
            return;
        };
        let speaker = self.label(Some(turn.speaker));
        let start = self.session_ms(u.start_ms);

        if turn.end_ms - turn.start_ms < MIN_TURN_MS {
            self.emit(Update {
                key: u.key,
                speaker,
                text: String::new(),
                is_final: true,
                start_ms: start,
                discard: true,
            });
            return;
        }

        // Runda przyrostowa w locie dotyczy krótszego audio — nieaktualna.
        if let Some(task) = self.in_flight.take() {
            task.abort();
        }

        let pcm = self
            .ring
            .read_range(
                u.start_ms - PAD_MS,
                (turn.end_ms + PAD_MS).min(u.start_ms + MAX_UTTERANCE_MS),
            )
            .filter(|p| !p.is_empty());
        let tx = self.tx.clone();
        let Some(pcm) = pcm else {
            let _ = tx.send(Msg::Final {
                key: u.key,
                speaker,
                start_ms: u.start_ms,
                fallback: u.last_text,
                result: Ok(String::new()),
            });
            return;
        };
        let whisper = self.whisper.clone();
        let prompt = self.whisper_prompt();
        self.finals.retain(|t| !t.is_finished());
        self.finals.push(tokio::spawn(async move {
            let result = whisper
                .transcribe(&pcm, &prompt, Quality::ACCURATE)
                .await
                .map_err(describe);
            let _ = tx.send(Msg::Final {
                key: u.key,
                speaker,
                start_ms: u.start_ms,
                fallback: u.last_text,
                result,
            });
        }));
    }

    fn final_done(
        &mut self,
        key: String,
        speaker: String,
        start_ms: f64,
        fallback: String,
        result: Result<String, String>,
    ) {
        let text = match result {
            Ok(text) if !text.is_empty() => text,
            Ok(_) => fallback,
            Err(err) => {
                self.error(err);
                fallback
            }
        };
        // Domknięta wypowiedź zasila kontekst kolejnej.
        if !text.is_empty() {
            let joined = format!("{} {}", self.context, text);
            let skip = joined.chars().count().saturating_sub(MAX_CONTEXT_CHARS);
            self.context = joined.chars().skip(skip).collect();
        }
        let start = self.session_ms(start_ms);
        let discard = text.is_empty();
        self.emit(Update {
            key,
            speaker,
            text,
            is_final: true,
            start_ms: start,
            discard,
        });
    }

    /// Bez rozpoznawania mówcy zostaje podział, który i tak jest pewny:
    /// mikrofon to Ty, dźwięk komputera to reszta.
    fn label(&self, index: Option<usize>) -> String {
        if !self.identify_speakers {
            return if self.source == AudioSource::Microphone {
                "Ty"
            } else {
                "Rozmówcy"
            }
            .into();
        }
        match self.source {
            AudioSource::Microphone => match index {
                Some(i) if i > 0 => format!("Osoba obok {}", i + 1),
                _ => "Ty".into(),
            },
            AudioSource::System => match index {
                Some(i) => format!("Rozmówca {}", i + 1),
                None => UNKNOWN_SPEAKER.into(),
            },
        }
    }

    fn emit(&self, update: Update) {
        let _ = self.events.send(Event::Update(update));
    }

    fn error(&self, err: String) {
        if !err.is_empty() {
            let _ = self.events.send(Event::Error(err));
        }
    }
}

/// Anulowanie to tu przerwane zadanie tokio, nie błąd — do kanału nie dociera.
fn describe(err: WhisperError) -> String {
    err.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cały łańcuch na prawdziwym nagraniu: `CW_TEST_AUDIO=plik CW_TEST_PORT=8898
    /// cargo test -p call-whisper -- --ignored live_pipeline` (z działającym
    /// whisper-server i ffmpeg w PATH).
    #[tokio::test]
    #[ignore]
    async fn live_pipeline_transcribes_speech() {
        let file = std::env::var("CW_TEST_AUDIO").expect("CW_TEST_AUDIO");
        let port: u16 = std::env::var("CW_TEST_PORT")
            .unwrap_or("8898".into())
            .parse()
            .unwrap();
        let mut pcm = crate::media_import::decode(
            std::path::Path::new("ffmpeg"),
            std::path::Path::new(&file),
        )
        .unwrap();
        // Cisza na końcu, żeby VAD domknął turę.
        pcm.extend(std::iter::repeat(0.0).take(16_000 * 2));

        let (events_tx, mut events) = mpsc::unbounded_channel();
        let pipeline = Pipeline::start(
            Config {
                source: AudioSource::System,
                whisper_port: port,
                language_code: "pl".into(),
                identify_speakers: false,
                vocabulary: "React, Next.js".into(),
                live_preview: true,
            },
            events_tx,
        );
        let send = pipeline.sender();
        // Tempo zbliżone do rzeczywistego: paczki po 100 ms co 50 ms.
        for (i, chunk) in pcm.chunks(1600).enumerate() {
            send(PcmChunk {
                samples: chunk.to_vec(),
                start_ms: i as f64 * 100.0,
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let mut finals = Vec::new();
        let mut partials = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
            match event {
                Event::Update(u) if u.is_final && !u.discard => {
                    finals.push(u.text.clone());
                    break;
                }
                Event::Update(u) if !u.is_final => partials += 1,
                Event::Update(_) => {}
                Event::Error(e) => panic!("{e}"),
            }
        }
        pipeline.cancel();
        let text = finals.join(" ");
        eprintln!("partials={partials} final={text:?}");
        assert!(text.to_lowercase().contains("react"), "{text}");
        assert!(partials > 0, "brak rund przyrostowych");
    }
}

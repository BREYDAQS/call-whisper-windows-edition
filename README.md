# brqs-whisper — Windows

Transkrypcja rozmów na żywo (whisper.cpp, lokalnie) z podpowiedziami. Port
aplikacji macOS z [vibebrosi-main/whisper](https://github.com/vibebrosi-main/whisper).

## Instalacja

Pobierz `brqs-whisper_*_x64-setup.exe` z [Releases](../../releases/latest)
i uruchom. Instalator wiezie silnik mowy (Vulkan na każde GPU, zapasowo CPU)
ffmpeg i rozpoznawanie głosów (sherpa-onnx). Model mowy (~490 MB) pobiera się przy pierwszym „Słuchaj".

Instalator nie jest podpisany — SmartScreen pokaże ostrzeżenie:
„Więcej informacji" → „Uruchom mimo to".

## Aktualizacje

Każdy push na `main` buduje nowe wydanie (GitHub Actions). Aplikacja co 30 min
sprawdza najnowsze i instaluje je po zakończonej rozmowie. Tylko pobiera.

## Budowanie lokalnie (Windows)

Rust, Visual Studio Build Tools (C++), Vulkan SDK, CMake. Silnik mowy i ffmpeg
do `vendor/` — kroki jak w `.github/workflows/build.yml`. Potem:

```
cargo install tauri-cli --version "^2"
cargo tauri build
```

Testy rdzenia: `cargo test -p cw-core`.

## Stan portu

- [x] Etap 1: dźwięk komputera (WASAPI loopback) i mikrofon z wyborem
  urządzenia, whisper na żywo, transkrypt, kopiowanie, eksport, ustawienia,
  gotowość, aktualizacje
- [x] Etap 2: podpowiedzi (Claude Code, API), wykrywanie pytań, zrzuty ekranu
- [x] Etap 3: nakładka, pasek u góry ekranu (ukryte w udostępnianiu ekranu), import nagrań
- [x] Etap 4: wykrywanie rozmów, OBS
- [x] Rozpoznawanie głosów po rozmowie (sherpa-onnx: pyannote + CAM++, jak na macOS)
- [x] Dyktowanie do dowolnego pola: skrót (domyślnie Ctrl+Shift+J, własny), trzymaj albo
  naciśnij, model Turbo rozgrzany od startu, wklejanie Ctrl+V z przywróceniem
  schowka, wyciszanie dźwięku systemu, pigułka u dołu ekranu, gotowość i próba 3 s
- [x] Podpowiedzi przez Codex CLI (ChatGPT, bez klucza): `npm i -g @openai/codex`,
  potem `codex login`; w Ustawieniach „Skąd biorą się podpowiedzi” → Codex
- [x] Tekst na żywo do wyłączenia (domyślnie wył. — mniej obciążenia w rozmowie)
- [ ] Parakeet na żywo, analiza rozmowy przez Claude Code, biblioteka modeli

## Licencja

Patrz [LICENSE.md](LICENSE.md).
<!-- bump: 29ce314 -->

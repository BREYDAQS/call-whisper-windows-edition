//! Okna pomocnicze: nakładka z odpowiedziami i pasek u góry ekranu.
//!
//! Oba zawsze na wierzchu, bez ramki, bez fokusu (kliknięcie nie może
//! zabrać klawiatury rozmowie) i z `content_protected` — Windows wycina je
//! z udostępniania ekranu i nagrań, tak jak wyspa w notchu na macOS.

use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, WebviewUrl, WebviewWindowBuilder};

pub const OVERLAY: &str = "overlay";
pub const TOPBAR: &str = "topbar";

/// Przełącza nakładkę; zwraca, czy jest teraz widoczna.
pub fn toggle_overlay(app: &AppHandle) -> tauri::Result<bool> {
    if let Some(window) = app.get_webview_window(OVERLAY) {
        window.close()?;
        return Ok(false);
    }
    let (x, y) = corner(app, 400.0, 520.0);
    WebviewWindowBuilder::new(app, OVERLAY, WebviewUrl::App("overlay.html".into()))
        .title("call-whisper — nakładka")
        .inner_size(400.0, 520.0)
        .min_inner_size(300.0, 240.0)
        .position(x, y)
        .always_on_top(true)
        .decorations(false)
        .skip_taskbar(true)
        .focused(false)
        .content_protected(true)
        .build()?;
    Ok(true)
}

/// Prawy dolny róg ekranu głównego, nad paskiem zadań.
fn corner(app: &AppHandle, width: f64, height: f64) -> (f64, f64) {
    app.primary_monitor()
        .ok()
        .flatten()
        .map(|m| {
            let scale = m.scale_factor();
            let size = m.size().to_logical::<f64>(scale);
            (size.width - width - 24.0, size.height - height - 72.0)
        })
        .unwrap_or((40.0, 40.0))
}

/// Pasek widoczny tylko w trakcie nasłuchu (i gdy włączony w ustawieniach).
pub fn sync_topbar(app: &AppHandle, show: bool) {
    let existing = app.get_webview_window(TOPBAR);
    match (show, existing) {
        (true, Some(window)) => {
            let _ = window.show();
        }
        (true, None) => {
            let built =
                WebviewWindowBuilder::new(app, TOPBAR, WebviewUrl::App("topbar.html".into()))
                    .title("call-whisper — pasek")
                    .inner_size(300.0, 54.0)
                    .always_on_top(true)
                    .decorations(false)
                    .transparent(true)
                    .shadow(false)
                    .resizable(false)
                    .skip_taskbar(true)
                    .focused(false)
                    .content_protected(true)
                    .build();
            if built.is_ok() {
                place_topbar(app, 300.0, 54.0);
            }
        }
        (false, Some(window)) => {
            let _ = window.hide();
        }
        (false, None) => {}
    }
}

/// Rozmiar paska zależy od treści; zawsze na środku, przy górnej krawędzi.
pub fn place_topbar(app: &AppHandle, width: f64, height: f64) {
    let Some(window) = app.get_webview_window(TOPBAR) else {
        return;
    };
    let screen_width = window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten())
        .map(|m| m.size().to_logical::<f64>(m.scale_factor()).width)
        .unwrap_or(1920.0);
    let _ = window.set_size(LogicalSize::new(width, height));
    let _ = window.set_position(LogicalPosition::new((screen_width - width) / 2.0, 0.0));
}

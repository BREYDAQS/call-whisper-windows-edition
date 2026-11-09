//! Wykrywanie rozmowy — warstwa Windows dla `cw_core::meeting_detection`.
//!
//! Windows zapisuje w rejestrze, która aplikacja trzyma mikrofon
//! (CapabilityAccessManager\ConsentStore\microphone: `LastUsedTimeStop == 0`
//! znaczy „używa teraz"). To dokładniej niż na macOS, gdzie widać tylko, że
//! *ktoś* trzyma mikrofon — tu wiadomo kto, więc Slack otwarty cały dzień
//! nie udaje rozmowy.

use cw_core::meeting_detection::{Debouncer, MeetingChange, MeetingDetection};
use std::collections::HashSet;

/// Plik wykonywalny albo rodzina pakietu -> identyfikator z rdzenia.
const APPS: &[(&str, &str)] = &[
    ("zoom.exe", "us.zoom.xos"),
    ("ms-teams.exe", "com.microsoft.teams2"),
    ("msteams_", "com.microsoft.teams2"),
    ("teams.exe", "com.microsoft.teams"),
    ("slack.exe", "com.tinyspeck.slackmacgap"),
    ("discord.exe", "com.hnc.Discord"),
    ("ciscocollabhost.exe", "Cisco-Systems.Spark"),
    ("webex.exe", "Cisco-Systems.Spark"),
    ("atmgr.exe", "com.cisco.webexmeetingsapp"),
    ("whatsapp", "net.whatsapp.WhatsApp"),
    ("telegram.exe", "ru.keepcoder.Telegram"),
    ("skype", "com.skype.skype"),
    ("chrome.exe", "com.google.Chrome"),
    ("msedge.exe", "com.microsoft.edgemac"),
    ("firefox.exe", "org.mozilla.firefox"),
    ("brave.exe", "com.brave.Browser"),
    ("arc.exe", "company.thebrowser.Browser"),
];

fn identify(holder: &str) -> Option<&'static str> {
    let name = holder.to_lowercase();
    // NonPackaged: pełna ścieżka z `#` zamiast `\`; bierzemy nazwę pliku.
    let file = name.rsplit(['#', '\\']).next().unwrap_or(&name);
    APPS.iter()
        .find(|(pattern, _)| {
            file == *pattern || (!pattern.ends_with(".exe") && file.contains(pattern))
        })
        .map(|(_, id)| *id)
}

/// Kto trzyma teraz mikrofon (klucze z rejestru), bez nas samych.
#[cfg(windows)]
pub fn microphone_holders() -> Vec<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    const BASE: &str = r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";
    let mut holders = Vec::new();
    let Ok(root) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(BASE) else {
        return holders;
    };
    let mut scan = |key: &RegKey| {
        for name in key.enum_keys().flatten() {
            let Ok(app) = key.open_subkey(&name) else {
                continue;
            };
            let stop: u64 = app.get_value("LastUsedTimeStop").unwrap_or(1);
            let start: u64 = app.get_value("LastUsedTimeStart").unwrap_or(0);
            // Sami nie jesteśmy rozmową: nasłuch i dyktowanie też trzymają mikrofon.
            let own = name.to_lowercase();
            if stop == 0 && start > 0 && !own.contains("call-whisper") && !own.contains("brqs-whisper") {
                holders.push(name);
            }
        }
    };
    scan(&root);
    if let Ok(non_packaged) = root.open_subkey("NonPackaged") {
        scan(&non_packaged);
    }
    holders
}

#[cfg(not(windows))]
pub fn microphone_holders() -> Vec<String> {
    Vec::new()
}

/// Nazwa aplikacji rozmowy albo `None`.
pub fn active_meeting(holders: &[String]) -> Option<&'static str> {
    let ids: Vec<&str> = holders.iter().filter_map(|h| identify(h)).collect();
    let running: HashSet<String> = ids.iter().map(|s| s.to_string()).collect();
    // Przeglądarka trzymająca mikrofon to zwykle Google Meet w karcie —
    // podajemy ją jako „na pierwszym planie", żeby rdzeń ją uznał.
    let browser = ids.iter().copied().find(|id| {
        cw_core::meeting_detection::BROWSERS
            .iter()
            .any(|(b, _)| b == id)
    });
    MeetingDetection::active_meeting(!holders.is_empty(), &running, browser)
}

/// Odpytywanie co 2 s z histerezą: rozmowa zaczyna się po 2 trafieniach
/// i kończy po 5 pudłach.
pub struct MeetingWatcher {
    debouncer: Debouncer,
}

impl MeetingWatcher {
    pub fn new() -> Self {
        Self {
            debouncer: Debouncer::new(2, 5),
        }
    }

    pub fn poll(&mut self) -> Option<MeetingChange> {
        let holders = microphone_holders();
        self.debouncer.feed(active_meeting(&holders))
    }

    pub fn active(&self) -> Option<String> {
        self.debouncer.active().map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_non_packaged_paths_and_packages() {
        assert_eq!(
            identify(r"C:#Users#Ala#AppData#Roaming#Zoom#bin#Zoom.exe"),
            Some("us.zoom.xos")
        );
        assert_eq!(
            identify("MSTeams_8wekyb3d8bbwe"),
            Some("com.microsoft.teams2")
        );
        assert_eq!(
            identify("5319275A.WhatsAppDesktop_cv1g1gvanyjgm"),
            Some("net.whatsapp.WhatsApp")
        );
        assert_eq!(
            identify(r"C:#Program Files#Google#Chrome#Application#chrome.exe"),
            Some("com.google.Chrome")
        );
    }

    #[test]
    fn browser_with_mic_is_a_meeting() {
        let holders = vec![r"C:#Program Files#Google#Chrome#Application#chrome.exe".to_string()];
        assert_eq!(active_meeting(&holders), Some("przeglądarce"));
    }

    #[test]
    fn known_app_wins_over_browser() {
        let holders = vec![
            r"C:#Program Files#Google#Chrome#Application#chrome.exe".to_string(),
            r"C:#Users#Ala#AppData#Roaming#Zoom#bin#Zoom.exe".to_string(),
        ];
        assert_eq!(active_meeting(&holders), Some("Zoom"));
    }

    #[test]
    fn no_mic_no_meeting() {
        assert_eq!(active_meeting(&[]), None);
    }
}

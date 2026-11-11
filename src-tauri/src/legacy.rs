//! Sprzątanie po dawnej nazwie (brqs-whisper -> z powrotem call-whisper).
//!
//! Zmiana nazwy produktu sprawiła, że nowy instalator kładzie się obok
//! starego, zamiast go zastąpić. Zostawały: stara aplikacja w
//! %LOCALAPPDATA%\brqs-whisper, jej skróty i wpis w „Aplikacje i funkcje".
//!
//! Dane zostają tam, gdzie były: ustawienia (%APPDATA%\call-whisper),
//! modele i logi (%LOCALAPPDATA%\call-whisper\models, \logs) — brqs-whisper
//! też z nich korzystała. W %LOCALAPPDATA%\brqs-whisper leżą tylko pliki
//! starej aplikacji. Jedyne dane pod starą nazwą to transkrypty
//! (Dokumenty\brqs-whisper) — przenosimy je.

/// Działająca stara apka ma ten sam identyfikator, więc wtyczka pojedynczej
/// instancji pokazałaby ją zamiast uruchomić nową. Dlatego zamykamy ją, zanim
/// wtyczka się włączy. Jej serwery mowy giną razem z nią (Job Object).
#[cfg(windows)]
pub fn close_old_instances() {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE,
    };

    let Some(old_dir) = dirs::data_local_dir().map(|d| d.join("brqs-whisper")) else {
        return;
    };
    let me = std::process::id();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snapshot, &mut entry) != 0;
        let mut killed = false;
        while ok {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            // Nowa apka to call-whisper.exe. Zamykamy tylko brqs-whisper.exe
            // ze starego folderu instalacji — nie wersję deweloperską z cargo.
            if name.eq_ignore_ascii_case("brqs-whisper.exe") && entry.th32ProcessID != me {
                let process = OpenProcess(
                    PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    entry.th32ProcessID,
                );
                if !process.is_null() {
                    let mut buffer = [0u16; 1024];
                    let mut size = buffer.len() as u32;
                    let path = (QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut size) != 0)
                        .then(|| std::path::PathBuf::from(String::from_utf16_lossy(&buffer[..size as usize])));
                    if path.is_some_and(|p| p.starts_with(&old_dir)) {
                        killed |= TerminateProcess(process, 0) != 0;
                    }
                    CloseHandle(process);
                }
            }
            ok = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        if killed {
            // Chwila na zwolnienie plików i muteksu pojedynczej instancji.
            std::thread::sleep(std::time::Duration::from_millis(600));
        }
    }
}

#[cfg(not(windows))]
pub fn close_old_instances() {}

/// Dokumenty\brqs-whisper -> Dokumenty\call-whisper, gdy folder transkryptów
/// jest domyślny, a nowego jeszcze nie ma. Przed startem nagrywania, żeby
/// pierwszy zapis nie założył pustego nowego folderu.
#[cfg(windows)]
pub fn move_transcripts() {
    if !crate::settings::Settings::load().transcripts_dir.trim().is_empty() {
        return;
    }
    let new = crate::settings::default_transcripts_dir();
    let Some(old) = new.parent().map(|p| p.join("brqs-whisper")) else {
        return;
    };
    if old.is_dir() && !new.exists() {
        let _ = std::fs::rename(&old, &new);
    }
}

#[cfg(not(windows))]
pub fn move_transcripts() {}

/// Usuwa pliki, skróty i wpis deinstalacji starej aplikacji. Najlepszym
/// wysiłkiem: co się nie uda (plik zablokowany), spróbujemy przy kolejnym
/// starcie.
#[cfg(windows)]
pub fn cleanup() {
    let Some(local) = dirs::data_local_dir().map(|d| d.join("brqs-whisper")) else {
        return;
    };
    // Gdyby ta kopia sama działała ze starego folderu — nic nie ruszamy.
    if std::env::current_exe()
        .map(|exe| exe.starts_with(&local))
        .unwrap_or(true)
    {
        return;
    }
    // Cały folder starej instalacji: danych w nim nie było (patrz wyżej).
    let _ = std::fs::remove_dir_all(&local);

    if let Some(roaming) = dirs::data_dir() {
        let shortcuts = [
            roaming.join(r"Microsoft\Windows\Start Menu\Programs\brqs-whisper.lnk"),
            roaming.join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar\brqs-whisper.lnk"),
        ];
        for shortcut in shortcuts {
            let _ = std::fs::remove_file(shortcut);
        }
    }
    if let Some(desktop) = dirs::desktop_dir() {
        let _ = std::fs::remove_file(desktop.join("brqs-whisper.lnk"));
    }

    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    // Wpis w „Aplikacje i funkcje" — tylko jeśli to na pewno stara apka.
    let uninstall = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\brqs-whisper";
    let is_old: bool = hkcu
        .open_subkey(uninstall)
        .ok()
        .and_then(|k| k.get_value::<String, _>("DisplayName").ok())
        .is_some_and(|name| name.eq_ignore_ascii_case("brqs-whisper"));
    if is_old {
        let _ = hkcu.delete_subkey_all(uninstall);
    }
    // Klucz instalatora NSIS starej apki. Wydawca (Software\callwhisper)
    // zostaje — pod nim jest też klucz nowej call-whisper.
    let _ = hkcu.delete_subkey_all(r"Software\callwhisper\brqs-whisper");
}

#[cfg(not(windows))]
pub fn cleanup() {}

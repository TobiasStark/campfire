//! Windows-spezifisches: Einzelinstanz (Named Mutex + Event), Autostart, Speichermessung.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentProcess, SetEvent, WaitForSingleObject, INFINITE,
};

/// Macht das Fenster klick-durchlaessig und fokuslos (kein Eintrag in Taskleiste/Alt-Tab).
pub fn make_overlay_window(raw: u64) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
    };
    // SAFETY: `raw` ist das HWND unseres eigenen Overlay-Fensters.
    unsafe {
        let hwnd = HWND(raw as *mut _);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let add =
            (WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW).0 as isize;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | add);
    }
}

/// Startet die App neu (die alte Instanz muss sich danach beenden). `true`, wenn gestartet.
pub fn restart_app() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = to_wide(&exe.to_string_lossy());
    // SAFETY: gueltige, nullterminierte Zeichenketten.
    let r = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(exe.as_ptr()),
            w!("--restarted"),
            PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    r.0 as isize > 32
}

struct SendHandle(HANDLE);
// SAFETY: Kernel-Handles duerfen von jedem Thread benutzt werden.
unsafe impl Send for SendHandle {}

/// Haelt den Mutex der ersten Instanz bis zum Prozessende.
pub struct SingleInstance(#[allow(dead_code)] SendHandle);

/// SID des angemeldeten Nutzers als Text ("S-1-5-21-..."), `None` bei Fehlern.
fn user_sid() -> Option<String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::OpenProcessToken;
    // SAFETY: Token des eigenen Prozesses; Puffer in der gemeldeten Groesse; alle Handles und
    // der von ConvertSidToStringSidW angelegte String werden freigegeben.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        if !ok {
            return None;
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = windows::core::PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s).ok()?;
        let out = s.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(s.0.cast())));
        out
    }
}

/// Name eines prozessuebergreifenden Objekts, eindeutig je Nutzer (I-8).
fn object_name(kind: &str) -> Vec<u16> {
    let sid = user_sid().unwrap_or_else(|| "unknown".into());
    to_wide(&format!("Local\\Campfire.{kind}.{sid}"))
}

/// `Some`, wenn dies die erste Instanz ist.
///
/// Der Mutex wird nicht beim Anlegen, sondern per Warten in Besitz genommen: Hat ein anderer
/// Prozess den Namen nur angelegt (oder ist die alte Instanz abgestuerzt), startet die App
/// trotzdem. Nur eine laufende Instanz, die den Mutex haelt, verhindert den Start (I-8).
pub fn acquire_single_instance() -> Option<SingleInstance> {
    use windows::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
    let name = object_name("SingleInstance");
    // SAFETY: gueltiger, nullterminierter Name; Handle wird bei Misserfolg geschlossen.
    unsafe {
        let h = CreateMutexW(None, false, PCWSTR(name.as_ptr())).ok()?;
        let r = WaitForSingleObject(h, 0);
        if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
            return Some(SingleInstance(SendHandle(h)));
        }
        let _ = CloseHandle(h);
        None
    }
}

fn show_event() -> Option<HANDLE> {
    let name = object_name("ShowWindow");
    // SAFETY: gueltiger Name; automatisches Zuruecksetzen.
    unsafe { CreateEventW(None, false, false, PCWSTR(name.as_ptr())).ok() }
}

/// Zweite Instanz: erste bitten, ihr Fenster zu zeigen.
pub fn signal_existing_instance() {
    if let Some(e) = show_event() {
        // SAFETY: e ist ein gueltiges Event-Handle.
        unsafe {
            let _ = SetEvent(e);
            let _ = CloseHandle(e);
        }
    }
}

/// Erste Instanz: ruft `f` auf, sobald eine zweite Instanz gestartet wurde.
pub fn spawn_show_listener(f: impl Fn() + Send + 'static) {
    let Some(e) = show_event() else { return };
    let e = SendHandle(e);
    std::thread::Builder::new()
        .name("show-listener".into())
        .spawn(move || {
            let e = e;
            // SAFETY: Handle lebt bis Prozessende.
            while unsafe { WaitForSingleObject(e.0, INFINITE) }.0 == 0 {
                f();
            }
        })
        .ok();
}

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("Campfire");

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Autostart fuer den aktuellen Benutzer ein-/ausschalten (`--background` startet im Tray).
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    // SAFETY: Registry-Aufrufe mit gueltigen Zeigern/Puffern.
    unsafe {
        let mut key = HKEY::default();
        RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_SET_VALUE, &mut key)
            .ok()
            .map_err(|e| e.to_string())?;
        let res = if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let cmd = to_wide(&format!("\"{}\" --background", exe.display()));
            let bytes = std::slice::from_raw_parts(cmd.as_ptr().cast::<u8>(), cmd.len() * 2);
            RegSetValueExW(key, RUN_VALUE, None, REG_SZ, Some(bytes)).ok()
        } else {
            let r = RegDeleteValueW(key, RUN_VALUE);
            // Fehlender Wert ist beim Ausschalten kein Fehler.
            if r.0 == 2 {
                Ok(())
            } else {
                r.ok()
            }
        };
        let _ = RegCloseKey(key);
        res.map_err(|e| e.to_string())
    }
}

pub fn autostart_enabled() -> bool {
    // SAFETY: Nur Lesen mit gueltigem Namen; kein Zielpuffer, nur Existenzpruefung.
    unsafe {
        let mut size = 0u32;
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
        .is_ok()
    }
}

/// Privater Speicher des Prozesses in MB.
pub fn private_mb() -> f64 {
    let mut c = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: c ist gross genug, cb passt.
    unsafe {
        let _ = GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut c as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            c.cb,
        );
    }
    c.PrivateUsage as f64 / 1_048_576.0
}

/// Oeffnet einen http(s)-Link im Standardbrowser.
pub fn open_url(url: &str) {
    // Nur geparste, normalisierte http(s)-Adressen ohne Userinfo an die Shell geben (N-6).
    let Some(url) = app_proto::urls::link(url) else {
        return;
    };
    let u = to_wide(&url);
    // SAFETY: gueltige, nullterminierte Zeichenketten; "open" startet den Standardbrowser.
    unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(u.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
    }
}

/// Abstand der lokalen Zeit zu UTC in Minuten (aktuelle Sommer-/Winterzeit).
pub fn utc_offset_minutes() -> i32 {
    use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let mut tz = TIME_ZONE_INFORMATION::default();
    // SAFETY: `tz` ist ein gueltiger, beschreibbarer Puffer.
    let id = unsafe { GetTimeZoneInformation(&mut tz) };
    let extra = if id == 2 {
        tz.DaylightBias
    } else {
        tz.StandardBias
    };
    -(tz.Bias + extra)
}

/// Oeffnet eine Datei mit dem Standardprogramm. Nur nach Rueckfrage bei riskanten Typen aufrufen.
pub fn open_path(path: &std::path::Path) {
    let p = to_wide(&path.to_string_lossy());
    // SAFETY: gueltige, nullterminierte Zeichenketten; "open" startet das Standardprogramm.
    unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(p.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
    }
}

/// Zeigt eine Datei im Explorer (markiert).
pub fn reveal_path(path: &std::path::Path) {
    let args = to_wide(&format!("/select,\"{}\"", path.display()));
    // SAFETY: wie oben.
    unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            w!("open"),
            w!("explorer.exe"),
            PCWSTR(args.as_ptr()),
            PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
    }
}

/// Dateien in der Zwischenablage (Explorer: Kopieren). Leer, wenn dort etwas anderes liegt.
pub fn clipboard_files() -> Vec<std::path::PathBuf> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
    const CF_HDROP: u32 = 15;
    let mut out = Vec::new();
    // SAFETY: Die Zwischenablage wird geoeffnet und am Ende geschlossen; das HDROP-Handle gehoert
    // der Zwischenablage und wird nur gelesen, solange sie offen ist.
    unsafe {
        if OpenClipboard(Some(HWND::default())).is_err() {
            return out;
        }
        if let Ok(h) = GetClipboardData(CF_HDROP) {
            let drop = HDROP(h.0);
            let n = DragQueryFileW(drop, u32::MAX, None);
            for i in 0..n {
                let len = DragQueryFileW(drop, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                let got = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
                out.push(std::path::PathBuf::from(String::from_utf16_lossy(
                    &buf[..got],
                )));
            }
        }
        let _ = CloseClipboard();
    }
    out
}

#[cfg(test)]
mod sid_tests {
    #[test]
    fn instance_objects_are_named_per_user() {
        let sid = super::user_sid().expect("SID");
        assert!(sid.starts_with("S-1-"), "{sid}");
        let name = String::from_utf16_lossy(&super::object_name("SingleInstance"));
        assert!(
            name.starts_with(r"Local\Campfire.SingleInstance.S-1-"),
            "{name}"
        );
    }
}

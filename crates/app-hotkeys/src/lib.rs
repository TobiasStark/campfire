//! Globale Hotkeys über Low-Level-Hooks (M0-04, Spike).
//!
//! * `WH_KEYBOARD_LL` und `WH_MOUSE_LL` laufen in einem eigenen Thread mit Message-Loop.
//! * Jedes Ereignis wird **immer** mit `CallNextHookEx` weitergereicht (nichts wird verschluckt).
//! * Der Callback macht nur das Nötigste (Vergleich + Channel-Send), damit Windows den Hook
//!   nicht wegen Zeitüberschreitung entfernt.

pub mod engine;

use std::{
    cell::Cell,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
        OnceLock,
    },
    thread,
    time::Instant,
};

use serde::{Deserialize, Serialize};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, LPARAM, LRESULT, WPARAM},
    Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY},
    System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{GetCurrentProcess, GetCurrentThreadId, OpenProcessToken},
    },
    UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, PostThreadMessageW, SetWindowsHookExW,
        TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT,
        WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_QUIT,
        WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    },
};

/// Eine belegbare Taste oder Maustaste (linke/rechte Maustaste sind nicht belegbar).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Trigger {
    /// Virtual-Key-Code (z. B. `0x77` = F8).
    Key(u32),
    /// Mausrad-Klick.
    Mouse3,
    /// Seitentaste 1 ("Mouse4", zurueck).
    Mouse4,
    /// Seitentaste 2 ("Mouse5", vor).
    Mouse5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotkeyEvent {
    pub trigger: Trigger,
    pub pressed: bool,
}

/// Statistik ueber die Callback-Laufzeit (muss < 1 ms bleiben).
static MAX_CALLBACK_NS: AtomicU64 = AtomicU64::new(0);
static SENDER: OnceLock<Sender<HotkeyEvent>> = OnceLock::new();

thread_local! {
    // Tastatur: ist die Taste gerade gedrueckt? Unterdrueckt Auto-Repeat.
    static KEYS_DOWN: Cell<[u64; 4]> = const { Cell::new([0; 4]) };
    // Maus: Bit 0 = Mouse3, 1 = Mouse4, 2 = Mouse5.
    static MOUSE_DOWN: Cell<u8> = const { Cell::new(0) };
}

const XBUTTON1: u32 = 1;

fn send(trigger: Trigger, pressed: bool) {
    if let Some(tx) = SENDER.get() {
        let _ = tx.send(HotkeyEvent { trigger, pressed });
    }
}

fn key_event(vk: u32, pressed: bool) {
    let (w, b) = ((vk as usize >> 6) & 3, vk & 63);
    let changed = KEYS_DOWN.with(|k| {
        let mut a = k.get();
        let was = a[w] >> b & 1 == 1;
        if was == pressed {
            return false;
        }
        a[w] ^= 1 << b;
        k.set(a);
        true
    });
    if changed {
        send(Trigger::Key(vk), pressed);
    }
}

fn mouse_event(bit: u8, trigger: Trigger, pressed: bool) {
    let changed = MOUSE_DOWN.with(|m| {
        let was = m.get() >> bit & 1 == 1;
        if was == pressed {
            return false;
        }
        m.set(m.get() ^ (1 << bit));
        true
    });
    if changed {
        send(trigger, pressed);
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let t0 = Instant::now();
    if code >= 0 {
        // SAFETY: Bei code >= 0 zeigt lparam laut Doku auf eine gültige KBDLLHOOKSTRUCT.
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let msg = wparam.0 as u32;
        match msg {
            WM_KEYDOWN | WM_SYSKEYDOWN => key_event(info.vkCode, true),
            WM_KEYUP | WM_SYSKEYUP => key_event(info.vkCode, false),
            _ => {}
        }
    }
    record(t0);
    // SAFETY: Reicht das Ereignis unverändert weiter.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let t0 = Instant::now();
    if code >= 0 {
        let msg = wparam.0 as u32;
        match msg {
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                // SAFETY: Bei Mausnachrichten zeigt lparam auf eine gueltige MSLLHOOKSTRUCT.
                let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
                let which = (info.mouseData >> 16) & 0xFFFF;
                let down = msg == WM_XBUTTONDOWN;
                if which == XBUTTON1 {
                    mouse_event(1, Trigger::Mouse4, down);
                } else {
                    mouse_event(2, Trigger::Mouse5, down);
                }
            }
            WM_MBUTTONDOWN | WM_MBUTTONUP => {
                mouse_event(0, Trigger::Mouse3, msg == WM_MBUTTONDOWN);
            }
            _ => {}
        }
    }
    record(t0);
    // SAFETY: Reicht das Ereignis unverändert weiter.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn record(t0: Instant) {
    let ns = t0.elapsed().as_nanos() as u64;
    MAX_CALLBACK_NS.fetch_max(ns, Ordering::Relaxed);
}

/// Längste bisher gemessene Callback-Laufzeit in Mikrosekunden.
pub fn max_callback_micros() -> u64 {
    MAX_CALLBACK_NS.load(Ordering::Relaxed) / 1000
}

/// Läuft der aktuelle Prozess mit Administratorrechten (erhöht)?
pub fn is_elevated() -> bool {
    // SAFETY: Standard-Token-Abfrage für den eigenen Prozess; Handle wird wieder geschlossen.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elev = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elev as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elev.TokenIsElevated != 0
    }
}

/// Laufender Hook-Thread. Beim Drop werden die Hooks entfernt.
pub struct HotkeyService {
    thread_id: u32,
    join: Option<thread::JoinHandle<()>>,
}

impl HotkeyService {
    /// Startet den Hook-Thread; liefert alle (entprellten) Tasten- und Maustastenereignisse.
    /// Es kann pro Prozess nur einen Service geben.
    pub fn start() -> Result<(Self, Receiver<HotkeyEvent>), String> {
        let (tx, rx) = mpsc::channel();
        SENDER
            .set(tx)
            .map_err(|_| "HotkeyService is already running".to_string())?;
        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();

        let join = thread::Builder::new()
            .name("hotkey-hooks".into())
            .spawn(move || {
                // SAFETY: Hooks werden in diesem Thread installiert und entfernt; die Message-Loop
                // unten hält sie am Leben.
                unsafe {
                    let hmod = GetModuleHandleW(None).ok().map(|h| h.into());
                    let kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hmod, 0);
                    let ms = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hmod, 0);
                    match (&kb, &ms) {
                        (Ok(_), Ok(_)) => {
                            let _ = ready_tx.send(Ok(GetCurrentThreadId()));
                        }
                        _ => {
                            let _ = ready_tx.send(Err(format!("Hook-Installation: {kb:?} {ms:?}")));
                        }
                    }
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    if let Ok(h) = kb {
                        let _ = UnhookWindowsHookEx(h);
                    }
                    if let Ok(h) = ms {
                        let _ = UnhookWindowsHookEx(h);
                    }
                }
            })
            .map_err(|e| e.to_string())?;

        let thread_id = ready_rx.recv().map_err(|e| e.to_string())??;
        Ok((
            Self {
                thread_id,
                join: Some(join),
            },
            rx,
        ))
    }
}

impl Drop for HotkeyService {
    fn drop(&mut self) {
        // SAFETY: Beendet die Message-Loop des Hook-Threads.
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_work_per_event_is_far_below_one_millisecond() {
        let (tx, rx) = mpsc::channel();
        let _ = SENDER.set(tx);
        let mut worst = 0u128;
        for i in 0..20_000u32 {
            let t0 = Instant::now();
            key_event(0x41 + (i % 26), i % 2 == 0);
            mouse_event(1, Trigger::Mouse4, i % 3 == 0);
            worst = worst.max(t0.elapsed().as_micros());
        }
        assert!(worst < 1000, "Ereignis dauerte {worst} us");
        drop(rx);
    }

    #[test]
    fn auto_repeat_is_filtered() {
        // Im Test-Thread frischer Zustand; Wiederholungen derselben Richtung senden nichts.
        std::thread::spawn(|| {
            let (tx, rx) = mpsc::channel();
            // SENDER ist global; hier nur die Entprellung pruefen.
            let _ = SENDER.set(tx);
            KEYS_DOWN.with(|k| k.set([0; 4]));
            let before = rx.try_iter().count();
            let _ = before;
            key_event(0x77, true);
            let first = KEYS_DOWN.with(Cell::get);
            key_event(0x77, true);
            assert_eq!(KEYS_DOWN.with(Cell::get), first);
            key_event(0x77, false);
            assert_eq!(KEYS_DOWN.with(Cell::get), [0; 4]);
        })
        .join()
        .unwrap();
    }
}

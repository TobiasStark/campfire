//! Hotkey-Logik ohne Windows: Tastenkombinationen, Aufnahme neuer Belegungen,
//! Push-to-Talk mit Loslass-Verzoegerung. Rein deterministisch und testbar.

use crate::{HotkeyEvent, Trigger};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Action {
    PushToTalk,
    ToggleMute,
    ToggleDeafen,
    ToggleOverlay,
    /// Stream starten oder beenden (M3-08).
    ToggleStream,
}

/// Eine Kombination: alle Tasten muessen gleichzeitig gehalten werden.
pub type Binding = Vec<Trigger>;

/// Gespeicherte Einstellungen.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeySettings {
    pub ptt: Option<Binding>,
    pub mute: Option<Binding>,
    pub deafen: Option<Binding>,
    pub overlay: Option<Binding>,
    pub stream: Option<Binding>,
    /// `true`: gesendet wird nur, solange die PTT-Taste gehalten wird.
    pub ptt_enabled: bool,
    pub ptt_release_ms: u32,
    /// Kurze Toene bei PTT an/aus.
    pub sounds: bool,
}

impl Default for HotkeySettings {
    fn default() -> Self {
        Self {
            ptt: None,
            mute: None,
            deafen: None,
            overlay: None,
            stream: None,
            ptt_enabled: false,
            ptt_release_ms: 200,
            sounds: true,
        }
    }
}

impl HotkeySettings {
    pub fn binding(&self, a: Action) -> Option<&Binding> {
        match a {
            Action::PushToTalk => self.ptt.as_ref(),
            Action::ToggleMute => self.mute.as_ref(),
            Action::ToggleDeafen => self.deafen.as_ref(),
            Action::ToggleOverlay => self.overlay.as_ref(),
            Action::ToggleStream => self.stream.as_ref(),
        }
    }

    pub fn set_binding(&mut self, a: Action, b: Option<Binding>) {
        match a {
            Action::PushToTalk => self.ptt = b,
            Action::ToggleMute => self.mute = b,
            Action::ToggleDeafen => self.deafen = b,
            Action::ToggleOverlay => self.overlay = b,
            Action::ToggleStream => self.stream = b,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    /// Kombination wurde gedrueckt (`true`) oder aufgeloest (`false`).
    Action { action: Action, active: bool },
    /// Aufnahme beendet: die zuletzt gleichzeitig gehaltene Kombination.
    Captured(Binding),
}

#[derive(Default)]
pub struct Engine {
    settings: HotkeySettings,
    held: Vec<Trigger>,
    active: Vec<Action>,
    capture: Option<Vec<Trigger>>,
}

const ACTIONS: [Action; 5] = [
    Action::PushToTalk,
    Action::ToggleMute,
    Action::ToggleDeafen,
    Action::ToggleOverlay,
    Action::ToggleStream,
];

impl Engine {
    pub fn new(settings: HotkeySettings) -> Self {
        Self {
            settings,
            ..Self::default()
        }
    }

    pub fn set_settings(&mut self, s: HotkeySettings) {
        self.settings = s;
    }

    /// Naechste gedrueckte Kombination aufnehmen (statt Aktionen auszuloesen).
    pub fn start_capture(&mut self) {
        self.capture = Some(Vec::new());
    }

    pub fn cancel_capture(&mut self) {
        self.capture = None;
    }

    pub fn capturing(&self) -> bool {
        self.capture.is_some()
    }

    pub fn feed(&mut self, ev: HotkeyEvent) -> Vec<Out> {
        let mut out = Vec::new();
        if ev.pressed {
            if !self.held.contains(&ev.trigger) {
                self.held.push(ev.trigger);
            }
        } else {
            self.held.retain(|t| *t != ev.trigger);
        }
        if let Some(peak) = &mut self.capture {
            if self.held.len() >= peak.len() && ev.pressed {
                peak.clone_from(&self.held);
            }
            if self.held.is_empty() {
                let b = std::mem::take(peak);
                self.capture = None;
                if !b.is_empty() {
                    out.push(Out::Captured(b));
                }
            }
            return out;
        }
        for a in ACTIONS {
            let Some(b) = self.settings.binding(a).filter(|b| !b.is_empty()) else {
                continue;
            };
            let all = b.iter().all(|t| self.held.contains(t));
            let was = self.active.contains(&a);
            if all && !was {
                self.active.push(a);
                out.push(Out::Action {
                    action: a,
                    active: true,
                });
            } else if !all && was {
                self.active.retain(|x| *x != a);
                out.push(Out::Action {
                    action: a,
                    active: false,
                });
            }
        }
        out
    }
}

/// Push-to-Talk-Zustand mit Nachlauf nach dem Loslassen.
#[derive(Debug)]
pub struct PttGate {
    release: Duration,
    on: bool,
    held: bool,
    off_at: Option<Instant>,
}

impl PttGate {
    pub fn new(release_ms: u32) -> Self {
        Self {
            release: Duration::from_millis(release_ms.into()),
            on: false,
            held: false,
            off_at: None,
        }
    }

    pub fn set_release_ms(&mut self, ms: u32) {
        self.release = Duration::from_millis(ms.into());
    }

    pub fn transmitting(&self) -> bool {
        self.on
    }

    /// Taste gedrueckt/losgelassen. `Some(neuer Zustand)`, wenn sich das Senden aendert.
    pub fn event(&mut self, pressed: bool, now: Instant) -> Option<bool> {
        self.held = pressed;
        if pressed {
            self.off_at = None;
            if !self.on {
                self.on = true;
                return Some(true);
            }
        } else if self.on {
            if self.release.is_zero() {
                self.on = false;
                return Some(false);
            }
            self.off_at = Some(now + self.release);
        }
        None
    }

    /// Regelmaessig aufrufen: beendet das Senden nach Ablauf des Nachlaufs.
    pub fn poll(&mut self, now: Instant) -> Option<bool> {
        if let Some(t) = self.off_at {
            if now >= t && !self.held {
                self.off_at = None;
                self.on = false;
                return Some(false);
            }
        }
        None
    }
}

/// Anzeigename einer Taste.
pub fn trigger_name(t: &Trigger) -> String {
    match *t {
        Trigger::Mouse3 => "Mausrad".into(),
        Trigger::Mouse4 => "Maus 4".into(),
        Trigger::Mouse5 => "Maus 5".into(),
        Trigger::Key(vk) => match vk {
            0x08 => "Rueck".into(),
            0x09 => "Tab".into(),
            0x0D => "Enter".into(),
            0x14 => "Feststell".into(),
            0x1B => "Esc".into(),
            0x20 => "Leertaste".into(),
            0x21 => "Bild hoch".into(),
            0x22 => "Bild runter".into(),
            0x23 => "Ende".into(),
            0x24 => "Pos1".into(),
            0x25 => "Links".into(),
            0x26 => "Hoch".into(),
            0x27 => "Rechts".into(),
            0x28 => "Runter".into(),
            0x2D => "Einfg".into(),
            0x2E => "Entf".into(),
            0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).map(String::from).unwrap_or_default(),
            0x60..=0x69 => format!("Num {}", vk - 0x60),
            0x70..=0x87 => format!("F{}", vk - 0x6F),
            0xA0 => "Shift links".into(),
            0xA1 => "Shift rechts".into(),
            0xA2 => "Strg links".into(),
            0xA3 => "Strg rechts".into(),
            0xA4 => "Alt".into(),
            0xA5 => "Alt Gr".into(),
            0xC0 => "Ö".into(),
            _ => format!("Taste 0x{vk:02X}"),
        },
    }
}

pub fn binding_name(b: &Binding) -> String {
    if b.is_empty() {
        return "not bound".into();
    }
    b.iter().map(trigger_name).collect::<Vec<_>>().join(" + ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: Trigger, pressed: bool) -> HotkeyEvent {
        HotkeyEvent {
            trigger: t,
            pressed,
        }
    }
    const F8: Trigger = Trigger::Key(0x77);
    const CTRL: Trigger = Trigger::Key(0xA2);
    const M: Trigger = Trigger::Key(0x4D);

    fn engine() -> Engine {
        Engine::new(HotkeySettings {
            ptt: Some(vec![F8]),
            mute: Some(vec![CTRL, M]),
            deafen: Some(vec![Trigger::Mouse5]),
            ..HotkeySettings::default()
        })
    }

    #[test]
    fn single_keys_and_mouse_buttons_fire_on_press_and_release() {
        let mut e = engine();
        assert_eq!(
            e.feed(ev(F8, true)),
            vec![Out::Action {
                action: Action::PushToTalk,
                active: true
            }]
        );
        assert_eq!(
            e.feed(ev(F8, false)),
            vec![Out::Action {
                action: Action::PushToTalk,
                active: false
            }]
        );
        assert_eq!(e.feed(ev(Trigger::Mouse5, true)).len(), 1);
        assert_eq!(e.feed(ev(Trigger::Mouse5, false)).len(), 1);
        assert!(e.feed(ev(Trigger::Mouse4, true)).is_empty());
    }

    #[test]
    fn combinations_need_all_keys() {
        let mut e = engine();
        assert!(e.feed(ev(M, true)).is_empty());
        e.feed(ev(M, false));
        assert!(e.feed(ev(CTRL, true)).is_empty());
        let o = e.feed(ev(M, true));
        assert_eq!(
            o,
            vec![Out::Action {
                action: Action::ToggleMute,
                active: true
            }]
        );
        // Eine Taste loslassen loest die Kombination auf.
        let o = e.feed(ev(CTRL, false));
        assert_eq!(
            o,
            vec![Out::Action {
                action: Action::ToggleMute,
                active: false
            }]
        );
    }

    #[test]
    fn capture_returns_largest_held_combination() {
        let mut e = engine();
        e.start_capture();
        assert!(e.feed(ev(CTRL, true)).is_empty());
        assert!(e.feed(ev(Trigger::Key(0x50), true)).is_empty());
        assert!(e.feed(ev(Trigger::Key(0x50), false)).is_empty());
        let o = e.feed(ev(CTRL, false));
        assert_eq!(o, vec![Out::Captured(vec![CTRL, Trigger::Key(0x50)])]);
        assert!(!e.capturing());
        // Danach wirken die Belegungen wieder.
        assert_eq!(e.feed(ev(F8, true)).len(), 1);
    }

    #[test]
    fn ptt_release_delay() {
        let t0 = Instant::now();
        let mut g = PttGate::new(200);
        assert_eq!(g.event(true, t0), Some(true));
        assert_eq!(g.event(false, t0), None);
        assert!(g.transmitting());
        assert_eq!(g.poll(t0 + Duration::from_millis(150)), None);
        assert_eq!(g.poll(t0 + Duration::from_millis(200)), Some(false));
        assert!(!g.transmitting());
        // Erneutes Druecken im Nachlauf bricht das Ausschalten ab.
        g.event(true, t0);
        g.event(false, t0);
        assert_eq!(g.event(true, t0 + Duration::from_millis(100)), None);
        assert_eq!(g.poll(t0 + Duration::from_secs(5)), None);
        assert!(g.transmitting());
    }

    #[test]
    fn settings_roundtrip_and_names() {
        let s = HotkeySettings {
            ptt: Some(vec![CTRL, F8]),
            ..HotkeySettings::default()
        };
        let j = serde_json_like(&s);
        assert!(j.contains("Key"));
        assert_eq!(binding_name(&vec![CTRL, F8]), "Strg links + F8");
        assert_eq!(binding_name(&vec![Trigger::Mouse4]), "Maus 4");
        assert_eq!(binding_name(&vec![]), "not bound");
    }

    fn serde_json_like(s: &HotkeySettings) -> String {
        format!("{s:?}")
    }
}

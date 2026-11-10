//! Globale Hotkeys in der App: Push-to-Talk, Stumm, Taub (M1-12).
//!
//! Ein eigener Thread wertet die Hook-Ereignisse aus. PTT wirkt direkt auf den Voice-Kern
//! (ohne Umweg ueber die Oberflaeche); Stumm/Taub werden als Signal an die UI gemeldet.

use crate::signals::{send, Signal};
use crate::sounds::Sounds;
use app_audio::sfx::Sound;
use app_audio::voice::VoiceCore;
use app_core::storage::Db;
use app_hotkeys::engine::{Action, Engine, HotkeySettings, Out, PttGate};
use app_hotkeys::HotkeyService;
use app_proto::lock::Locked;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Ist gerade Voice aktiv (Channel oder Anruf)? Nur dann sind die Hooks installiert.
pub type VoiceActive = Arc<dyn Fn() -> bool + Send + Sync>;

struct Shared {
    voice_active: Option<VoiceActive>,
    settings: HotkeySettings,
    engine: Engine,
    gate: PttGate,
    capture_for: Option<Action>,
    available: bool,
}

#[derive(Clone)]
pub struct Hotkeys {
    shared: Arc<Mutex<Shared>>,
    voice: Arc<Mutex<VoiceCore>>,
    sounds: Sounds,
    db: Db,
}

impl Hotkeys {
    pub fn start(db: &Db, voice: Arc<Mutex<VoiceCore>>, sounds: Sounds) -> Self {
        let settings: HotkeySettings = db
            .call(|c| app_core::settings::get(c, app_core::settings::HOTKEYS))
            .ok()
            .flatten()
            .unwrap_or_default();
        let shared = Arc::new(Mutex::new(Shared {
            engine: Engine::new(settings.clone()),
            gate: PttGate::new(settings.ptt_release_ms),
            settings,
            capture_for: None,
            available: false,
            voice_active: None,
        }));
        let hk = Self {
            shared: shared.clone(),
            voice,
            sounds,
            db: db.clone(),
        };
        hk.apply_transmit();
        match HotkeyService::start() {
            Ok((svc, rx)) => {
                shared.locked().available = true;
                let hk2 = hk.clone();
                let _ = std::thread::Builder::new()
                    .name("hotkey-logic".into())
                    .spawn(move || {
                        let svc = svc; // haelt den Hook-Thread, solange dieser Thread lebt
                        let mut hooked = false;
                        let mut checked = Instant::now() - Duration::from_secs(1);
                        loop {
                            match rx.recv_timeout(Duration::from_millis(10)) {
                                Ok(ev) => hk2.handle(ev),
                                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                                Err(_) => break,
                            }
                            hk2.poll();
                            if checked.elapsed() >= Duration::from_millis(100) {
                                checked = Instant::now();
                                let want = hk2.hooks_wanted();
                                if want != hooked {
                                    hooked = want;
                                    svc.set_hooks(want);
                                    if !want {
                                        hk2.release_all();
                                    }
                                }
                            }
                        }
                    });
            }
            Err(e) => tracing::warn!("Hotkeys nicht verfuegbar: {e}"),
        }
        hk
    }

    /// Woran die Hooks erkennen, ob Voice aktiv ist (wird nach dem Voice-Dienst gesetzt).
    pub fn watch_voice(&self, f: VoiceActive) {
        self.shared.locked().voice_active = Some(f);
    }

    /// Hooks nur, waehrend eine Taste aufgenommen wird oder Voice aktiv ist und mindestens
    /// eine Taste belegt ist (I-6).
    fn hooks_wanted(&self) -> bool {
        let (capturing, bound, probe) = {
            let s = self.shared.locked();
            let st = &s.settings;
            let bound = st.ptt.is_some()
                || st.mute.is_some()
                || st.deafen.is_some()
                || st.overlay.is_some()
                || st.stream.is_some();
            (s.capture_for.is_some(), bound, s.voice_active.clone())
        };
        capturing || (bound && probe.is_some_and(|f| f()))
    }

    /// Nach dem Entfernen der Hooks kommen keine Loslass-Ereignisse mehr: alles zuruecksetzen.
    fn release_all(&self) {
        {
            let mut s = self.shared.locked();
            let settings = s.settings.clone();
            s.engine = Engine::new(settings.clone());
            s.gate = PttGate::new(settings.ptt_release_ms);
        }
        self.apply_transmit();
    }

    /// Senden erlaubt? Ohne PTT immer, mit PTT nur waehrend die Taste gilt.
    fn apply_transmit(&self) {
        let t = {
            let s = self.shared.locked();
            !s.settings.ptt_enabled || s.gate.transmitting()
        };
        self.voice.locked().set_transmitting(t);
    }

    fn ptt_changed(&self, change: Option<bool>, enabled: bool) {
        if let (Some(on), true) = (change, enabled) {
            self.voice.locked().set_transmitting(on);
            self.sounds
                .play(if on { Sound::PttOn } else { Sound::PttOff });
        }
    }

    fn handle(&self, ev: app_hotkeys::HotkeyEvent) {
        let outs = self.shared.locked().engine.feed(ev);
        for o in outs {
            match o {
                Out::Action {
                    action: Action::PushToTalk,
                    active,
                } => {
                    let (change, enabled) = {
                        let mut s = self.shared.locked();
                        (s.gate.event(active, Instant::now()), s.settings.ptt_enabled)
                    };
                    self.ptt_changed(change, enabled);
                }
                Out::Action { action, active } => {
                    if active {
                        send(Signal::Hotkey(action));
                    }
                }
                Out::Captured(b) => {
                    let target = self.shared.locked().capture_for.take();
                    if let Some(a) = target {
                        self.update(|st| st.set_binding(a, Some(b)));
                    }
                    send(Signal::HotkeysChanged);
                }
            }
        }
    }

    fn poll(&self) {
        let (change, enabled) = {
            let mut s = self.shared.locked();
            (s.gate.poll(Instant::now()), s.settings.ptt_enabled)
        };
        self.ptt_changed(change, enabled);
    }

    pub fn available(&self) -> bool {
        self.shared.locked().available
    }

    pub fn settings(&self) -> HotkeySettings {
        self.shared.locked().settings.clone()
    }

    /// Einstellungen aendern, sofort anwenden und speichern.
    pub fn update(&self, f: impl FnOnce(&mut HotkeySettings)) {
        let new = {
            let mut s = self.shared.locked();
            let mut n = s.settings.clone();
            f(&mut n);
            s.engine.set_settings(n.clone());
            s.gate.set_release_ms(n.ptt_release_ms);
            s.settings = n.clone();
            n
        };
        self.apply_transmit();
        let _ = self.db.spawn(
            move |c| app_core::settings::set(c, app_core::settings::HOTKEYS, &new),
            |_| {},
        );
    }

    /// Naechste gedrueckte Kombination fuer `a` uebernehmen.
    pub fn capture(&self, a: Action) {
        let mut s = self.shared.locked();
        s.capture_for = Some(a);
        s.engine.start_capture();
    }

    pub fn cancel_capture(&self) {
        let mut s = self.shared.locked();
        s.capture_for = None;
        s.engine.cancel_capture();
    }

    pub fn capturing(&self) -> Option<Action> {
        self.shared.locked().capture_for
    }
}

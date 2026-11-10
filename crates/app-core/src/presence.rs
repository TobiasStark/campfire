//! Presence: Online-Status der Freunde ueber den Control-Stream.
//!
//! * Jede Seite sendet ihren Zustand beim Verbinden und bei jeder Aenderung.
//! * "Unsichtbar" wird gesendet, die Gegenseite zeigt dafuer "offline".
//! * Bricht ein Peer hart ab (Absturz), erkennt das QUIC-Idle-Timeout (6 s) und
//!   der Eintrag verschwindet mit `PeerDown`.
//! * Den Voice-Channel (und ob gestreamt wird) erfaehrt nur, wer Mitglied des
//!   Spaces ist; alle anderen Freunde sehen nur den Status.

use app_net::{Net, NetEvent};
use app_proto::control::{ControlMessage, Presence, PresenceStatus};
use app_proto::event::SpaceId;
use app_proto::lock::Locked;
use app_proto::PublicKey;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Standard: nach 10 Minuten ohne Eingabe gilt man als abwesend.
pub const DEFAULT_AWAY_AFTER: Duration = Duration::from_secs(10 * 60);

/// Prueft, ob ein Nutzer Mitglied eines Spaces ist.
pub type MemberCheck = Arc<dyn Fn(&SpaceId, &PublicKey) -> bool + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenceEvent {
    /// Der sichtbare Zustand dieses Nutzers hat sich geaendert (auch: offline gegangen).
    Changed { user: PublicKey },
}

struct Remote {
    user: PublicKey,
    presence: Option<Presence>,
    /// Reihenfolge der letzten Aktualisierung (bei mehreren Geraeten gewinnt das neueste).
    order: u64,
}

struct Inner {
    net: Net,
    local: Mutex<Presence>,
    remote: Mutex<HashMap<PublicKey, Remote>>,
    counter: Mutex<u64>,
    events: mpsc::UnboundedSender<PresenceEvent>,
    is_member: MemberCheck,
}

#[derive(Clone)]
pub struct PresenceService {
    inner: Arc<Inner>,
}

impl PresenceService {
    pub fn new(
        net: Net,
        initial: Presence,
        is_member: MemberCheck,
    ) -> (Self, mpsc::UnboundedReceiver<PresenceEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let svc = Self {
            inner: Arc::new(Inner {
                net,
                local: Mutex::new(initial),
                remote: Mutex::default(),
                counter: Mutex::new(0),
                events: tx,
                is_member,
            }),
        };
        (svc, rx)
    }

    /// Vom Ereignis-Verteiler der App fuer jedes `NetEvent` aufrufen.
    pub fn on_net_event(&self, ev: &NetEvent) {
        match ev {
            NetEvent::PeerUp(info) => {
                let order = self.next_order();
                let before = self.visible(&info.user);
                self.inner
                    .remote
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        info.device,
                        Remote {
                            user: info.user,
                            presence: None,
                            order,
                        },
                    );
                self.notify_if_changed(info.user, before);
                self.send_to(info.device);
            }
            NetEvent::PeerDown { device } => {
                let user = self.inner.remote.locked().get(device).map(|r| r.user);
                if let Some(user) = user {
                    let before = self.visible(&user);
                    self.inner
                        .remote
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(device);
                    self.notify_if_changed(user, before);
                }
            }
            NetEvent::Control {
                from,
                msg: ControlMessage::Presence(p),
            } => {
                let order = self.next_order();
                let before = self.visible(&from.user);
                if let Some(r) = self
                    .inner
                    .remote
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_mut(&from.device)
                {
                    r.presence = Some(p.clone());
                    r.order = order;
                }
                self.notify_if_changed(from.user, before);
            }
            _ => {}
        }
    }

    fn next_order(&self) -> u64 {
        let mut c = self
            .inner
            .counter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *c += 1;
        *c
    }

    /// Sichtbarer Zustand eines Nutzers: `None` = offline (auch bei "unsichtbar").
    pub fn visible(&self, user: &PublicKey) -> Option<Presence> {
        let remote = self
            .inner
            .remote
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        remote
            .values()
            .filter(|r| r.user == *user)
            .filter_map(|r| r.presence.as_ref().map(|p| (r.order, p)))
            .filter(|(_, p)| p.status != PresenceStatus::Invisible)
            .max_by_key(|(order, _)| *order)
            .map(|(_, p)| p.clone())
    }

    /// Alle Nutzer, die gerade sichtbar online sind.
    pub fn online_users(&self) -> Vec<(PublicKey, Presence)> {
        let users: Vec<PublicKey> = {
            let remote = self
                .inner
                .remote
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut u: Vec<_> = remote.values().map(|r| r.user).collect();
            u.sort();
            u.dedup();
            u
        };
        users
            .into_iter()
            .filter_map(|u| self.visible(&u).map(|p| (u, p)))
            .collect()
    }

    fn notify_if_changed(&self, user: PublicKey, before: Option<Presence>) {
        if self.visible(&user) != before {
            let _ = self.inner.events.send(PresenceEvent::Changed { user });
        }
    }

    pub fn local(&self) -> Presence {
        self.inner
            .local
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Setzt den eigenen Zustand und meldet ihn allen Verbundenen (nur bei Aenderung).
    pub fn set_local(&self, p: Presence) {
        {
            let mut l = self
                .inner
                .local
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *l == p {
                return;
            }
            *l = p;
        }
        self.resend();
    }

    /// Der eigene Zustand, wie ihn `user` sehen darf: Voice-Channel und Stream nur
    /// fuer Mitglieder des Spaces.
    fn for_recipient(&self, user: &PublicKey) -> Presence {
        let mut p = self.local();
        if let Some((space, _)) = p.voice {
            if !(self.inner.is_member)(&space, user) {
                p.voice = None;
                p.streaming = false;
            }
        }
        p
    }

    /// Sendet den eigenen Zustand allen Verbundenen erneut, z. B. weil sich Mitgliedschaften
    /// geaendert haben und damit, wer den Voice-Channel sehen darf.
    pub fn resend(&self) {
        let devices: Vec<PublicKey> = self
            .inner
            .remote
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied()
            .collect();
        for d in devices {
            self.send_to(d);
        }
    }

    fn send_to(&self, device: PublicKey) {
        let net = self.inner.net.clone();
        let user = self
            .inner
            .remote
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&device)
            .map(|r| r.user);
        let Some(user) = user else { return };
        let msg = ControlMessage::Presence(self.for_recipient(&user));
        tokio::spawn(async move {
            let _ = net.send_control(&device, msg).await;
        });
    }
}

/// Wandelt den vom Nutzer gewaehlten Status in den gesendeten um: nur "online" wird
/// bei Inaktivitaet zu "abwesend"; "bitte nicht stoeren" und "unsichtbar" bleiben.
pub fn effective_status(
    chosen: PresenceStatus,
    idle: Duration,
    away_after: Duration,
) -> PresenceStatus {
    match chosen {
        PresenceStatus::Online if idle >= away_after => PresenceStatus::Away,
        other => other,
    }
}

/// Verwaltet gewaehlten Status, Voice-/Stream-Anzeige und Auto-Abwesend.
pub struct LocalState {
    svc: PresenceService,
    chosen: Mutex<Presence>,
    away_after: Duration,
}

impl LocalState {
    pub fn new(svc: PresenceService, chosen: Presence, away_after: Duration) -> Self {
        Self {
            svc,
            chosen: Mutex::new(chosen),
            away_after,
        }
    }

    /// Nutzer-Einstellung (Status, Voice-Channel, Stream) aendern.
    pub fn update(&self, f: impl FnOnce(&mut Presence), idle: Duration) {
        f(&mut self
            .chosen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner));
        self.apply(idle);
    }

    /// Regelmaessig mit der aktuellen Leerlaufzeit aufrufen.
    pub fn apply(&self, idle: Duration) {
        let mut p = self
            .chosen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        p.status = effective_status(p.status, idle, self.away_after);
        self.svc.set_local(p);
    }

    /// Hintergrundaufgabe: fragt `idle_source` (z. B. [`idle_time`]) im Abstand `poll` ab.
    pub fn spawn_auto_away(
        self: &Arc<Self>,
        poll: Duration,
        idle_source: impl Fn() -> Duration + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(poll);
            loop {
                t.tick().await;
                me.apply(idle_source());
            }
        })
    }
}

/// Zeit seit der letzten Tastatur-/Mauseingabe des Benutzers (`GetLastInputInfo`).
#[cfg(windows)]
pub fn idle_time() -> Duration {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: info ist korrekt initialisiert (cbSize gesetzt) und lebt waehrend des Aufrufs.
    unsafe {
        if !GetLastInputInfo(&mut info).as_bool() {
            return Duration::ZERO;
        }
        Duration::from_millis(u64::from(GetTickCount().wrapping_sub(info.dwTime)))
    }
}

#[cfg(test)]
mod tests;

//! Voice-Sitzung: Join/Leave per Control-Nachricht, Teilnehmerliste, Datagramm-Verteilung.
//!
//! Wer im Channel ist, wissen nur die Teilnehmer selbst (`VoiceSignal::Join`). Audio geht
//! ausschliesslich an Geraete, die selbst Join gesendet haben und Mitglied des Spaces sind;
//! empfangen wird nur von diesen.

use crate::voice::VoiceCore;
use app_net::{Net, NetEvent};
use app_proto::control::{ControlMessage, VoiceSignal};
use app_proto::event::{ChannelId, SpaceId};
use app_proto::lock::Locked;
use app_proto::PublicKey;
use bytes::Bytes;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Platzhalter-Space fuer Direktanrufe (kein echter Space hat diese ID).
pub const CALL_SPACE: SpaceId = [0; 16];
/// So lange klingelt es, bevor der Anruf als unbeantwortet gilt.
pub const RING_TIMEOUT: Duration = Duration::from_secs(30);
/// So lange klingelt es bei uns nicht erneut, nachdem wir einen Anruf desselben Freundes
/// abgelehnt oder verpasst haben (gegen Anruf-Spam).
pub const CALL_COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallState {
    Idle,
    /// Wir rufen an und warten auf Antwort.
    Outgoing {
        user: PublicKey,
    },
    /// Jemand ruft uns an.
    Incoming {
        user: PublicKey,
    },
    /// Gespraech laeuft.
    Active {
        user: PublicKey,
    },
}

struct Outgoing {
    call_id: ChannelId,
    user: PublicKey,
    devices: Vec<PublicKey>,
}

struct Incoming {
    call_id: ChannelId,
    user: PublicKey,
    device: PublicKey,
}

/// `true`, wenn `user` Mitglied von `space` ist (aus der Projektion des Event-Logs).
pub type MemberCheck = Arc<dyn Fn(&SpaceId, &PublicKey) -> bool + Send + Sync>;

/// `true`, wenn `user` ein angenommener Freund ist. Nur Freunde duerfen uns anrufen.
pub type FriendCheck = Arc<dyn Fn(&PublicKey) -> bool + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    pub user: PublicKey,
    pub device: PublicKey,
    pub muted: bool,
    pub deafened: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceEvent {
    /// Teilnehmerliste oder Zustand eines Teilnehmers hat sich geaendert.
    Roster,
    /// Anrufzustand hat sich geaendert (klingelt, angenommen, beendet ...).
    Call,
}

#[derive(Default)]
struct State {
    current: Option<(SpaceId, ChannelId)>,
    roster: HashMap<PublicKey, Participant>,
    /// Geraete, denen wir in dieser Sitzung schon Join gesendet haben.
    told: HashSet<PublicKey>,
    muted: bool,
    deafened: bool,
    outgoing: Option<Outgoing>,
    incoming: Option<Incoming>,
    /// Gesetzt, solange ein Direktanruf laeuft: nur dieser Nutzer darf teilnehmen.
    call_peer: Option<PublicKey>,
    /// Der Gespraechspartner war schon im Gespraech (dann beendet sein Weggang den Anruf).
    call_seen: bool,
    note: Option<&'static str>,
    /// Seit wann wir im aktuellen Channel/Gespraech sind (fuer die Anzeige der Dauer).
    since: Option<std::time::Instant>,
    /// Bis wann Einladungen dieses Nutzers ignoriert werden (nach Ablehnen/Verpassen).
    cooldown: HashMap<PublicKey, std::time::Instant>,
}

struct Inner {
    net: Net,
    core: Arc<Mutex<VoiceCore>>,
    is_member: MemberCheck,
    is_friend: FriendCheck,
    state: Mutex<State>,
    events: mpsc::UnboundedSender<VoiceEvent>,
    ring_timeout: Duration,
    call_cooldown: Duration,
}

#[derive(Clone)]
pub struct VoiceService {
    inner: Arc<Inner>,
}

impl VoiceService {
    pub fn new(
        net: Net,
        core: Arc<Mutex<VoiceCore>>,
        is_member: MemberCheck,
        is_friend: FriendCheck,
    ) -> (Self, mpsc::UnboundedReceiver<VoiceEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                inner: Arc::new(Inner {
                    net,
                    core,
                    is_member,
                    is_friend,
                    state: Mutex::default(),
                    events: tx,
                    ring_timeout: RING_TIMEOUT,
                    call_cooldown: CALL_COOLDOWN,
                }),
            },
            rx,
        )
    }

    pub fn core(&self) -> Arc<Mutex<VoiceCore>> {
        self.inner.core.clone()
    }

    pub fn current(&self) -> Option<(SpaceId, ChannelId)> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current
    }

    /// Seit wann wir im aktuellen Voice-Channel oder Gespraech sind.
    pub fn connected_since(&self) -> Option<std::time::Instant> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .since
    }

    pub fn participants(&self) -> Vec<Participant> {
        let mut v: Vec<_> = self.inner.state.locked().roster.values().cloned().collect();
        v.sort_by_key(|p| p.device);
        v
    }

    fn notify(&self) {
        let _ = self.inner.events.send(VoiceEvent::Roster);
    }

    fn send_to(&self, device: PublicKey, msg: VoiceSignal) {
        let net = self.inner.net.clone();
        tokio::spawn(async move {
            let _ = net.send_control(&device, ControlMessage::Voice(msg)).await;
        });
    }

    /// Join (und State) an ein Geraet schicken, falls dessen Nutzer im Space ist.
    fn introduce(&self, device: PublicKey, user: PublicKey) {
        let (cur, muted, deafened) = {
            let mut s = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(cur) = s.current else { return };
            let ok = if cur.0 == CALL_SPACE {
                s.call_peer == Some(user)
            } else {
                (self.inner.is_member)(&cur.0, &user)
            };
            if !ok || !s.told.insert(device) {
                return;
            }
            (cur, s.muted, s.deafened)
        };
        self.send_to(
            device,
            VoiceSignal::Join {
                space_id: cur.0,
                channel: cur.1,
                stream_id: crate::voice::AUDIO_STREAM,
            },
        );
        self.send_to(device, VoiceSignal::State { muted, deafened });
    }

    /// Tritt einem Voice-Channel bei (ein vorheriger wird verlassen).
    pub fn join(&self, space: SpaceId, channel: ChannelId) {
        self.leave();
        self.join_inner(space, channel);
    }

    fn join_inner(&self, space: SpaceId, channel: ChannelId) {
        {
            let mut s = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.current = Some((space, channel));
            s.since = Some(std::time::Instant::now());
        }
        for p in self.inner.net.peers() {
            self.introduce(p.device, p.user);
        }
        self.notify();
    }

    pub fn leave(&self) {
        let (cur, told) = {
            let mut s = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.call_peer = None;
            s.call_seen = false;
            s.since = None;
            let Some(cur) = s.current.take() else { return };
            s.roster.clear();
            (cur, std::mem::take(&mut s.told))
        };
        for d in told {
            self.send_to(
                d,
                VoiceSignal::Leave {
                    space_id: cur.0,
                    channel: cur.1,
                },
            );
        }
        self.inner
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear_peers();
        self.notify();
    }

    /// Stumm/Taub schalten und den Teilnehmern melden.
    pub fn set_state(&self, muted: bool, deafened: bool) {
        let told: Vec<PublicKey> = {
            let mut s = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.muted = muted;
            s.deafened = deafened;
            s.told.iter().copied().collect()
        };
        {
            let mut c = self
                .inner
                .core
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            c.set_muted(muted || deafened);
            c.set_deafened(deafened);
        }
        for d in told {
            self.send_to(d, VoiceSignal::State { muted, deafened });
        }
    }

    /// Schickt ein fertiges Voice-Datagramm an alle Teilnehmer.
    pub fn send_packet(&self, data: Vec<u8>) {
        let devices: Vec<PublicKey> = {
            let s = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if s.current.is_none() {
                return;
            }
            s.roster.keys().copied().collect()
        };
        let bytes = Bytes::from(data);
        for d in devices {
            let _ = self.inner.net.send_datagram(&d, bytes.clone());
        }
    }

    /// Ein Frame vom Mikrofon (ohne Geraetetreiber, z. B. Tests): kodieren und senden.
    pub fn submit_frame(&self, pcm: &[f32]) {
        let pkt = self
            .inner
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .encode_frame(pcm);
        if let Some(p) = pkt {
            self.send_packet(p);
        }
    }

    /// Netzwerk-Ereignisse verarbeiten (Aufruf aus dem Ereignisverteiler der App).
    pub fn on_net_event(&self, ev: &NetEvent) {
        match ev {
            NetEvent::PeerUp(p) => self.introduce(p.device, p.user),
            NetEvent::PeerDown { device } => {
                let removed = {
                    let mut s = self
                        .inner
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    s.told.remove(device);
                    s.roster.remove(device).is_some()
                };
                self.inner
                    .core
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove_peer(device);
                if removed {
                    self.notify();
                }
                self.end_call_if_alone();
            }
            NetEvent::Control {
                from,
                msg: ControlMessage::Voice(sig),
            } => self.on_signal(from.device, from.user, sig),
            NetEvent::Datagram { from, data } => {
                let known = self
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .roster
                    .contains_key(from);
                if known {
                    self.inner
                        .core
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .on_datagram(from, data);
                }
            }
            _ => {}
        }
    }

    fn on_signal(&self, device: PublicKey, user: PublicKey, sig: &VoiceSignal) {
        let mut changed = false;
        match sig {
            VoiceSignal::Join {
                space_id, channel, ..
            } => {
                let mine = {
                    let mut s = self
                        .inner
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let allowed = if *space_id == CALL_SPACE {
                        s.call_peer == Some(user)
                    } else {
                        (self.inner.is_member)(space_id, &user)
                    };
                    if s.current == Some((*space_id, *channel)) && allowed {
                        if s.call_peer.is_some() {
                            s.call_seen = true;
                        }
                        let is_new = !s.roster.contains_key(&device);
                        if is_new {
                            s.roster.insert(
                                device,
                                Participant {
                                    user,
                                    device,
                                    muted: false,
                                    deafened: false,
                                },
                            );
                            // Our own Join may have reached him before he was in the channel
                            // (messages are sent from separate tasks and can overtake each
                            // other), in which case he dropped it. Tell him again.
                            s.told.remove(&device);
                        }
                        changed = true;
                        true
                    } else {
                        // Er ist (jetzt) woanders: aus unserer Liste nehmen.
                        changed = s.roster.remove(&device).is_some();
                        false
                    }
                };
                if mine {
                    // Auch er soll uns kennen (falls sein Join vor unserem kam).
                    self.introduce(device, user);
                } else {
                    self.inner
                        .core
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove_peer(&device);
                }
            }
            VoiceSignal::Leave { space_id, channel } => {
                let mut s = self
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if s.current == Some((*space_id, *channel)) && s.roster.remove(&device).is_some() {
                    changed = true;
                    // Er kann spaeter erneut beitreten: wir muessen ihm dann wieder Join senden.
                    s.told.remove(&device);
                    drop(s);
                    self.inner
                        .core
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove_peer(&device);
                    self.end_call_if_alone();
                }
            }
            // Anrufsignale nur von angenommenen Freunden (sonst koennte jeder andere verbundene
            // Peer, z. B. ein Space-Mitglied, bei uns klingeln lassen).
            VoiceSignal::CallInvite { .. }
            | VoiceSignal::CallAccept { .. }
            | VoiceSignal::CallDecline { .. }
            | VoiceSignal::CallCancel { .. }
                if !(self.inner.is_friend)(&user) => {}
            VoiceSignal::CallInvite { call_id } => self.on_invite(device, user, *call_id),
            VoiceSignal::CallAccept { call_id } => self.on_accept(device, user, *call_id),
            VoiceSignal::CallDecline { call_id, .. } => self.on_decline(device, user, *call_id),
            VoiceSignal::CallCancel { call_id } => self.on_cancel(device, *call_id),
            VoiceSignal::State { muted, deafened } => {
                let mut s = self
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(p) = s.roster.get_mut(&device) {
                    if p.muted != *muted || p.deafened != *deafened {
                        p.muted = *muted;
                        p.deafened = *deafened;
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.notify();
        }
    }
}

impl VoiceService {
    /// Nur fuer Tests: kuerzeres Klingeln.
    pub fn with_ring_timeout(mut self, t: Duration) -> Self {
        let inner = Arc::get_mut(&mut self.inner).expect("noch nicht geteilt");
        inner.ring_timeout = t;
        self
    }

    /// Fuer Tests: Sperrzeit nach abgelehntem/verpasstem Anruf.
    pub fn with_call_cooldown(mut self, t: Duration) -> Self {
        let inner = Arc::get_mut(&mut self.inner).expect("noch nicht geteilt");
        inner.call_cooldown = t;
        self
    }

    /// (stumm, taub) der eigenen Seite.
    pub fn local_state(&self) -> (bool, bool) {
        let s = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (s.muted, s.deafened)
    }

    pub fn call_state(&self) -> CallState {
        let s = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(p) = s.call_peer {
            return CallState::Active { user: p };
        }
        if let Some(o) = &s.outgoing {
            return CallState::Outgoing { user: o.user };
        }
        if let Some(i) = &s.incoming {
            return CallState::Incoming { user: i.user };
        }
        CallState::Idle
    }

    /// Kurzer Hinweis zum letzten beendeten Anruf (abgelehnt, keine Antwort ...).
    pub fn call_note(&self) -> Option<&'static str> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .note
    }

    fn notify_call(&self) {
        let _ = self.inner.events.send(VoiceEvent::Call);
    }

    /// Ruft einen verbundenen Freund an (Einladung an alle seine Geraete).
    pub fn call(&self, user: PublicKey) -> Result<(), &'static str> {
        let devices: Vec<PublicKey> = self
            .inner
            .net
            .peers()
            .iter()
            .filter(|p| p.user == user)
            .map(|p| p.device)
            .collect();
        if devices.is_empty() {
            return Err(app_proto::i18n::tr(
                "Unreachable (offline)",
                "Nicht erreichbar (offline)",
            ));
        }
        let call_id: ChannelId = rand::random();
        {
            let mut s = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if s.current.is_some() || s.outgoing.is_some() || s.incoming.is_some() {
                return Err(app_proto::i18n::tr(
                    "You are already in a call",
                    "Du bist schon in einem Gespräch",
                ));
            }
            s.note = None;
            s.outgoing = Some(Outgoing {
                call_id,
                user,
                devices: devices.clone(),
            });
        }
        for d in devices {
            self.send_to(d, VoiceSignal::CallInvite { call_id });
        }
        self.notify_call();
        let me = self.clone();
        let t = self.inner.ring_timeout;
        tokio::spawn(async move {
            tokio::time::sleep(t).await;
            me.finish_outgoing(
                call_id,
                Some(app_proto::i18n::tr("No answer", "Keine Antwort")),
                true,
            );
        });
        Ok(())
    }

    /// Beendet einen ausgehenden Anruf (Auflegen vor dem Annehmen oder Zeitablauf).
    fn finish_outgoing(&self, call_id: ChannelId, note: Option<&'static str>, tell: bool) {
        let o = {
            let mut s = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if s.outgoing.as_ref().is_none_or(|o| o.call_id != call_id) {
                return;
            }
            s.note = note;
            s.outgoing.take()
        };
        if let (Some(o), true) = (o, tell) {
            for d in o.devices {
                self.send_to(d, VoiceSignal::CallCancel { call_id });
            }
        }
        self.notify_call();
    }

    /// Ausgehenden Anruf abbrechen.
    pub fn cancel_call(&self) {
        let id = self
            .inner
            .state
            .locked()
            .outgoing
            .as_ref()
            .map(|o| o.call_id);
        if let Some(id) = id {
            self.finish_outgoing(id, None, true);
        }
    }

    /// Eingehenden Anruf annehmen.
    pub fn accept_call(&self) {
        let inc = {
            let mut s = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(i) = s.incoming.take() else { return };
            s.call_peer = Some(i.user);
            i
        };
        self.send_to(
            inc.device,
            VoiceSignal::CallAccept {
                call_id: inc.call_id,
            },
        );
        self.join_inner(CALL_SPACE, inc.call_id);
        self.notify_call();
    }

    /// Eingehenden Anruf ablehnen.
    pub fn decline_call(&self) {
        let inc = {
            let mut s = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let inc = s.incoming.take();
            if let Some(i) = &inc {
                self.start_cooldown(&mut s, i.user);
            }
            inc
        };
        if let Some(i) = inc {
            self.send_to(
                i.device,
                VoiceSignal::CallDecline {
                    call_id: i.call_id,
                    busy: false,
                },
            );
            self.notify_call();
        }
    }

    /// Laufendes Gespraech beenden.
    pub fn hangup(&self) {
        self.leave();
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .note = None;
        self.notify_call();
    }

    fn end_call_if_alone(&self) {
        let end = {
            let mut s = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let end = s.call_peer.is_some() && s.call_seen && s.roster.is_empty();
            if end {
                s.note = Some(app_proto::i18n::tr("Call ended", "Gespräch beendet"));
            }
            end
        };
        if end {
            self.leave();
            self.notify_call();
        }
    }

    fn start_cooldown(&self, s: &mut State, user: PublicKey) {
        let now = std::time::Instant::now();
        s.cooldown.retain(|_, until| *until > now);
        s.cooldown.insert(user, now + self.inner.call_cooldown);
    }

    fn on_invite(&self, device: PublicKey, user: PublicKey, call_id: ChannelId) {
        {
            let mut s = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Wer schon in einem Gespraech oder Channel ist, antwortet nicht (ein automatisches
            // "Besetzt" verriete jedem Freund den eigenen Status); der Anrufer sieht nach dem
            // Klingel-Timeout "Keine Antwort". Ebenso waehrend der Sperre nach einem abgelehnten
            // oder verpassten Anruf desselben Freundes.
            let busy = s.current.is_some() || s.outgoing.is_some() || s.incoming.is_some();
            let cooling = s
                .cooldown
                .get(&user)
                .is_some_and(|until| *until > std::time::Instant::now());
            if busy || cooling {
                return;
            }
            s.note = None;
            s.incoming = Some(Incoming {
                call_id,
                user,
                device,
            });
        }
        self.notify_call();
        let me = self.clone();
        let t = self.inner.ring_timeout + Duration::from_secs(5).min(self.inner.ring_timeout);
        tokio::spawn(async move {
            tokio::time::sleep(t).await;
            me.drop_incoming(
                device,
                call_id,
                app_proto::i18n::tr("Missed call", "Verpasster Anruf"),
            );
        });
    }

    fn drop_incoming(&self, device: PublicKey, call_id: ChannelId, note: &'static str) {
        {
            let mut s = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !s
                .incoming
                .as_ref()
                .is_some_and(|i| i.call_id == call_id && i.device == device)
            {
                return;
            }
            if let Some(i) = s.incoming.take() {
                self.start_cooldown(&mut s, i.user);
            }
            s.note = Some(note);
        }
        self.notify_call();
    }

    fn on_cancel(&self, device: PublicKey, call_id: ChannelId) {
        self.drop_incoming(
            device,
            call_id,
            app_proto::i18n::tr("Missed call", "Verpasster Anruf"),
        );
    }

    fn on_accept(&self, device: PublicKey, user: PublicKey, call_id: ChannelId) {
        let o = {
            let mut s = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !s
                .outgoing
                .as_ref()
                .is_some_and(|o| o.call_id == call_id && o.user == user)
            {
                return;
            }
            s.call_peer = Some(user);
            s.outgoing.take()
        };
        let Some(o) = o else { return };
        // Die anderen Geraete des Freundes sollen aufhoeren zu klingeln.
        for d in o.devices.into_iter().filter(|d| *d != device) {
            self.send_to(d, VoiceSignal::CallCancel { call_id });
        }
        self.join_inner(CALL_SPACE, call_id);
        self.notify_call();
    }

    fn on_decline(&self, device: PublicKey, user: PublicKey, call_id: ChannelId) {
        let done =
            {
                let mut s = self
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Nur die angerufenen Geraete duerfen ablehnen, jedes hoechstens einmal.
                let Some(o) = s.outgoing.as_mut().filter(|o| {
                    o.call_id == call_id && o.user == user && o.devices.contains(&device)
                }) else {
                    return;
                };
                o.devices.retain(|d| *d != device);
                if o.devices.is_empty() {
                    s.outgoing = None;
                    s.note = Some(app_proto::i18n::tr("Declined", "Abgelehnt"));
                    true
                } else {
                    false
                }
            };
        if done {
            self.notify_call();
        }
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;

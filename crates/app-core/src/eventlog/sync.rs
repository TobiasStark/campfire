//! Sync-Dienst: Heads tauschen, Fehlendes anfordern, Neues sofort verteilen.
//!
//! Ablauf eines Sync-Streams (beide Seiten symmetrisch):
//! `Heads` -> Antwort `Heads` + `Want` -> Antwort `Events`* + `Done`. Ein Push-Stream
//! besteht nur aus `Events`* + `Done`.

use super::{
    create_event, gate, heads_for, ingest_batch_with, read_want, rebuild_space, verify, Error,
    HlcClock, IngestOpts, Reject,
};
use crate::storage::{Db, StorageError};
use app_net::{read_frame, write_frame, Net, PeerInfo};
use app_proto::event::{EventEnvelope, EventKind, SpaceId};
use app_proto::lock::Locked;
use app_proto::sync::SyncMessage;
use app_proto::{PublicKey, Signature};
use iroh::endpoint::{RecvStream, SendStream};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Semaphore};

/// Gleichzeitig bearbeitete eingehende Streams je Geraet; weitere warten (N-3).
const INBOUND_RUNNING: usize = 4;
/// Eingehende Streams je Geraet, die hoechstens warten; darueber hinaus wird verworfen.
const INBOUND_WAITING: usize = 32;

/// Events, die ein Peer in einem Stream ungefragt schicken darf (Push eigener oder
/// weitergereichter Events). Mehr gibt es nur auf ein eigenes `Want`.
const PUSH_ALLOWANCE: usize = 200;

/// Abstand, in dem die Speichergrenze fuer Nachrichten geprueft wird (erstmals kurz nach dem
/// Start).
const PRUNE_EVERY: Duration = Duration::from_secs(300);
const PRUNE_FIRST: Duration = Duration::from_secs(30);

/// (laufende, laufende + wartende) eingehende Streams eines Geraets.
type InboundSlots = (Arc<Semaphore>, Arc<Semaphore>);

pub type Signer = Arc<dyn Fn(&[u8]) -> Signature + Send + Sync>;

/// Hinweis fuer die UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogEvent {
    /// In diesen Spaces gibt es neue Events oder eine neue Projektion.
    SpacesChanged(Vec<SpaceId>),
    /// In diesen Spaces hat ein Mitglied weitere eigene Nachrichten bestaetigt
    /// (Zustellstatus "zugestellt" neu anzeigen).
    DeliveryChanged(Vec<SpaceId>),
}

#[derive(Debug, Clone, Copy)]
pub struct SyncConfig {
    /// Abstand der Abgleiche mit allen verbundenen Peers (faengt verpasste Pushes ab).
    pub resync_every: Duration,
    /// Hoechstdauer eines Streams.
    pub stream_timeout: Duration,
    /// Bis zu dieser Anzahl neuer Events werden sie an andere Peers weitergereicht.
    pub relay_limit: usize,
    /// Neuaufbauten nach Events aus der Vergangenheit hoechstens so oft je Space (M-4).
    pub rebuild_delay: Duration,
    /// Mindestabstand zweier Abgleiche mit demselben Geraet; Anstoesse dazwischen werden
    /// zusammengefasst (N-3).
    pub min_resync: Duration,
    /// So lange wird ein Geraet ignoriert, das gefaelschte Events oder mehr Events als
    /// angefordert schickt.
    pub penalty: Duration,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            resync_every: Duration::from_secs(60),
            stream_timeout: Duration::from_secs(120),
            relay_limit: 50,
            rebuild_delay: Duration::from_secs(2),
            min_resync: Duration::from_secs(1),
            penalty: Duration::from_secs(300),
        }
    }
}

struct Inner {
    /// Eigene Aktionen (Vorrang auf dem DB-Thread).
    db: Db,
    /// Arbeit, die Peers ausloesen (Empfang, Abgleich, Neuaufbau): laeuft hinter den eigenen.
    bg: Db,
    net: Net,
    clock: HlcClock,
    user: PublicKey,
    sign: Signer,
    events: mpsc::UnboundedSender<LogEvent>,
    /// Geraete, mit denen gerade ein von uns gestarteter Abgleich laeuft.
    active: Mutex<HashSet<PublicKey>>,
    /// Spaces, deren Neuaufbau ansteht (entprellt, M-4).
    dirty: Mutex<HashSet<SpaceId>>,
    /// Geraete, fuer die ein Abgleich angestossen, aber noch nicht gestartet ist (N-3).
    queued: Mutex<HashSet<PublicKey>>,
    /// Beginn des letzten Abgleichs je Geraet (N-3).
    last_sync: Mutex<HashMap<PublicKey, Instant>>,
    /// Je Geraet: (laufende, laufende + wartende) eingehende Streams (N-3).
    inbound: Mutex<HashMap<PublicKey, InboundSlots>>,
    /// Geraete, die gegen die Regeln verstossen haben, bis zu diesem Zeitpunkt.
    penalized: Mutex<HashMap<PublicKey, Instant>>,
    cfg: SyncConfig,
}

#[derive(Clone)]
pub struct SyncService {
    inner: Arc<Inner>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

async fn run_db<R: Send + 'static>(
    db: &Db,
    f: impl FnOnce(&mut Connection) -> rusqlite::Result<R> + Send + 'static,
) -> Result<R, StorageError> {
    let db = db.clone();
    tokio::task::spawn_blocking(move || db.call(f))
        .await
        .map_err(|_| StorageError::Closed)?
}

impl SyncService {
    pub async fn start(
        db: Db,
        net: Net,
        user: PublicKey,
        sign: Signer,
        cfg: SyncConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<LogEvent>), StorageError> {
        let clock = run_db(&db, |c| HlcClock::load(c)).await?;
        let (tx, rx) = mpsc::unbounded_channel();
        let svc = Self {
            inner: Arc::new(Inner {
                bg: db.background(),
                db,
                net,
                clock,
                user,
                sign,
                events: tx,
                active: Mutex::default(),
                dirty: Mutex::default(),
                queued: Mutex::default(),
                last_sync: Mutex::default(),
                inbound: Mutex::default(),
                penalized: Mutex::default(),
                cfg,
            }),
        };
        let s = svc.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(s.inner.cfg.resync_every);
            t.tick().await;
            loop {
                t.tick().await;
                for p in s.inner.net.peers() {
                    s.spawn_sync(p);
                }
            }
        });
        let s = svc.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PRUNE_FIRST).await;
            loop {
                s.enforce_storage().await;
                tokio::time::sleep(PRUNE_EVERY).await;
            }
        });
        Ok((svc, rx))
    }

    /// Haelt die Speichergrenze fuer Nachrichten ein (Einstellung `messages.limit_mb`); gibt die
    /// Zahl der aufgeraeumten Events zurueck.
    pub async fn enforce_storage(&self) -> usize {
        use super::prune::{enforce, LIMIT_MB_DEFAULT, LIMIT_MB_MIN};
        let me = self.inner.user;
        let Ok(out) = run_db(&self.inner.bg, move |c| {
            let mb = crate::settings::get::<u64>(c, crate::settings::MESSAGES_MB)?
                .unwrap_or(LIMIT_MB_DEFAULT)
                .max(LIMIT_MB_MIN);
            enforce(c, &me, mb.saturating_mul(1024 * 1024))
        })
        .await
        else {
            return 0;
        };
        if !out.spaces.is_empty() {
            let _ = self.inner.events.send(LogEvent::SpacesChanged(
                out.spaces.iter().copied().collect(),
            ));
        }
        out.pruned
    }

    /// Neues Event lokal erzeugen und an Mitglieder verteilen.
    /// `space` ist nur fuer `SpaceCreate` leer.
    pub async fn publish(
        &self,
        space: Option<SpaceId>,
        kind: EventKind,
    ) -> Result<EventEnvelope, Error> {
        let (user, sign) = (self.inner.user, self.inner.sign.clone());
        let this = self.clone();
        let env = run_db(&self.inner.db, move |c| {
            Ok(create_event(
                c,
                &this.inner.clock,
                user,
                &*sign,
                space,
                kind,
                now_ms(),
            ))
        })
        .await
        .map_err(|_| Error::Db(rusqlite::Error::InvalidQuery))??;
        let _ = self
            .inner
            .events
            .send(LogEvent::SpacesChanged(vec![env.body.space_id]));
        let s = self.clone();
        let e = env.clone();
        tokio::spawn(async move { s.push(vec![e], None).await });
        Ok(env)
    }

    /// Verbindung steht: abgleichen (nur die Seite mit der kleineren Geraete-ID beginnt).
    pub fn on_peer_up(&self, peer: PeerInfo) {
        if self.inner.net.addr().id.as_bytes() < &peer.device {
            self.spawn_sync(peer);
        }
    }

    /// Stoesst einen Abgleich an. Mehrfache Anstoesse (Luecken, fehlende Mitgliedschaft, neue
    /// Events, Timer) werden zusammengefasst und hoechstens alle `min_resync` je Geraet
    /// ausgefuehrt, damit ein Peer keine beliebig wiederholte Arbeit ausloesen kann (N-3).
    pub fn spawn_sync(&self, peer: PeerInfo) {
        if !self.inner.queued.locked().insert(peer.device) {
            return;
        }
        let s = self.clone();
        tokio::spawn(async move {
            let wait = s
                .inner
                .last_sync
                .locked()
                .get(&peer.device)
                .map(|t| (*t + s.inner.cfg.min_resync).saturating_duration_since(Instant::now()))
                .unwrap_or_default();
            tokio::time::sleep(wait).await;
            s.inner.queued.locked().remove(&peer.device);
            // Gleich nach dem Verbindungsaufbau scheitert der erste Versuch gelegentlich
            // (Verbindung wird noch zusammengefuehrt); kurz spaeter nochmal versuchen.
            for pause in [0u64, 300, 1000, 3000] {
                tokio::time::sleep(Duration::from_millis(pause)).await;
                if !s.inner.net.peers().iter().any(|p| p.device == peer.device) {
                    return;
                }
                if s.sync_with(peer.clone()).await.is_ok() {
                    return;
                }
            }
        });
    }

    /// Ob `device` gerade gesperrt ist.
    pub fn is_penalized(&self, device: &PublicKey) -> bool {
        let mut g = self.inner.penalized.locked();
        match g.get(device) {
            Some(until) if *until > Instant::now() => true,
            Some(_) => {
                g.remove(device);
                false
            }
            None => false,
        }
    }

    /// Sperrt ein Geraet, das gefaelschte oder nicht angeforderte Events geschickt hat.
    fn penalize(&self, device: PublicKey) {
        self.inner
            .penalized
            .locked()
            .insert(device, Instant::now() + self.inner.cfg.penalty);
    }

    /// Eingehender Sync- oder Push-Stream.
    pub fn on_sync_stream(&self, from: PublicKey, send: SendStream, recv: RecvStream) {
        if self.is_penalized(&from) {
            return;
        }
        let Some(peer) = self
            .inner
            .net
            .peers()
            .into_iter()
            .find(|p| p.device == from)
        else {
            return;
        };
        // Je Geraet nur wenige Streams gleichzeitig, eine begrenzte Zahl wartet, der Rest wird
        // verworfen (der Abgleich holt es spaeter nach).
        let (running, admitted) = self
            .inner
            .inbound
            .locked()
            .entry(from)
            .or_insert_with(|| {
                (
                    Arc::new(Semaphore::new(INBOUND_RUNNING)),
                    Arc::new(Semaphore::new(INBOUND_RUNNING + INBOUND_WAITING)),
                )
            })
            .clone();
        let Ok(ticket) = admitted.try_acquire_owned() else {
            return;
        };
        let s = self.clone();
        tokio::spawn(async move {
            let _ticket = ticket;
            let Ok(_run) = running.acquire_owned().await else {
                return;
            };
            // Hat uns der andere bei seinem Abgleich Fehlendes geschickt, kennt er nur unseren
            // Stand von vorher: gleich den neuen melden, damit er "zugestellt" anzeigt.
            if let Ok((got, true)) = s.run_stream(peer.clone(), send, recv, Vec::new()).await {
                if got > 0 {
                    s.spawn_sync(peer);
                }
            }
        });
    }

    pub async fn sync_with(&self, peer: PeerInfo) -> Result<(), String> {
        if self.is_penalized(&peer.device) {
            return Err("Geraet gesperrt".into());
        }
        if !self
            .inner
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(peer.device)
        {
            return Ok(());
        }
        self.inner
            .last_sync
            .locked()
            .insert(peer.device, Instant::now());
        let r = async {
            let user = peer.user;
            let heads = run_db(&self.inner.bg, move |c| heads_for(c, &user))
                .await
                .map_err(|e| e.to_string())?;
            let (send, recv) = self
                .inner
                .net
                .open_sync(&peer.device)
                .await
                .map_err(|e| e.to_string())?;
            self.run_stream(peer.clone(), send, recv, vec![SyncMessage::Heads(heads)])
                .await
        }
        .await;
        self.inner
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&peer.device);
        // Neues bekommen: der andere hat unsere Heads von vor dem Empfang. Noch einmal
        // abgleichen, damit er sofort sieht, dass seine Nachrichten angekommen sind.
        if let Ok((got, _)) = r {
            if got > 0 {
                self.spawn_sync(peer);
            }
        }
        r.map(|_| ())
    }

    /// Nur fuer Tests: verteilt fertig gebaute Events wie ein veraenderter Client.
    #[cfg(test)]
    pub(crate) async fn push_for_test(&self, envs: Vec<EventEnvelope>) {
        self.push(envs, None).await;
    }

    /// Wie `push`, aber ohne rekursiven Future-Typ (push -> run_stream -> ingest -> push).
    fn push_boxed(
        &self,
        envs: Vec<EventEnvelope>,
        exclude: Option<PublicKey>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let s = self.clone();
        Box::pin(async move { s.push(envs, exclude).await })
    }

    /// Schickt Events direkt an verbundene Mitglieder (ausser `exclude`).
    async fn push(&self, envs: Vec<EventEnvelope>, exclude: Option<PublicKey>) {
        let peers: Vec<PeerInfo> = self
            .inner
            .net
            .peers()
            .into_iter()
            .filter(|p| Some(p.device) != exclude && p.user != self.inner.user)
            .collect();
        if peers.is_empty() {
            return;
        }
        let list: Vec<(PublicKey, PublicKey)> = peers.iter().map(|p| (p.device, p.user)).collect();
        let e2 = envs.clone();
        let Ok(plan) = run_db(&self.inner.db, move |c| {
            let mut st =
                c.prepare("SELECT 1 FROM members WHERE space_id = ?1 AND user_pubkey = ?2")?;
            let mut out = Vec::new();
            for (dev, user) in &list {
                let mut mine = Vec::new();
                for e in &e2 {
                    if st.exists(rusqlite::params![e.body.space_id, user])? {
                        mine.push(e.clone());
                    }
                }
                if !mine.is_empty() {
                    out.push((*dev, mine));
                }
            }
            Ok(out)
        })
        .await
        else {
            return;
        };
        for (device, mine) in plan {
            let Some(peer) = peers.iter().find(|p| p.device == device).cloned() else {
                continue;
            };
            let s = self.clone();
            tokio::spawn(async move {
                let Ok((send, recv)) = s.inner.net.open_sync(&peer.device).await else {
                    return;
                };
                let first = vec![SyncMessage::Events(mine), SyncMessage::Done];
                let _ = s.run_stream(peer.clone(), send, recv, first).await;
                // Der Abgleich danach zeigt, ob der Empfaenger die Events wirklich hat.
                let _ = s.sync_with(peer).await;
            });
        }
    }

    /// Nimmt Events auf; gibt die Zahl der neu angenommenen zurueck. `Err`, wenn eine Signatur
    /// gefaelscht ist: das tut kein ehrlicher Client (jeder prueft vor dem Weiterreichen).
    async fn ingest(
        &self,
        peer: &PeerInfo,
        envs: Vec<EventEnvelope>,
        from_sync: bool,
    ) -> Result<usize, &'static str> {
        let n = envs.len();
        // Signaturen auf dem Thread-Pool pruefen, nicht auf dem DB-Thread: der bleibt frei fuer
        // die eigene Bedienung. Bei der ersten Faelschung ist Schluss.
        let now = now_ms();
        let envs = tokio::task::spawn_blocking(move || {
            let mut ok = Vec::with_capacity(envs.len());
            for e in envs {
                match verify(&e, now) {
                    Ok(()) => ok.push(e),
                    Err(Reject::BadSignature) => return Err("gefaelschte Signatur"),
                    Err(_) => {}
                }
            }
            Ok(ok)
        })
        .await
        .map_err(|_| "Pruefung abgebrochen")??;
        let opts = IngestOpts {
            me: Some(self.inner.user),
            defer_rebuild: true,
            verified: true,
            ..IngestOpts::default()
        };
        let Ok(out) = run_db(&self.inner.bg, move |c| {
            ingest_batch_with(c, envs, now_ms(), opts)
        })
        .await
        else {
            return Ok(0);
        };
        if !out.dirty.is_empty() {
            self.schedule_rebuild(&out.dirty);
        }
        let accepted = out.accepted.len();
        if let Some(h) = out.accepted.iter().map(|e| e.body.hlc).max() {
            self.inner.clock.observe(h, now_ms());
        }
        if !out.spaces.is_empty() {
            let _ = self.inner.events.send(LogEvent::SpacesChanged(
                out.spaces.iter().copied().collect(),
            ));
        }
        let missing = out
            .rejected
            .iter()
            .any(|r| matches!(r, Reject::Gap { .. } | Reject::NotMember));
        if missing && !from_sync && n > 0 {
            self.spawn_sync(peer.clone());
        }
        if !out.accepted.is_empty() && out.accepted.len() <= self.inner.cfg.relay_limit {
            // Events blockierter Nutzer nicht aktiv weiterreichen (M-2). Im Log bleiben sie,
            // damit alle Knoten dasselbe speichern; angezeigt werden sie nicht.
            let blocked = run_db(&self.inner.bg, |c| super::consent::blocked_users(c))
                .await
                .unwrap_or_default();
            let (mut acc, from) = (out.accepted, peer.device);
            acc.retain(|e| !blocked.contains(&e.body.author));
            if !acc.is_empty() {
                tokio::spawn(self.push_boxed(acc, Some(from)));
            }
        }
        Ok(accepted)
    }

    /// Baut die Spaces nach `rebuild_delay` neu auf; weitere Anforderungen bis dahin werden
    /// zusammengefasst. So kann niemand mit Events aus der Vergangenheit einen Neuaufbau pro
    /// Stapel erzwingen (M-4).
    fn schedule_rebuild(&self, spaces: &HashSet<SpaceId>) {
        let first = {
            let mut g = self.inner.dirty.locked();
            let first = g.is_empty();
            g.extend(spaces.iter().copied());
            first
        };
        if !first {
            return;
        }
        let s = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(s.inner.cfg.rebuild_delay).await;
            let spaces: Vec<SpaceId> = std::mem::take(&mut *s.inner.dirty.locked())
                .into_iter()
                .collect();
            let todo = spaces.clone();
            let done = run_db(&s.inner.bg, move |c| {
                let now_s = (now_ms() / 1000) as i64;
                let tx = c.transaction()?;
                for sp in &todo {
                    rebuild_space(&tx, sp, now_s)?;
                }
                tx.commit()
            })
            .await;
            if done.is_ok() {
                let _ = s.inner.events.send(LogEvent::SpacesChanged(spaces));
            }
        });
    }

    /// Fuehrt einen Sync- oder Push-Stream. Ergebnis: (neu angenommene Events, ob es ein
    /// Abgleich mit Heads war).
    async fn run_stream(
        &self,
        peer: PeerInfo,
        mut send: SendStream,
        mut recv: RecvStream,
        first: Vec<SyncMessage>,
    ) -> Result<(usize, bool), String> {
        let (tx, mut rx) = mpsc::channel::<SyncMessage>(8);
        let writer = tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if write_frame(&mut send, &m).await.is_err() {
                    return;
                }
            }
            let _ = send.finish();
        });
        let sent_first_heads = first.iter().any(|m| matches!(m, SyncMessage::Heads(_)));
        let push_only = !first.is_empty() && !sent_first_heads;
        let inbound = first.is_empty();
        let mut tx = Some(tx);
        for m in first {
            if tx.as_ref().unwrap().send(m).await.is_err() {
                break;
            }
        }
        if push_only {
            tx = None;
        }
        let (user, me) = (peer.user, self.inner.user);
        let db = self.inner.bg.clone();
        // Wie viele Events der andere noch schicken darf: ungefragt nur ein kleiner Push (in
        // Streams, die er geoeffnet hat), sonst so viele, wie wir mit `Want` angefordert haben.
        let mut allowance = if inbound {
            PUSH_ALLOWANCE.max(self.inner.cfg.relay_limit)
        } else {
            0
        };
        let mut sent_heads = sent_first_heads;
        let mut heads_seen = false;
        let mut got = 0usize;
        let events = self.inner.events.clone();
        let body = async {
            loop {
                let Ok(msg) = read_frame::<SyncMessage>(&mut recv).await else {
                    break;
                };
                match msg {
                    SyncMessage::Heads(remote) => {
                        // Je Stream genau ein Heads je Richtung; mehr waere beliebig
                        // wiederholbare Arbeit (N-3).
                        if std::mem::replace(&mut heads_seen, true) {
                            return Err("Heads mehrfach".into());
                        }
                        let (r2, pu) = (remote.clone(), peer.user);
                        if let Ok(changed) =
                            run_db(&db, move |c| super::record_acks(c, &pu, &r2)).await
                        {
                            if !changed.is_empty() {
                                let _ = events.send(LogEvent::DeliveryChanged(changed));
                            }
                        }
                        let Some(t) = tx.as_ref() else { continue };
                        if !sent_heads {
                            sent_heads = true;
                            let h = run_db(&db, move |c| heads_for(c, &user))
                                .await
                                .map_err(|e| e.to_string())?;
                            let _ = t.send(SyncMessage::Heads(h)).await;
                        }
                        let wants = run_db(&db, move |c| {
                            gate::wants_for(c, &me, &remote, now_ms(), gate::PENDING_SPACE_EVENTS)
                        })
                        .await
                        .map_err(|e| e.to_string())?;
                        allowance = wants.iter().fold(allowance, |a, w| {
                            let n = w.to_seq.saturating_sub(w.from_seq).saturating_add(1);
                            a.saturating_add(usize::try_from(n).unwrap_or(usize::MAX))
                        });
                        let _ = t.send(SyncMessage::Want(wants)).await;
                    }
                    SyncMessage::Want(wants) => {
                        let Some(t) = tx.take() else { continue };
                        'serve: for w in wants {
                            let mut want = w;
                            loop {
                                let w2 = want.clone();
                                let (evs, next) = run_db(&db, move |c| read_want(c, &user, &w2))
                                    .await
                                    .map_err(|e| e.to_string())?;
                                if !evs.is_empty()
                                    && t.send(SyncMessage::Events(evs)).await.is_err()
                                {
                                    break 'serve;
                                }
                                match next {
                                    Some(n) => want.from_seq = n,
                                    None => break,
                                }
                            }
                        }
                        let _ = t.send(SyncMessage::Done).await;
                        // `t` faellt weg: Writer beendet den Stream.
                    }
                    SyncMessage::Events(evs) => {
                        let Some(rest) = allowance.checked_sub(evs.len()) else {
                            self.penalize(peer.device);
                            return Err("mehr Events als angefordert".into());
                        };
                        allowance = rest;
                        match self.ingest(&peer, evs, sent_heads).await {
                            Ok(n) => got += n,
                            Err(why) => {
                                self.penalize(peer.device);
                                return Err(why.into());
                            }
                        }
                    }
                    SyncMessage::Done => {}
                }
            }
            Ok::<(), String>(())
        };
        let r = tokio::time::timeout(self.inner.cfg.stream_timeout, body).await;
        drop(tx);
        let _ = writer.await;
        r.unwrap_or(Err("Timeout".into()))?;
        Ok((got, sent_heads))
    }
}

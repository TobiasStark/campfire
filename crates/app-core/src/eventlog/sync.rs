//! Sync-Dienst: Heads tauschen, Fehlendes anfordern, Neues sofort verteilen.
//!
//! Ablauf eines Sync-Streams (beide Seiten symmetrisch):
//! `Heads` -> Antwort `Heads` + `Want` -> Antwort `Events`* + `Done`. Ein Push-Stream
//! besteht nur aus `Events`* + `Done`.

use super::{
    create_event, heads_for, ingest_batch, read_want, wants_from, Error, HlcClock, Reject,
};
use crate::storage::{Db, StorageError};
use app_net::{read_frame, write_frame, Net, PeerInfo};
use app_proto::event::{EventEnvelope, EventKind, SpaceId};
use app_proto::sync::SyncMessage;
use app_proto::{PublicKey, Signature};
use iroh::endpoint::{RecvStream, SendStream};
use rusqlite::Connection;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

pub type Signer = Arc<dyn Fn(&[u8]) -> Signature + Send + Sync>;

/// Hinweis fuer die UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogEvent {
    /// In diesen Spaces gibt es neue Events oder eine neue Projektion.
    SpacesChanged(Vec<SpaceId>),
}

#[derive(Debug, Clone, Copy)]
pub struct SyncConfig {
    /// Abstand der Abgleiche mit allen verbundenen Peers (faengt verpasste Pushes ab).
    pub resync_every: Duration,
    /// Hoechstdauer eines Streams.
    pub stream_timeout: Duration,
    /// Bis zu dieser Anzahl neuer Events werden sie an andere Peers weitergereicht.
    pub relay_limit: usize,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            resync_every: Duration::from_secs(60),
            stream_timeout: Duration::from_secs(120),
            relay_limit: 50,
        }
    }
}

struct Inner {
    db: Db,
    net: Net,
    clock: HlcClock,
    user: PublicKey,
    sign: Signer,
    events: mpsc::UnboundedSender<LogEvent>,
    /// Geraete, mit denen gerade ein von uns gestarteter Abgleich laeuft.
    active: Mutex<HashSet<PublicKey>>,
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
                db,
                net,
                clock,
                user,
                sign,
                events: tx,
                active: Mutex::default(),
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
        Ok((svc, rx))
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

    pub fn spawn_sync(&self, peer: PeerInfo) {
        let s = self.clone();
        tokio::spawn(async move {
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

    /// Eingehender Sync- oder Push-Stream.
    pub fn on_sync_stream(&self, from: PublicKey, send: SendStream, recv: RecvStream) {
        let Some(peer) = self
            .inner
            .net
            .peers()
            .into_iter()
            .find(|p| p.device == from)
        else {
            return;
        };
        let s = self.clone();
        tokio::spawn(async move {
            let _ = s.run_stream(peer, send, recv, Vec::new()).await;
        });
    }

    pub async fn sync_with(&self, peer: PeerInfo) -> Result<(), String> {
        if !self
            .inner
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(peer.device)
        {
            return Ok(());
        }
        let r = async {
            let user = peer.user;
            let heads = run_db(&self.inner.db, move |c| heads_for(c, &user))
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
        r
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

    async fn ingest(&self, peer: &PeerInfo, envs: Vec<EventEnvelope>, from_sync: bool) {
        let n = envs.len();
        let Ok(out) = run_db(&self.inner.db, move |c| ingest_batch(c, envs, now_ms())).await else {
            return;
        };
        if let Some(h) = out.accepted.iter().map(|e| e.body.hlc).max() {
            self.inner.clock.observe(h);
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
            let (acc, from) = (out.accepted, peer.device);
            tokio::spawn(self.push_boxed(acc, Some(from)));
        }
    }

    async fn run_stream(
        &self,
        peer: PeerInfo,
        mut send: SendStream,
        mut recv: RecvStream,
        first: Vec<SyncMessage>,
    ) -> Result<(), String> {
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
        let mut tx = Some(tx);
        for m in first {
            if tx.as_ref().unwrap().send(m).await.is_err() {
                break;
            }
        }
        if push_only {
            tx = None;
        }
        let user = peer.user;
        let db = self.inner.db.clone();
        let mut sent_heads = sent_first_heads;
        let body = async {
            loop {
                let Ok(msg) = read_frame::<SyncMessage>(&mut recv).await else {
                    break;
                };
                match msg {
                    SyncMessage::Heads(remote) => {
                        let (r2, pu) = (remote.clone(), peer.user);
                        let _ = run_db(&db, move |c| super::record_acks(c, &pu, &r2)).await;
                        let Some(t) = tx.as_ref() else { continue };
                        if !sent_heads {
                            sent_heads = true;
                            let h = run_db(&db, move |c| heads_for(c, &user))
                                .await
                                .map_err(|e| e.to_string())?;
                            let _ = t.send(SyncMessage::Heads(h)).await;
                        }
                        let wants = run_db(&db, move |c| wants_from(c, &remote))
                            .await
                            .map_err(|e| e.to_string())?;
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
                    SyncMessage::Events(evs) => self.ingest(&peer, evs, sent_heads).await,
                    SyncMessage::Done => {}
                }
            }
            Ok::<(), String>(())
        };
        let r = tokio::time::timeout(self.inner.cfg.stream_timeout, body).await;
        drop(tx);
        let _ = writer.await;
        r.unwrap_or(Err("Zeitueberschreitung".into()))
    }
}

//! Der Freunde-Dienst: Anfragen senden/annehmen, Outbox-Zustellung, Hello-Server.

use super::limiter::{Limits, RateLimiter};
use super::store::{self, Friend, FriendStatus, InResult, OutboxEntry, OutboxKind};
use super::FriendsDirectory;
use crate::storage::{Db, StorageError};
use app_net::{read_frame_max, verify_cert, write_frame, Net};
use app_proto::control::DeviceCert;
use app_proto::friend_code::{FriendCode, FriendCodeError};
use app_proto::hello::{HelloMessage, HelloReply, MAX_NAME_LEN, MAX_NOTE_LEN};
use app_proto::PublicKey;
use iroh::{EndpointAddr, EndpointId};
use rusqlite::Connection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify, Semaphore};

/// Maximal gespeicherte unbeantwortete Anfragen von Fremden.
pub const MAX_PENDING_IN: i64 = 100;
const MAX_CONCURRENT_HELLO: usize = 32;
const HELLO_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Groesste Hello-Nachricht (Zertifikat, Name, Notiz); Fremde duerfen sie schicken.
const MAX_HELLO_LEN: usize = 4096;

/// Einstellungen des Dienstes (Standardwerte fuer den Normalbetrieb).
#[derive(Debug, Clone, Copy)]
pub struct ServiceConfig {
    pub limits: Limits,
    /// Zeitlimit fuer einen Zustellversuch.
    pub deliver_timeout: Duration,
    /// Wie oft die Outbox ohne Anstoss geprueft wird.
    pub outbox_poll: Duration,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            deliver_timeout: Duration::from_secs(20),
            outbox_poll: Duration::from_secs(15),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FriendsError {
    #[error("Ungueltiger Freundescode: {0}")]
    Code(#[from] FriendCodeError),
    #[error("Das ist dein eigener Code")]
    SelfAdd,
    #[error("Ihr seid bereits befreundet")]
    AlreadyFriends,
    #[error("Dieser Nutzer ist blockiert")]
    Blocked,
    #[error("Unbekannter Nutzer")]
    NotFound,
    #[error("Speicher: {0}")]
    Storage(#[from] StorageError),
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FriendEvent {
    RequestReceived {
        user: PublicKey,
        name: String,
        note: String,
    },
    Accepted {
        user: PublicKey,
    },
    Declined {
        user: PublicKey,
    },
}

struct Me {
    cert: DeviceCert,
    name: String,
}

struct Inner {
    db: Db,
    net: Net,
    dir: Arc<FriendsDirectory>,
    me: Mutex<Me>,
    limiter: Mutex<RateLimiter>,
    cfg: ServiceConfig,
    events: mpsc::UnboundedSender<FriendEvent>,
    kick: Notify,
    hints: Mutex<HashMap<PublicKey, Vec<SocketAddr>>>,
    hello_slots: Arc<Semaphore>,
}

#[derive(Clone)]
pub struct FriendsService {
    inner: Arc<Inner>,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn clean(s: &str, max_chars: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_owned()
}

impl FriendsService {
    /// Laedt die Allowlist und startet die Outbox-Zustellung.
    pub async fn start(
        db: Db,
        net: Net,
        dir: Arc<FriendsDirectory>,
        cert: DeviceCert,
        display_name: String,
        cfg: ServiceConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<FriendEvent>), FriendsError> {
        let d = dir.clone();
        run_db(&db, move |c| d.reload(c)).await?;
        let (tx, rx) = mpsc::unbounded_channel();
        let svc = Self {
            inner: Arc::new(Inner {
                db,
                net,
                dir,
                me: Mutex::new(Me {
                    cert,
                    name: display_name,
                }),
                limiter: Mutex::new(RateLimiter::new(cfg.limits, Instant::now())),
                cfg,
                events: tx,
                kick: Notify::new(),
                hints: Mutex::default(),
                hello_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_HELLO)),
            }),
        };
        let s = svc.clone();
        tokio::spawn(async move { s.outbox_loop().await });
        Ok((svc, rx))
    }

    /// Direkte Adressen fuer einen Knoten (Tests, LAN); sonst loest iroh ueber Discovery/Relay auf.
    pub fn add_addr_hint(&self, node: PublicKey, addr: SocketAddr) {
        self.inner
            .hints
            .lock()
            .unwrap()
            .entry(node)
            .or_default()
            .push(addr);
    }

    /// Outbox pruefen (faellige Eintraege).
    pub fn kick(&self) {
        self.inner.kick.notify_one();
    }

    /// Alle wartenden Zustellungen sofort neu versuchen (Start, Netzwechsel, Peer online).
    pub async fn retry_now(&self) {
        let _ = run_db(&self.inner.db, |c| store::outbox_reset_backoff(c)).await;
        self.kick();
    }

    // ---------------------------------------------------------- Aktionen

    pub async fn list(&self) -> Result<Vec<Friend>, FriendsError> {
        Ok(run_db(&self.inner.db, |c| store::list(c)).await?)
    }

    /// Code eines Freundes eingeben: legt eine ausgehende Anfrage an und stellt sie zu,
    /// sobald der andere erreichbar ist.
    pub async fn add_friend(&self, code: &str, note: &str) -> Result<PublicKey, FriendsError> {
        let code = FriendCode::decode(code)?;
        let my_user = self
            .inner
            .me
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cert
            .user;
        if code.user == my_user {
            return Err(FriendsError::SelfAdd);
        }
        let note = clean(note, MAX_NOTE_LEN);
        let dir = self.inner.dir.clone();
        let c = code.clone();
        run_db(&self.inner.db, move |conn| {
            match store::get(conn, &c.user)? {
                Some(f) if f.status == FriendStatus::Accepted => {
                    return Ok(Err(FriendsError::AlreadyFriends))
                }
                Some(f) if f.status == FriendStatus::Blocked => {
                    return Ok(Err(FriendsError::Blocked))
                }
                Some(f) if f.status == FriendStatus::PendingIn => {
                    // Der andere hat uns schon angefragt: Code eingeben = annehmen.
                    store::set_status(conn, &c.user, FriendStatus::Accepted)?;
                    store::outbox_put(
                        conn,
                        &c.user,
                        OutboxKind::Accept,
                        &c.node_id,
                        c.relay_url.as_deref(),
                        now_secs(),
                    )?;
                }
                _ => {
                    store::upsert_pending_out(conn, &c, &note)?;
                    store::outbox_put(
                        conn,
                        &c.user,
                        OutboxKind::Request,
                        &c.node_id,
                        c.relay_url.as_deref(),
                        now_secs(),
                    )?;
                }
            }
            dir.reload(conn)?;
            Ok(Ok(()))
        })
        .await??;
        self.kick();
        Ok(code.user)
    }

    /// Eingehende Anfrage annehmen.
    pub async fn accept(&self, user: PublicKey) -> Result<(), FriendsError> {
        let dir = self.inner.dir.clone();
        run_db(&self.inner.db, move |conn| {
            let Some(f) = store::get(conn, &user)? else {
                return Ok(Err(FriendsError::NotFound));
            };
            if f.status != FriendStatus::PendingIn {
                return Ok(Err(FriendsError::NotFound));
            }
            store::set_status(conn, &user, FriendStatus::Accepted)?;
            if let Some(node) = f.node_ids().first() {
                store::outbox_put(conn, &user, OutboxKind::Accept, node, None, now_secs())?;
            }
            dir.reload(conn)?;
            Ok(Ok(()))
        })
        .await??;
        self.kick();
        Ok(())
    }

    /// Eingehende Anfrage ablehnen (der andere erfaehrt es, sobald erreichbar).
    pub async fn decline(&self, user: PublicKey) -> Result<(), FriendsError> {
        let dir = self.inner.dir.clone();
        run_db(&self.inner.db, move |conn| {
            let Some(f) = store::get(conn, &user)? else {
                return Ok(Err(FriendsError::NotFound));
            };
            if f.status != FriendStatus::PendingIn {
                return Ok(Err(FriendsError::NotFound));
            }
            store::delete(conn, &user)?;
            store::outbox_delete(conn, &user)?;
            if let Some(node) = f.node_ids().first() {
                store::outbox_put(conn, &user, OutboxKind::Decline, node, None, now_secs())?;
            }
            dir.reload(conn)?;
            Ok(Ok(()))
        })
        .await??;
        self.kick();
        Ok(())
    }

    pub async fn block(&self, user: PublicKey) -> Result<(), FriendsError> {
        let dir = self.inner.dir.clone();
        run_db(&self.inner.db, move |conn| {
            let name =
                store::get(conn, &user)?.map_or_else(|| store::short_name(&user), |f| f.name);
            store::block(conn, &user, &name)?;
            store::outbox_delete(conn, &user)?;
            dir.reload(conn)
        })
        .await?;
        // Die Allowlist wirkt nur im Handshake: laufende Verbindungen jetzt trennen.
        self.inner.net.disconnect_user(&user);
        Ok(())
    }

    /// Hebt eine Blockierung auf oder entfernt einen Freund. Danach sind neue Anfragen moeglich.
    pub async fn remove(&self, user: PublicKey) -> Result<(), FriendsError> {
        let dir = self.inner.dir.clone();
        run_db(&self.inner.db, move |conn| {
            store::delete(conn, &user)?;
            store::outbox_delete(conn, &user)?;
            dir.reload(conn)
        })
        .await?;
        // Kein Freund mehr = nicht mehr auf der Allowlist: Verbindung ebenfalls trennen.
        self.inner.net.disconnect_user(&user);
        Ok(())
    }

    // ---------------------------------------------------------- Hello-Server

    /// Behandelt eine eingehende Verbindung auf `app/hello/1` (aus `NetEvent::Hello`).
    pub fn spawn_hello_handler(&self, conn: iroh::endpoint::Connection) {
        let Ok(permit) = self.inner.hello_slots.clone().try_acquire_owned() else {
            conn.close(1u32.into(), b"busy");
            return;
        };
        let svc = self.clone();
        tokio::spawn(async move {
            svc.handle_connection(conn).await;
            drop(permit);
        });
    }

    async fn handle_connection(&self, conn: iroh::endpoint::Connection) {
        let from = *conn.remote_id().as_bytes();
        let msg = tokio::time::timeout(HELLO_READ_TIMEOUT, async {
            let (send, mut recv) = conn.accept_bi().await.map_err(|_| ())?;
            // Fremde duerfen das schicken: kleine Grenze statt der allgemeinen 1 MiB.
            let msg: HelloMessage = read_frame_max(&mut recv, MAX_HELLO_LEN)
                .await
                .map_err(|_| ())?;
            Ok::<_, ()>((send, msg))
        })
        .await;
        let Ok(Ok((mut send, msg))) = msg else {
            conn.close(1u32.into(), b"bad hello");
            return;
        };
        let reply = self.process(from, msg, Instant::now()).await;
        if write_frame(&mut send, &reply).await.is_ok() {
            let _ = send.finish();
            let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
        }
        conn.close(0u32.into(), b"done");
    }

    /// Verarbeitet eine Hello-Nachricht von `from` (Node-ID der Verbindung).
    pub async fn process(&self, from: PublicKey, msg: HelloMessage, now: Instant) -> HelloReply {
        if !self
            .inner
            .limiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .allow(&from, now)
        {
            return HelloReply::RateLimited;
        }
        if !msg.is_sane() {
            return HelloReply::Invalid;
        }
        let dir = self.inner.dir.clone();
        let events = self.inner.events.clone();
        let my_user = self
            .inner
            .me
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cert
            .user;
        let r = run_db(&self.inner.db, move |conn| {
            Ok(apply_hello(conn, &dir, &events, my_user, from, msg))
        })
        .await;
        match r {
            Ok(reply) => reply,
            Err(_) => HelloReply::RateLimited,
        }
    }

    // ---------------------------------------------------------- Outbox

    async fn outbox_loop(self) {
        loop {
            let _ =
                tokio::time::timeout(self.inner.cfg.outbox_poll, self.inner.kick.notified()).await;
            self.deliver_due().await;
        }
    }

    /// Stellt alle faelligen Outbox-Eintraege zu.
    pub async fn deliver_due(&self) {
        let Ok(entries) = run_db(&self.inner.db, |c| store::outbox_due(c, now_secs())).await else {
            return;
        };
        let mut set = tokio::task::JoinSet::new();
        for e in entries {
            let svc = self.clone();
            set.spawn(async move { svc.deliver(e).await });
        }
        while set.join_next().await.is_some() {}
    }

    async fn deliver(&self, e: OutboxEntry) {
        let msg = self.build_message(&e).await;
        let Some(msg) = msg else {
            let _ = run_db(&self.inner.db, move |c| store::outbox_delete(c, &e.user)).await;
            return;
        };
        let res =
            tokio::time::timeout(self.inner.cfg.deliver_timeout, self.send_hello(&e, &msg)).await;
        let user = e.user;
        let done = matches!(
            res,
            Ok(Ok(HelloReply::Received
                | HelloReply::Blocked
                | HelloReply::Invalid))
        );
        let _ = run_db(&self.inner.db, move |c| {
            if done {
                store::outbox_delete(c, &user)
            } else {
                store::outbox_failed(c, &user, now_secs())
            }
        })
        .await;
    }

    async fn build_message(&self, e: &OutboxEntry) -> Option<HelloMessage> {
        let (cert, name) = {
            let me = self
                .inner
                .me
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (me.cert.clone(), me.name.clone())
        };
        match e.kind {
            OutboxKind::Decline => Some(HelloMessage::Decline),
            OutboxKind::Accept => Some(HelloMessage::Accept {
                cert,
                display_name: name,
            }),
            OutboxKind::Request => {
                let user = e.user;
                let f = run_db(&self.inner.db, move |c| store::get(c, &user))
                    .await
                    .ok()??;
                (f.status == FriendStatus::PendingOut).then(|| HelloMessage::Request {
                    cert,
                    display_name: name,
                    note: f.note.unwrap_or_default(),
                })
            }
        }
    }

    async fn send_hello(&self, e: &OutboxEntry, msg: &HelloMessage) -> Result<HelloReply, String> {
        let id = EndpointId::from_bytes(&e.node_id).map_err(|e| e.to_string())?;
        let mut addr = EndpointAddr::new(id);
        if let Some(r) = e.relay_url.as_deref().and_then(|r| r.parse().ok()) {
            addr = addr.with_relay_url(r);
        }
        if let Some(h) = self
            .inner
            .hints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&e.node_id)
        {
            for s in h {
                addr = addr.with_ip_addr(*s);
            }
        }
        let conn = self
            .inner
            .net
            .connect_hello(addr)
            .await
            .map_err(|e| e.to_string())?;
        let res = async {
            let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
            write_frame(&mut send, msg)
                .await
                .map_err(|e| e.to_string())?;
            send.finish().map_err(|e| e.to_string())?;
            read_frame_max::<HelloReply>(&mut recv, MAX_HELLO_LEN)
                .await
                .map_err(|e| e.to_string())
        }
        .await;
        conn.close(0u32.into(), b"done");
        res
    }
}

/// Fuehrt eine DB-Operation auf dem DB-Thread aus, ohne einen Tokio-Worker zu blockieren.
async fn run_db<R: Send + 'static>(
    db: &Db,
    f: impl FnOnce(&mut Connection) -> rusqlite::Result<R> + Send + 'static,
) -> Result<R, StorageError> {
    let db = db.clone();
    tokio::task::spawn_blocking(move || db.call(f))
        .await
        .map_err(|_| StorageError::Closed)?
}

/// Kern der Hello-Logik (laeuft auf dem DB-Thread).
fn apply_hello(
    conn: &mut Connection,
    dir: &FriendsDirectory,
    events: &mpsc::UnboundedSender<FriendEvent>,
    my_user: PublicKey,
    from: PublicKey,
    msg: HelloMessage,
) -> HelloReply {
    let res = (|| -> rusqlite::Result<HelloReply> {
        let tx = conn.transaction()?;
        let reply = match msg {
            HelloMessage::Request {
                cert,
                display_name,
                note,
            } => {
                if !verify_cert(&cert) || cert.device != from || cert.user == my_user {
                    return Ok(HelloReply::Invalid);
                }
                let name = clean(&display_name, MAX_NAME_LEN);
                let name = if name.is_empty() {
                    store::short_name(&cert.user)
                } else {
                    name
                };
                let note = clean(&note, MAX_NOTE_LEN);
                match store::get(&tx, &cert.user)? {
                    // Nicht verraten, dass blockiert wurde: fuer den Absender sieht es aus wie
                    // eine unbeantwortete Anfrage (sein Postausgang hoert ebenso auf).
                    Some(f) if f.status == FriendStatus::Blocked => HelloReply::Received,
                    Some(f) if f.status == FriendStatus::Accepted => {
                        store::add_device(&tx, &cert.user, &cert)?;
                        // Der Freund hat uns offenbar nicht als Freund: Zusage erneut zustellen.
                        store::outbox_put(
                            &tx,
                            &cert.user,
                            OutboxKind::Accept,
                            &cert.device,
                            None,
                            now_secs(),
                        )?;
                        HelloReply::Received
                    }
                    Some(f) if f.status == FriendStatus::PendingOut => {
                        // Gegenseitige Anfrage: sofort Freunde.
                        store::set_status(&tx, &cert.user, FriendStatus::Accepted)?;
                        store::set_name_note(&tx, &cert.user, &name, None)?;
                        store::add_device(&tx, &cert.user, &cert)?;
                        store::outbox_put(
                            &tx,
                            &cert.user,
                            OutboxKind::Accept,
                            &cert.device,
                            None,
                            now_secs(),
                        )?;
                        let _ = events.send(FriendEvent::Accepted { user: cert.user });
                        HelloReply::Received
                    }
                    Some(_) => {
                        store::set_name_note(&tx, &cert.user, &name, Some(&note))?;
                        store::add_device(&tx, &cert.user, &cert)?;
                        HelloReply::Received
                    }
                    None => {
                        match store::upsert_pending_in(&tx, &cert, &name, &note, MAX_PENDING_IN)? {
                            InResult::Stored => {
                                let _ = events.send(FriendEvent::RequestReceived {
                                    user: cert.user,
                                    name,
                                    note,
                                });
                                HelloReply::Received
                            }
                            InResult::Full => HelloReply::RateLimited,
                        }
                    }
                }
            }
            HelloMessage::Accept { cert, display_name } => {
                if !verify_cert(&cert) || cert.device != from {
                    return Ok(HelloReply::Invalid);
                }
                match store::get(&tx, &cert.user)? {
                    Some(f) if f.status == FriendStatus::PendingOut => {
                        let name = clean(&display_name, MAX_NAME_LEN);
                        let name = if name.is_empty() { f.name } else { name };
                        store::set_status(&tx, &cert.user, FriendStatus::Accepted)?;
                        store::set_name_note(&tx, &cert.user, &name, None)?;
                        store::add_device(&tx, &cert.user, &cert)?;
                        store::outbox_delete(&tx, &cert.user)?;
                        let _ = events.send(FriendEvent::Accepted { user: cert.user });
                        HelloReply::Received
                    }
                    Some(f) if f.status == FriendStatus::Accepted => {
                        store::add_device(&tx, &cert.user, &cert)?;
                        HelloReply::Received
                    }
                    _ => HelloReply::Invalid,
                }
            }
            HelloMessage::Decline => {
                let target = store::list(&tx)?
                    .into_iter()
                    .find(|f| f.status == FriendStatus::PendingOut && f.node_ids().contains(&from));
                match target {
                    Some(f) => {
                        store::delete(&tx, &f.user)?;
                        store::outbox_delete(&tx, &f.user)?;
                        let _ = events.send(FriendEvent::Declined { user: f.user });
                        HelloReply::Received
                    }
                    None => HelloReply::Invalid,
                }
            }
        };
        dir.reload(&tx)?;
        tx.commit()?;
        Ok(reply)
    })();
    res.unwrap_or(HelloReply::RateLimited)
}

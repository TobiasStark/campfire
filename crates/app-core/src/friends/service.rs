//! Der Freunde-Dienst: Anfragen senden/annehmen, Outbox-Zustellung, Hello-Server.

use super::limiter::{Limits, RateLimiter};
use super::store::{self, Friend, FriendStatus, OutboxEntry, OutboxKind};
use super::FriendsDirectory;
use crate::storage::{Db, StorageError};
use app_net::{read_frame_max, verify_binding, verify_cert, verify_rotation, write_frame, Net};
use app_proto::control::DeviceCert;
use app_proto::friend_code::{FriendCode, FriendCodeError};
use app_proto::hello::{HelloMessage, HelloReply, MAX_NAME_LEN, MAX_NOTE_LEN};
use app_proto::lock::Locked;
use app_proto::recovery::Rotation;
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
/// Kurz (N-1): sonst blockieren langsame Verbindungen die wenigen Hello-Plaetze.
const HELLO_READ_TIMEOUT: Duration = Duration::from_secs(2);
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
    #[error("{} {}", app_proto::i18n::tr("Invalid friend code:", "Ungültiger Freundescode:"), .0)]
    Code(#[from] FriendCodeError),
    #[error(
        "{}",
        app_proto::i18n::tr("That is your own code", "Das ist dein eigener Code")
    )]
    SelfAdd,
    #[error(
        "{}",
        app_proto::i18n::tr("You are already friends", "Ihr seid bereits befreundet")
    )]
    AlreadyFriends,
    #[error(
        "{}",
        app_proto::i18n::tr("This user is blocked", "Dieser Nutzer ist blockiert")
    )]
    Blocked,
    #[error("{}", app_proto::i18n::tr("Unknown user", "Unbekannter Nutzer"))]
    NotFound,
    #[error("Storage: {0}")]
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
    /// Ein Freund hat seinen Schluessel gewechselt (M-7); `old` ist jetzt gesperrt.
    Rotated {
        old: PublicKey,
        new: PublicKey,
    },
}

struct Me {
    cert: DeviceCert,
    name: String,
    /// Rendezvous-Endpoint und seine Bindung an dieses Geraet (M-5).
    rendezvous: PublicKey,
    binding: app_proto::Signature,
    /// Schluessel der Einladungsgeheimnisse (`Identity::invite_key`).
    invite_key: zeroize::Zeroizing<[u8; 32]>,
    /// Eigener letzter Schluesselwechsel (M-7), falls er zu `cert.user` fuehrt.
    rotation: Option<Rotation>,
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

/// Namen und Notizen von Fremden: zentrale Bereinigung (unsichtbare Zeichen, Bidi, NFC).
fn clean(s: &str, max_chars: usize) -> String {
    app_proto::text::clean_name(s, max_chars)
}

impl FriendsService {
    /// Laedt die Allowlist und startet die Outbox-Zustellung.
    pub async fn start(
        db: Db,
        net: Net,
        dir: Arc<FriendsDirectory>,
        cert: DeviceCert,
        display_name: String,
        invite_key: zeroize::Zeroizing<[u8; 32]>,
        cfg: ServiceConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<FriendEvent>), FriendsError> {
        let d = dir.clone();
        run_db(&db, move |c| d.reload(c)).await?;
        let rotation = run_db(&db, |c| Ok(crate::identity::own_rotation(c).ok().flatten()))
            .await?
            .filter(|r| r.new == cert.user);
        let (tx, rx) = mpsc::unbounded_channel();
        let (rendezvous, binding) = net.rendezvous();
        let svc = Self {
            inner: Arc::new(Inner {
                db,
                net,
                dir,
                me: Mutex::new(Me {
                    cert,
                    name: display_name,
                    rendezvous,
                    binding,
                    invite_key,
                    rotation,
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
            .locked()
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
    /// Eigener Freundescode (aktuelle Generation).
    pub async fn my_code(&self, relay_url: Option<String>) -> Result<String, FriendsError> {
        let epoch = run_db(&self.inner.db, |c| store::code_epoch(c)).await?;
        self.code_for(relay_url, epoch)
    }

    /// Neuer Freundescode; alte Codes nehmen keine Anfragen mehr an (N-1).
    pub async fn rotate_code(&self, relay_url: Option<String>) -> Result<String, FriendsError> {
        let epoch = run_db(&self.inner.db, |c| store::bump_code_epoch(c)).await?;
        self.code_for(relay_url, epoch)
    }

    fn code_for(&self, relay_url: Option<String>, epoch: u64) -> Result<String, FriendsError> {
        let me = self.inner.me.locked();
        Ok(FriendCode {
            user: me.cert.user,
            // Nur der Rendezvous-Endpoint; die Geraete-ID erfahren erst Freunde (M-5).
            node_id: me.rendezvous,
            secret: super::invite_secret(&me.invite_key, epoch),
            relay_url,
        }
        .encode()?)
    }

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
            if let Some(node) = f.rendezvous() {
                store::outbox_put(conn, &user, OutboxKind::Accept, &node, None, now_secs())?;
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
            if let Some(node) = f.rendezvous() {
                store::outbox_put(conn, &user, OutboxKind::Decline, &node, None, now_secs())?;
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
                store::get(conn, &user)?.map_or_else(|| store::short_name(&user), |f| f.chosen);
            store::block(conn, &user, &name)?;
            store::outbox_delete(conn, &user)?;
            dir.reload(conn)
        })
        .await?;
        // Die Allowlist wirkt nur im Handshake: laufende Verbindungen jetzt trennen.
        self.inner.net.disconnect_user(&user);
        Ok(())
    }

    /// Lokaler Spitzname fuer einen Freund (leer = loeschen). Er hat Vorrang vor dem selbst
    /// gewaehlten Namen, damit ein Umbenennen des Freundes nicht stillschweigend wirkt (M-8).
    pub async fn set_nickname(&self, user: PublicKey, nickname: &str) -> Result<(), FriendsError> {
        let nick = clean(nickname, MAX_NAME_LEN);
        let dir = self.inner.dir.clone();
        run_db(&self.inner.db, move |conn| {
            store::set_nickname(conn, &user, (!nick.is_empty()).then_some(nick.as_str()))?;
            dir.reload(conn)
        })
        .await?;
        Ok(())
    }

    /// Markiert den Fingerprint eines Freundes als (nicht) verglichen.
    /// Haelt den Hash des Wiederherstellungsschluessels eines Freundes aus dessen Handshake
    /// fest; nur der erste zaehlt (M-7).
    pub async fn note_recovery(
        &self,
        user: PublicKey,
        commitment: app_proto::Hash,
    ) -> Result<(), FriendsError> {
        run_db(&self.inner.db, move |c| {
            store::note_recovery(c, &user, &commitment)
        })
        .await?;
        Ok(())
    }

    pub async fn set_verified(&self, user: PublicKey, verified: bool) -> Result<(), FriendsError> {
        run_db(&self.inner.db, move |conn| {
            store::set_verified(conn, &user, verified)
        })
        .await?;
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
        let (my_user, key) = {
            let me = self.inner.me.locked();
            (me.cert.user, me.invite_key.clone())
        };
        let r = run_db(&self.inner.db, move |conn| {
            Ok(apply_hello(conn, &dir, &events, my_user, &key, from, msg))
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
        // Schluesselwechsel erst nach dem Neustart mit dem neuen Schluessel melden.
        if e.kind == OutboxKind::Rotate && self.inner.me.locked().rotation.is_none() {
            return;
        }
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
        let (cert, name, binding, rotation) = {
            let me = self.inner.me.locked();
            (
                me.cert.clone(),
                me.name.clone(),
                me.binding,
                me.rotation.clone().map(Box::new),
            )
        };
        match e.kind {
            OutboxKind::Decline => Some(HelloMessage::Decline),
            OutboxKind::Accept => Some(HelloMessage::Accept {
                cert,
                display_name: name,
                binding,
            }),
            OutboxKind::Request => {
                let user = e.user;
                let (f, secret) = run_db(&self.inner.db, move |c| {
                    Ok((store::get(c, &user)?, store::invite_secret_of(c, &user)?))
                })
                .await
                .ok()?;
                let f = f?;
                (f.status == FriendStatus::PendingOut).then(|| HelloMessage::Request {
                    cert,
                    display_name: name,
                    note: f.note.unwrap_or_default(),
                    // Fehlt nur bei Anfragen aus alten Versionen; der Empfaenger lehnt dann ab.
                    secret: secret.unwrap_or_default(),
                    binding,
                    rotation,
                })
            }
            // Meldung an einen Freund: Anfrage mit Rotation, ohne Einladungsgeheimnis.
            OutboxKind::Rotate => {
                let user = e.user;
                let f = run_db(&self.inner.db, move |c| store::get(c, &user))
                    .await
                    .ok()??;
                (f.status == FriendStatus::Accepted && rotation.is_some()).then(|| {
                    HelloMessage::Request {
                        cert,
                        display_name: name,
                        note: String::new(),
                        secret: Default::default(),
                        binding,
                        rotation,
                    }
                })
            }
        }
    }

    async fn send_hello(&self, e: &OutboxEntry, msg: &HelloMessage) -> Result<HelloReply, String> {
        let id = EndpointId::from_bytes(&e.node_id).map_err(|e| e.to_string())?;
        let mut addr = EndpointAddr::new(id);
        if let Some(r) = e
            .relay_url
            .as_deref()
            .filter(|r| app_proto::urls::is_public_https(r))
            .and_then(|r| r.parse().ok())
        {
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

/// Freund, dessen Schluessel `rot` ersetzt, falls die Rotation gilt (M-7): unterschrieben mit
/// dem Wiederherstellungsschluessel, dessen Hash wir fuer den alten Schluessel gespeichert
/// haben, auf den Schluessel des Zertifikats; der alte ist Freund, der neue nicht gesperrt.
fn rotation_target(
    conn: &Connection,
    rot: &Rotation,
    cert: &DeviceCert,
) -> rusqlite::Result<Option<Friend>> {
    if rot.new != cert.user || !verify_rotation(rot) {
        return Ok(None);
    }
    if store::recovery_of(conn, &rot.old)? != Some(app_proto::recovery::commitment(&rot.recovery)) {
        return Ok(None);
    }
    let Some(old) = store::get(conn, &rot.old)?.filter(|f| f.status == FriendStatus::Accepted)
    else {
        return Ok(None);
    };
    if store::get(conn, &rot.new)?.is_some_and(|f| f.status == FriendStatus::Blocked) {
        return Ok(None);
    }
    Ok(Some(old))
}

/// Kern der Hello-Logik (laeuft auf dem DB-Thread).
#[allow(clippy::too_many_arguments)]
fn apply_hello(
    conn: &mut Connection,
    dir: &FriendsDirectory,
    events: &mpsc::UnboundedSender<FriendEvent>,
    my_user: PublicKey,
    invite_key: &[u8; 32],
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
                secret,
                binding,
                rotation,
            } => {
                // `from` ist der Rendezvous-Endpoint des Absenders; das Geraet bestaetigt ihn.
                if !verify_cert(&cert)
                    || !verify_binding(&cert.device, &from, &binding)
                    || cert.user == my_user
                {
                    return Ok(HelloReply::Invalid);
                }
                if let Some(rot) = rotation {
                    if let Some(old) = rotation_target(&tx, &rot, &cert)? {
                        store::apply_rotation(&tx, &old, &cert, &from, &rot.next)?;
                        store::outbox_put(
                            &tx,
                            &cert.user,
                            OutboxKind::Accept,
                            &from,
                            None,
                            now_secs(),
                        )?;
                        dir.reload(&tx)?;
                        tx.commit()?;
                        let _ = events.send(FriendEvent::Rotated {
                            old: rot.old,
                            new: rot.new,
                        });
                        return Ok(HelloReply::Received);
                    }
                }
                // Einladungsgeheimnis (N-1): neue Anfragen nur mit dem aktuellen Code. Freunde
                // (neues Geraet) und Gegenanfragen auf meine eigene Anfrage brauchen es nicht.
                let expected = super::invite_secret(invite_key, store::code_epoch(&tx)?);
                let secret_ok = super::ct_eq(&secret, &expected);
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
                        store::set_rendezvous(&tx, &cert.user, &from)?;
                        // Der Freund hat uns offenbar nicht als Freund: Zusage erneut zustellen.
                        store::outbox_put(
                            &tx,
                            &cert.user,
                            OutboxKind::Accept,
                            &from,
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
                        store::set_rendezvous(&tx, &cert.user, &from)?;
                        store::outbox_put(
                            &tx,
                            &cert.user,
                            OutboxKind::Accept,
                            &from,
                            None,
                            now_secs(),
                        )?;
                        let _ = events.send(FriendEvent::Accepted { user: cert.user });
                        HelloReply::Received
                    }
                    _ if !secret_ok => HelloReply::Invalid,
                    Some(_) => {
                        store::set_name_note(&tx, &cert.user, &name, Some(&note))?;
                        store::add_device(&tx, &cert.user, &cert)?;
                        store::set_rendezvous(&tx, &cert.user, &from)?;
                        HelloReply::Received
                    }
                    None => {
                        store::upsert_pending_in(&tx, &cert, &from, &name, &note, MAX_PENDING_IN)?;
                        let _ = events.send(FriendEvent::RequestReceived {
                            user: cert.user,
                            name,
                            note,
                        });
                        HelloReply::Received
                    }
                }
            }
            HelloMessage::Accept {
                cert,
                display_name,
                binding,
            } => {
                if !verify_cert(&cert) || !verify_binding(&cert.device, &from, &binding) {
                    return Ok(HelloReply::Invalid);
                }
                match store::get(&tx, &cert.user)? {
                    Some(f) if f.status == FriendStatus::PendingOut => {
                        let name = clean(&display_name, MAX_NAME_LEN);
                        let name = if name.is_empty() { f.chosen } else { name };
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
                    .find(|f| f.status == FriendStatus::PendingOut && f.rendezvous() == Some(from));
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

//! Der Netzwerkknoten: Endpoint, Handshake, Peer-Register, Reconnect.

use crate::{
    read_frame, read_frame_max, verify_cert, write_frame, FrameError, CHANNEL_SYNC,
    SUPPORTED_VERSIONS,
};
use app_proto::control::{ControlMessage, Handshake, HandshakeReply, RejectReason};
use app_proto::{Hash, PublicKey};
use bytes::Bytes;
use iroh::endpoint::{presets, Connection, QuicTransportConfig, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tokio::task::{AbortHandle, JoinSet};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Groesster Handshake-Frame (Zertifikat, Name, Versionen: weit unter 1 KB). Fremde duerfen
/// ihn schicken, deshalb nicht die allgemeine Grenze von 1 MiB.
const MAX_HANDSHAKE_LEN: usize = 4096;
/// So viele noch nicht authentifizierte Handshakes laufen hoechstens gleichzeitig; weitere
/// Verbindungen werden sofort abgewiesen (PLAN 8: Rate-Limit fuer Fremde).
const MAX_PENDING_HANDSHAKES: usize = 64;
/// Hello-/Datei-Verbindungen (auch von Fremden) duerfen den Ereigniskanal nur bis auf diese
/// Reserve fuellen; der Rest bleibt fuer Nachrichten der Freunde.
const EVENT_RESERVE: usize = 256;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const CLOSE_DUPLICATE: u32 = 2;
const CLOSE_REJECTED: u32 = 3;
const CLOSE_SHUTDOWN: u32 = 0;
const MAX_NAME_BYTES: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("Endpoint konnte nicht gestartet werden: {0}")]
    Bind(String),
    #[error("Verbindung fehlgeschlagen: {0}")]
    Connect(String),
    #[error("Handshake: {0}")]
    Handshake(String),
    #[error("Gegenseite hat abgelehnt: {0:?}")]
    Rejected(RejectReason),
    #[error("Doppelte Verbindung, die andere gewinnt")]
    Duplicate,
    #[error("Peer ist nicht verbunden")]
    NotConnected,
    #[error("Senden fehlgeschlagen: {0}")]
    Send(String),
    #[error("Ungueltige Konfiguration: {0}")]
    Config(String),
}

/// Ergebnis der Allowlist-Pruefung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Allowed,
    Unknown,
    Blocked,
}

/// Allowlist fuer `app/peer/1`: Freunde und Mitglieder gemeinsamer Spaces.
pub trait PeerDirectory: Send + Sync + 'static {
    fn check(&self, user: &PublicKey) -> Access;
}

#[derive(Debug, Clone)]
pub enum RelayConfig {
    /// n0-Relays.
    Default,
    /// Eigene Relay-URLs.
    Custom(Vec<String>),
    /// Nur ueber Relay, keine direkten Pfade (verbirgt die IP vor Freunden).
    RelayOnly,
}

#[derive(Debug, Clone)]
pub enum NetMode {
    /// Normalbetrieb mit Discovery und Relay.
    Internet(RelayConfig),
    /// Nur Loopback ohne Relay/Discovery (Tests).
    Loopback,
}

#[derive(Clone)]
pub struct NetConfig {
    pub device_secret: [u8; 32],
    /// Eigener Handshake (Zertifikat, Anzeigename, Avatar).
    pub handshake: Handshake,
    pub directory: Arc<dyn PeerDirectory>,
    pub mode: NetMode,
    pub bind_addr: Option<SocketAddr>,
    pub keep_alive: Duration,
    pub idle_timeout: Duration,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl NetConfig {
    pub fn new(
        device_secret: [u8; 32],
        handshake: Handshake,
        directory: Arc<dyn PeerDirectory>,
    ) -> Self {
        Self {
            device_secret,
            handshake,
            directory,
            mode: NetMode::Internet(RelayConfig::Default),
            bind_addr: None,
            keep_alive: Duration::from_secs(2),
            idle_timeout: Duration::from_secs(6),
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub user: PublicKey,
    pub device: PublicKey,
    pub display_name: String,
    pub avatar_hash: Option<Hash>,
    pub version: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Direct,
    Relay,
    None,
}

pub enum NetEvent {
    PeerUp(PeerInfo),
    PeerDown {
        device: PublicKey,
    },
    Control {
        from: PeerInfo,
        msg: ControlMessage,
    },
    Datagram {
        from: PublicKey,
        data: Bytes,
    },
    SyncStream {
        from: PublicKey,
        send: SendStream,
        recv: RecvStream,
    },
    VideoStream {
        from: PublicKey,
        recv: RecvStream,
    },
    /// Verbindung auf `app/hello/1` (M1-05 behandelt sie, mit Rate-Limit).
    Hello(Connection),
    /// Eingehende Datei-Verbindung (iroh-blobs); der Empfaenger prueft die Berechtigung.
    Blobs(Connection),
}

struct PeerEntry {
    id: u64,
    info: PeerInfo,
    conn: Connection,
    initiator: EndpointId,
    control_tx: mpsc::Sender<ControlMessage>,
}

struct Inner {
    ep: Endpoint,
    cfg_handshake: Handshake,
    directory: Arc<dyn PeerDirectory>,
    initial_backoff: Duration,
    max_backoff: Duration,
    events: mpsc::Sender<NetEvent>,
    peers: Mutex<HashMap<EndpointId, PeerEntry>>,
    targets: Mutex<HashMap<EndpointId, AbortHandle>>,
    next_id: AtomicU64,
    /// Wird bei jeder Aenderung am Peer-Register oder bei Netzwechsel ausgeloest.
    changed: Notify,
    retry_now: Notify,
    accept_task: Mutex<Option<AbortHandle>>,
}

#[derive(Clone)]
pub struct Net {
    inner: Arc<Inner>,
}

fn eid(bytes: &PublicKey) -> Option<EndpointId> {
    EndpointId::from_bytes(bytes).ok()
}

impl Net {
    /// Startet den Endpoint. Ereignisse kommen ueber den zurueckgegebenen Receiver.
    pub async fn start(cfg: NetConfig) -> Result<(Net, mpsc::Receiver<NetEvent>), NetError> {
        let secret = SecretKey::from_bytes(&cfg.device_secret);
        let tc = QuicTransportConfig::builder()
            .keep_alive_interval(cfg.keep_alive)
            .max_idle_timeout(Some(
                cfg.idle_timeout
                    .try_into()
                    .map_err(|_| NetError::Config("idle_timeout".into()))?,
            ))
            .build();
        let alpns = vec![
            app_proto::alpn::PEER.to_vec(),
            app_proto::alpn::HELLO.to_vec(),
            app_proto::alpn::BLOBS.to_vec(),
        ];
        let mut b = match &cfg.mode {
            NetMode::Loopback => {
                Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled)
            }
            NetMode::Internet(relay) => {
                let b = Endpoint::builder(presets::N0);
                match relay {
                    RelayConfig::Default => b,
                    RelayConfig::RelayOnly => b.clear_ip_transports(),
                    RelayConfig::Custom(urls) => {
                        let map = iroh::RelayMap::try_from_iter(urls.iter().map(String::as_str))
                            .map_err(|e| NetError::Config(format!("Relay-URL: {e}")))?;
                        b.relay_mode(RelayMode::Custom(map))
                    }
                }
            }
        };
        b = b.secret_key(secret).alpns(alpns).transport_config(tc);
        if let Some(addr) = cfg.bind_addr {
            b = b
                .bind_addr(addr)
                .map_err(|e| NetError::Config(e.to_string()))?;
        }
        let ep = b.bind().await.map_err(|e| NetError::Bind(e.to_string()))?;

        let (tx, rx) = mpsc::channel(1024);
        let inner = Arc::new(Inner {
            ep,
            cfg_handshake: cfg.handshake,
            directory: cfg.directory,
            initial_backoff: cfg.initial_backoff,
            max_backoff: cfg.max_backoff,
            events: tx,
            peers: Mutex::default(),
            targets: Mutex::default(),
            next_id: AtomicU64::new(1),
            changed: Notify::new(),
            retry_now: Notify::new(),
            accept_task: Mutex::default(),
        });
        let net = Net { inner };
        let accept = tokio::spawn(net.clone().accept_loop()).abort_handle();
        *net.inner
            .accept_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(accept);
        Ok((net, rx))
    }

    /// Der iroh-Endpoint (fuer Datei-Downloads).
    pub fn endpoint(&self) -> Endpoint {
        self.inner.ep.clone()
    }

    pub fn device_id(&self) -> PublicKey {
        *self.inner.ep.id().as_bytes()
    }

    /// Adresse fuer Direktverbindungen (Loopback-Tests, Freundescode).
    pub fn addr(&self) -> EndpointAddr {
        let mut a = EndpointAddr::new(self.inner.ep.id());
        for s in self.inner.ep.bound_sockets() {
            a = a.with_ip_addr(s);
        }
        a
    }

    /// Loopback-Adresse mit den gebundenen Ports.
    pub fn loopback_addr(&self) -> EndpointAddr {
        let mut a = EndpointAddr::new(self.inner.ep.id());
        for s in self.inner.ep.bound_sockets() {
            if s.is_ipv4() {
                a = a.with_ip_addr(SocketAddr::new([127, 0, 0, 1].into(), s.port()));
            }
        }
        a
    }

    /// Der Netzwerkstatus hat sich geaendert (WLAN weg/da, Standby-Ende): sofort neu versuchen.
    pub async fn network_changed(&self) {
        self.inner.ep.network_change().await;
        self.inner.retry_now.notify_waiters();
    }

    pub fn peers(&self) -> Vec<PeerInfo> {
        let g = self
            .inner
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.values().map(|p| p.info.clone()).collect()
    }

    pub fn is_connected(&self, device: &PublicKey) -> bool {
        eid(device).is_some_and(|id| {
            self.inner
                .peers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&id)
        })
    }

    /// Direkt oder ueber Relay (fuer die UI).
    pub fn path_kind(&self, device: &PublicKey) -> PathKind {
        let Some(id) = eid(device) else {
            return PathKind::None;
        };
        let Some(conn) = self
            .inner
            .peers
            .lock()
            .unwrap()
            .get(&id)
            .map(|p| p.conn.clone())
        else {
            return PathKind::None;
        };
        let paths = conn.paths();
        match paths
            .iter()
            .find(|p| p.is_selected())
            .or_else(|| paths.iter().next())
        {
            Some(p) if p.is_relay() => PathKind::Relay,
            Some(p) if p.is_ip() => PathKind::Direct,
            _ => PathKind::None,
        }
    }

    fn conn_of(&self, device: &PublicKey) -> Result<Connection, NetError> {
        let id = eid(device).ok_or(NetError::NotConnected)?;
        self.inner
            .peers
            .lock()
            .unwrap()
            .get(&id)
            .map(|p| p.conn.clone())
            .ok_or(NetError::NotConnected)
    }

    pub async fn send_control(
        &self,
        device: &PublicKey,
        msg: ControlMessage,
    ) -> Result<(), NetError> {
        let id = eid(device).ok_or(NetError::NotConnected)?;
        let tx = self
            .inner
            .peers
            .lock()
            .unwrap()
            .get(&id)
            .map(|p| p.control_tx.clone())
            .ok_or(NetError::NotConnected)?;
        tx.send(msg).await.map_err(|_| NetError::NotConnected)
    }

    pub fn send_datagram(&self, device: &PublicKey, data: Bytes) -> Result<(), NetError> {
        self.conn_of(device)?
            .send_datagram(data)
            .map_err(|e| NetError::Send(e.to_string()))
    }

    /// Oeffnet einen Sync-Stream (Kanal-Byte ist schon gesendet).
    pub async fn open_sync(
        &self,
        device: &PublicKey,
    ) -> Result<(SendStream, RecvStream), NetError> {
        let (mut s, r) = self
            .conn_of(device)?
            .open_bi()
            .await
            .map_err(|e| NetError::Send(e.to_string()))?;
        s.write_all(&[CHANNEL_SYNC])
            .await
            .map_err(|e| NetError::Send(e.to_string()))?;
        Ok((s, r))
    }

    /// Verbindung auf `app/hello/1` zu einem (evtl. unbekannten) Geraet.
    pub async fn connect_hello(&self, addr: EndpointAddr) -> Result<Connection, NetError> {
        tokio::time::timeout(
            CONNECT_TIMEOUT,
            self.inner.ep.connect(addr, app_proto::alpn::HELLO),
        )
        .await
        .map_err(|_| NetError::Connect("Zeitueberschreitung".into()))?
        .map_err(|e| NetError::Connect(e.to_string()))
    }

    /// Oeffnet einen unidirektionalen Video-Stream.
    pub async fn open_video(&self, device: &PublicKey) -> Result<SendStream, NetError> {
        self.conn_of(device)?
            .open_uni()
            .await
            .map_err(|e| NetError::Send(e.to_string()))
    }

    /// Haelt die Verbindung zu `addr` aufrecht (Reconnect mit exponentiellem Backoff).
    pub fn add_target(&self, addr: EndpointAddr) {
        let id = addr.id;
        let mut targets = self
            .inner
            .targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(old) = targets.remove(&id) {
            old.abort();
        }
        let h = tokio::spawn(self.clone().supervise(addr)).abort_handle();
        targets.insert(id, h);
    }

    pub fn remove_target(&self, device: &PublicKey) {
        if let Some(id) = eid(device) {
            if let Some(h) = self
                .inner
                .targets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id)
            {
                h.abort();
            }
        }
    }

    /// Trennt sofort alle Verbindungen zu Geraeten von `user` und gibt deren Reconnect-Ziele
    /// auf. Noetig nach Blockieren oder Entfernen: Die Allowlist wird nur im Handshake geprueft,
    /// eine schon laufende Verbindung bliebe sonst bestehen. Die Allowlist muss vorher
    /// aktualisiert sein, damit ein erneuter Verbindungsversuch abgewiesen wird.
    /// Gibt die getrennten Geraete zurueck.
    pub fn disconnect_user(&self, user: &PublicKey) -> Vec<PublicKey> {
        let victims: Vec<(EndpointId, Connection)> = self
            .inner
            .peers
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p)| p.info.user == *user)
            .map(|(id, p)| (*id, p.conn.clone()))
            .collect();
        for (id, conn) in &victims {
            // Erst das Ziel aufgeben, dann schliessen: sonst waehlt `supervise` sofort neu.
            if let Some(h) = self
                .inner
                .targets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id)
            {
                h.abort();
            }
            conn.close(CLOSE_REJECTED.into(), b"blocked");
        }
        victims.into_iter().map(|(id, _)| *id.as_bytes()).collect()
    }

    /// Trennt alle Verbindungen und beendet den Endpoint.
    pub async fn shutdown(&self) {
        for (_, h) in self
            .inner
            .targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
        {
            h.abort();
        }
        if let Some(h) = self
            .inner
            .accept_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            h.abort();
        }
        let conns: Vec<_> = self
            .inner
            .peers
            .lock()
            .unwrap()
            .values()
            .map(|p| p.conn.clone())
            .collect();
        for c in conns {
            c.close(CLOSE_SHUTDOWN.into(), b"shutdown");
        }
        self.inner.ep.close().await;
    }

    // ------------------------------------------------------------ intern

    async fn accept_loop(self) {
        let pending = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_HANDSHAKES));
        while let Some(incoming) = self.inner.ep.accept().await {
            // Platz fuer einen weiteren unauthentifizierten Verbindungsaufbau? Sonst sofort ab.
            let Ok(permit) = pending.clone().try_acquire_owned() else {
                incoming.refuse();
                continue;
            };
            let net = self.clone();
            tokio::spawn(async move {
                let Ok(conn) = incoming.await else { return };
                let alpn = conn.alpn().to_vec();
                if alpn == app_proto::alpn::PEER {
                    let _ = net.run_server(conn, permit).await;
                } else if alpn == app_proto::alpn::HELLO || alpn == app_proto::alpn::BLOBS {
                    drop(permit);
                    net.forward_unauthenticated(conn, alpn == app_proto::alpn::HELLO);
                } else {
                    conn.close(CLOSE_REJECTED.into(), b"alpn");
                }
            });
        }
    }

    async fn supervise(self, addr: EndpointAddr) {
        let id = addr.id;
        let mut backoff = self.inner.initial_backoff;
        loop {
            // Schon verbunden (z. B. eingehend)? Dann warten, bis die Verbindung weg ist.
            if self
                .inner
                .peers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&id)
            {
                backoff = self.inner.initial_backoff;
                let wait = self.inner.changed.notified();
                tokio::pin!(wait);
                wait.as_mut().enable();
                if self
                    .inner
                    .peers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(&id)
                {
                    let _ = tokio::time::timeout(Duration::from_secs(1), wait).await;
                }
                continue;
            }
            let started = tokio::time::Instant::now();
            let res = self.dial(addr.clone()).await;
            match res {
                Ok(()) | Err(NetError::Duplicate) => {
                    // Verbindung stand; lief sie lange genug, Backoff zuruecksetzen.
                    if started.elapsed() > Duration::from_secs(10) {
                        backoff = self.inner.initial_backoff;
                    }
                }
                Err(_) => {}
            }
            if self
                .inner
                .peers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&id)
            {
                continue;
            }
            let retry = self.inner.retry_now.notified();
            tokio::pin!(retry);
            retry.as_mut().enable();
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = &mut retry => { backoff = self.inner.initial_backoff; }
            }
            backoff = (backoff * 2).min(self.inner.max_backoff);
        }
    }

    /// Verbindet und liefert erst zurueck, wenn die Verbindung wieder geschlossen ist.
    async fn dial(&self, addr: EndpointAddr) -> Result<(), NetError> {
        let conn = tokio::time::timeout(
            CONNECT_TIMEOUT,
            self.inner.ep.connect(addr, app_proto::alpn::PEER),
        )
        .await
        .map_err(|_| NetError::Connect("Zeitueberschreitung".into()))?
        .map_err(|e| NetError::Connect(e.to_string()))?;
        self.run_client(conn).await
    }

    fn validate(&self, hs: &Handshake, remote: EndpointId) -> Result<u16, RejectReason> {
        if !verify_cert(&hs.cert) || hs.cert.device != *remote.as_bytes() {
            return Err(RejectReason::InvalidCert);
        }
        let version = SUPPORTED_VERSIONS
            .iter()
            .copied()
            .filter(|v| hs.versions.contains(v))
            .max()
            .ok_or(RejectReason::VersionMismatch)?;
        match self.inner.directory.check(&hs.cert.user) {
            Access::Allowed => Ok(version),
            Access::Unknown => Err(RejectReason::UnknownUser),
            Access::Blocked => Err(RejectReason::Blocked),
        }
    }

    fn own_handshake(&self) -> Handshake {
        let mut h = self.inner.cfg_handshake.clone();
        h.versions = SUPPORTED_VERSIONS.to_vec();
        h
    }

    async fn run_client(&self, conn: Connection) -> Result<(), NetError> {
        let remote = conn.remote_id();
        let hs_err = |e: FrameError| NetError::Handshake(e.to_string());
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| NetError::Handshake(e.to_string()))?;
        let reply = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            write_frame(&mut send, &self.own_handshake()).await?;
            read_frame_max::<HandshakeReply>(&mut recv, MAX_HANDSHAKE_LEN).await
        })
        .await
        .map_err(|_| NetError::Handshake("Zeitueberschreitung".into()))?
        .map_err(hs_err);
        let reply = match reply {
            Ok(r) => r,
            Err(e) => {
                conn.close(CLOSE_REJECTED.into(), b"handshake");
                return Err(e);
            }
        };
        match reply {
            HandshakeReply::Reject { reason } => {
                conn.close(CLOSE_REJECTED.into(), b"rejected");
                Err(NetError::Rejected(reason))
            }
            HandshakeReply::Accept { version, handshake } => {
                match self.validate(&handshake, remote) {
                    Ok(v) if v == version => {}
                    _ => {
                        conn.close(CLOSE_REJECTED.into(), b"invalid server handshake");
                        return Err(NetError::Handshake("Antwort des Servers ungueltig".into()));
                    }
                }
                self.run_session(conn, send, recv, *handshake, version, self.inner.ep.id())
                    .await
            }
        }
    }

    /// Hello und Dateien (auch von Fremden): an die App weiterreichen, ohne zu warten. Ist der
    /// Ereigniskanal schon bis auf die Reserve voll, wird die Verbindung abgewiesen, statt die
    /// Nachrichten der Freunde zu verzoegern.
    fn forward_unauthenticated(&self, conn: Connection, hello: bool) {
        if self.inner.events.capacity() <= EVENT_RESERVE {
            conn.close(CLOSE_REJECTED.into(), b"busy");
            return;
        }
        let ev = if hello {
            NetEvent::Hello(conn)
        } else {
            NetEvent::Blobs(conn)
        };
        if let Err(e) = self.inner.events.try_send(ev) {
            if let NetEvent::Hello(c) | NetEvent::Blobs(c) = e.into_inner() {
                c.close(CLOSE_REJECTED.into(), b"busy");
            }
        }
    }

    async fn run_server(
        &self,
        conn: Connection,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<(), NetError> {
        let remote = conn.remote_id();
        let hs_err = |e: FrameError| NetError::Handshake(e.to_string());
        let accepted = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let (send, mut recv) = conn
                .accept_bi()
                .await
                .map_err(|e| FrameError::Io(e.to_string()))?;
            let hs: Handshake = read_frame_max(&mut recv, MAX_HANDSHAKE_LEN).await?;
            Ok::<_, FrameError>((send, recv, hs))
        })
        .await
        .map_err(|_| NetError::Handshake("Zeitueberschreitung".into()))
        .and_then(|r| r.map_err(hs_err));
        let (mut send, recv, hs) = match accepted {
            Ok(x) => x,
            Err(e) => {
                conn.close(CLOSE_REJECTED.into(), b"handshake");
                return Err(e);
            }
        };
        // Ab hier ist der Gegenueber geprueft (oder wird abgewiesen): Platz freigeben.
        drop(permit);
        match self.validate(&hs, remote) {
            Err(reason) => {
                let _ = write_frame(&mut send, &HandshakeReply::Reject { reason }).await;
                let _ = send.finish();
                let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
                conn.close(CLOSE_REJECTED.into(), b"rejected");
                Err(NetError::Rejected(reason))
            }
            Ok(version) => {
                let reply = HandshakeReply::Accept {
                    version,
                    handshake: Box::new(self.own_handshake()),
                };
                if let Err(e) = write_frame(&mut send, &reply).await {
                    conn.close(CLOSE_REJECTED.into(), b"handshake");
                    return Err(hs_err(e));
                }
                self.run_session(conn, send, recv, hs, version, remote)
                    .await
            }
        }
    }

    /// Registriert die Verbindung und betreibt sie bis zum Ende.
    async fn run_session(
        &self,
        conn: Connection,
        mut send: SendStream,
        mut recv: RecvStream,
        hs: Handshake,
        version: u16,
        initiator: EndpointId,
    ) -> Result<(), NetError> {
        let remote = conn.remote_id();
        let mut name = hs.display_name;
        while name.len() > MAX_NAME_BYTES {
            name.pop();
        }
        let info = PeerInfo {
            user: hs.cert.user,
            device: *remote.as_bytes(),
            display_name: name,
            avatar_hash: hs.avatar_hash,
            version,
        };
        let (control_tx, mut control_rx) = mpsc::channel::<ControlMessage>(256);
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        if !self.register(PeerEntry {
            id,
            info: info.clone(),
            conn: conn.clone(),
            initiator,
            control_tx,
        }) {
            conn.close(CLOSE_DUPLICATE.into(), b"duplicate");
            return Err(NetError::Duplicate);
        }
        let _ = self.inner.events.send(NetEvent::PeerUp(info.clone())).await;

        let mut tasks = JoinSet::new();
        let events = self.inner.events.clone();
        // Control lesen
        {
            let events = events.clone();
            let info = info.clone();
            tasks.spawn(async move {
                while let Ok(msg) = read_frame::<ControlMessage>(&mut recv).await {
                    if events
                        .send(NetEvent::Control {
                            from: info.clone(),
                            msg,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        // Control schreiben
        tasks.spawn(async move {
            while let Some(msg) = control_rx.recv().await {
                if write_frame(&mut send, &msg).await.is_err() {
                    break;
                }
            }
        });
        // Sync-Streams annehmen
        {
            let (conn, events) = (conn.clone(), events.clone());
            let from = info.device;
            tasks.spawn(async move {
                while let Ok((send, mut recv)) = conn.accept_bi().await {
                    let mut tag = [0u8; 1];
                    if recv.read_exact(&mut tag).await.is_err() || tag[0] != CHANNEL_SYNC {
                        continue;
                    }
                    if events
                        .send(NetEvent::SyncStream { from, send, recv })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        // Video-Streams annehmen
        {
            let (conn, events) = (conn.clone(), events.clone());
            let from = info.device;
            tasks.spawn(async move {
                while let Ok(recv) = conn.accept_uni().await {
                    if events
                        .send(NetEvent::VideoStream { from, recv })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        // Datagramme (Voice): bei vollem Puffer verwerfen
        {
            let (conn, events) = (conn.clone(), events.clone());
            let from = info.device;
            tasks.spawn(async move {
                while let Ok(data) = conn.read_datagram().await {
                    let _ = events.try_send(NetEvent::Datagram { from, data });
                }
            });
        }

        conn.closed().await;
        tasks.abort_all();
        self.unregister(remote, id);
        Ok(())
    }

    /// Traegt die Verbindung ein. Bei Doppelverbindungen gewinnt deterministisch
    /// die, die vom Knoten mit der kleineren ID aufgebaut wurde; beide Seiten
    /// entscheiden gleich. `false` = diese Verbindung verliert.
    fn register(&self, entry: PeerEntry) -> bool {
        let remote = entry.conn.remote_id();
        let local = self.inner.ep.id();
        let mut peers = self
            .inner
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(old) = peers.get(&remote) {
            let winner = if local.as_bytes() < remote.as_bytes() {
                local
            } else {
                remote
            };
            let keep_new = entry.initiator == old.initiator || entry.initiator == winner;
            if !keep_new {
                return false;
            }
            old.conn.close(CLOSE_DUPLICATE.into(), b"duplicate");
        }
        peers.insert(remote, entry);
        drop(peers);
        self.inner.changed.notify_waiters();
        true
    }

    fn unregister(&self, remote: EndpointId, id: u64) {
        let removed = {
            let mut peers = self
                .inner
                .peers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match peers.get(&remote) {
                Some(p) if p.id == id => {
                    peers.remove(&remote);
                    true
                }
                _ => false,
            }
        };
        if removed {
            let events = self.inner.events.clone();
            let device = *remote.as_bytes();
            tokio::spawn(async move {
                let _ = events.send(NetEvent::PeerDown { device }).await;
            });
            self.inner.changed.notify_waiters();
        }
    }
}

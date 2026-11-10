use super::limiter::{Limits, RateLimiter};
use super::service::MAX_PENDING_IN;
use super::*;
use crate::identity::Identity;
use crate::storage::Db;
use app_net::{Net, NetConfig, NetEvent, NetMode};
use app_proto::control::{DeviceCert, Handshake};
use app_proto::hello::{HelloMessage, HelloReply};
use app_proto::PublicKey;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const T: Duration = Duration::from_secs(15);

struct Peer {
    id: Identity,
    db: Db,
    net: Net,
    net_rx: Option<mpsc::Receiver<NetEvent>>,
    svc: FriendsService,
    events: mpsc::UnboundedReceiver<FriendEvent>,
    port: u16,
    _tmp: tempfile::TempDir,
}

fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn fast_cfg() -> ServiceConfig {
    ServiceConfig {
        deliver_timeout: Duration::from_secs(2),
        outbox_poll: Duration::from_millis(200),
        ..ServiceConfig::default()
    }
}

impl Peer {
    async fn start(name: &str) -> Self {
        Self::start_with(Identity::generate(name).unwrap(), free_port(), fast_cfg()).await
    }

    async fn start_with(id: Identity, port: u16, cfg: ServiceConfig) -> Self {
        Self::start_in(id, port, cfg, tempfile::tempdir().unwrap()).await
    }

    async fn start_in(id: Identity, port: u16, cfg: ServiceConfig, tmp: tempfile::TempDir) -> Self {
        Self::start_full(id, port, cfg, tmp, false).await
    }

    async fn start_full(
        id: Identity,
        port: u16,
        cfg: ServiceConfig,
        tmp: tempfile::TempDir,
        rendezvous: bool,
    ) -> Self {
        let db = Db::open_file(&tmp.path().join("t.db")).unwrap();
        let dir = Arc::new(FriendsDirectory::default());
        let hs = Handshake {
            versions: vec![1],
            cert: id.cert().clone(),
            display_name: id.display_name.clone(),
            avatar_hash: None,
            recovery: None,
        };
        let mut nc = NetConfig::new(*id.device_secret_bytes(), hs, dir.clone());
        nc.mode = NetMode::Loopback;
        nc.bind_addr = Some(format!("127.0.0.1:{port}").parse().unwrap());
        nc.initial_backoff = Duration::from_millis(100);
        nc.rendezvous = rendezvous;
        let (net, net_rx) = Net::start(nc).await.unwrap();
        let (svc, events) = FriendsService::start(
            db.clone(),
            net.clone(),
            dir,
            id.cert().clone(),
            id.display_name.clone(),
            id.invite_key(),
            cfg,
        )
        .await
        .unwrap();
        Peer {
            id,
            db,
            net,
            net_rx: Some(net_rx),
            svc,
            events,
            port,
            _tmp: tmp,
        }
    }

    /// Leitet Hello-Verbindungen an den Dienst (macht in der App der Ereignis-Verteiler).
    /// Gibt die uebrigen Netzwerk-Ereignisse ueber den Rueckgabekanal weiter.
    fn pump(&mut self) -> mpsc::UnboundedReceiver<NetEvent> {
        let mut rx = self.net_rx.take().unwrap();
        let svc = self.svc.clone();
        let (tx, out) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    NetEvent::Hello(conn) => svc.spawn_hello_handler(conn),
                    other => {
                        let _ = tx.send(other);
                    }
                }
            }
        });
        out
    }

    fn user(&self) -> PublicKey {
        self.id.user_pubkey()
    }

    fn code(&self) -> String {
        self.id.friend_code(None, 0).encode().unwrap()
    }

    /// Einladungsgeheimnis des aktuellen Codes (Generation 0).
    fn secret(&self) -> [u8; app_proto::friend_code::SECRET_LEN] {
        super::invite_secret(&self.id.invite_key(), 0)
    }

    fn hint(&self, other: &Peer) {
        self.svc.add_addr_hint(
            other.id.device_pubkey(),
            format!("127.0.0.1:{}", other.port).parse().unwrap(),
        );
    }

    async fn status_of(&self, user: PublicKey) -> Option<FriendStatus> {
        self.svc
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|f| f.user == user)
            .map(|f| f.status)
    }

    async fn wait_status(&self, user: PublicKey, want: Option<FriendStatus>) {
        let end = Instant::now() + T;
        while Instant::now() < end {
            if self.status_of(user).await == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "Status {:?} nicht erreicht, ist {:?}",
            want,
            self.status_of(user).await
        );
    }

    async fn outbox_len(&self) -> i64 {
        self.db.call(|c| store::outbox_len(c)).unwrap()
    }
}

async fn next_event(p: &mut Peer) -> FriendEvent {
    tokio::time::timeout(T, p.events.recv())
        .await
        .expect("Zeitlimit")
        .expect("Kanal")
}

#[tokio::test(flavor = "multi_thread")]
async fn befriend_via_code_even_if_recipient_was_offline() {
    let mut a = Peer::start("Anna").await;
    let b_id = Identity::generate("Ben").unwrap();
    let b_port = free_port();
    let b_node = b_id.device_pubkey();
    let b_user = b_id.user_pubkey();
    let b_code = b_id.friend_code(None, 0).encode().unwrap();

    // Ben ist offline: Anfrage wird gespeichert, Zustellung scheitert.
    a.svc
        .add_addr_hint(b_node, format!("127.0.0.1:{b_port}").parse().unwrap());
    let _a_net_events = a.pump();
    a.svc.add_friend(&b_code, "Hallo, ich bin's").await.unwrap();
    assert_eq!(a.status_of(b_user).await, Some(FriendStatus::PendingOut));
    tokio::time::sleep(Duration::from_millis(2500)).await; // erster Versuch laeuft in das Zeitlimit
    assert_eq!(
        a.outbox_len().await,
        1,
        "Anfrage muss in der Outbox bleiben"
    );

    // Ben kommt online.
    let mut b = Peer::start_with(b_id, b_port, fast_cfg()).await;
    let mut b_net = b.pump();
    a.svc.retry_now().await;

    match next_event(&mut b).await {
        FriendEvent::RequestReceived { user, name, note } => {
            assert_eq!(
                (user, name.as_str(), note.as_str()),
                (a.user(), "Anna", "Hallo, ich bin's")
            );
        }
        e => panic!("unerwartet: {e:?}"),
    }
    assert_eq!(b.status_of(a.user()).await, Some(FriendStatus::PendingIn));
    // Vor der Annahme darf Anna nicht auf app/peer/1 durch.
    b.svc.accept(a.user()).await.unwrap();
    b.hint(&a);
    assert_eq!(
        next_event(&mut a).await,
        FriendEvent::Accepted { user: b_user }
    );
    a.wait_status(b_user, Some(FriendStatus::Accepted)).await;
    b.wait_status(a.user(), Some(FriendStatus::Accepted)).await;
    a.wait_outbox_empty().await;
    b.wait_outbox_empty().await;

    // Die Allowlist greift jetzt: Peer-Verbindung kommt zustande.
    let a_addr = a.net.loopback_addr();
    b.net.add_target(a_addr);
    let up = tokio::time::timeout(T, async {
        while let Some(ev) = b_net.recv().await {
            if matches!(ev, NetEvent::PeerUp(_)) {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(up, Ok(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_go_through_the_rendezvous_endpoint_only() {
    // M-5: Der Code nennt nur den Rendezvous-Endpoint; die Geraete-ID (und damit der Weg zur
    // IP) wird erst mit der Annahme ausgetauscht. Der Haupt-Endpoint nimmt kein Hello an.
    let start = |name: &str| {
        Peer::start_full(
            Identity::generate(name).unwrap(),
            free_port(),
            fast_cfg(),
            tempfile::tempdir().unwrap(),
            true,
        )
    };
    let (mut a, mut b) = (start("Anna").await, start("Ben").await);
    let _na = a.pump();
    let _nb = b.pump();
    for (p, q) in [(&a, &b), (&b, &a)] {
        let addr = q.net.rendezvous_loopback_addr();
        for s in addr.ip_addrs() {
            p.svc.add_addr_hint(*addr.id.as_bytes(), *s);
        }
    }
    let code = b.svc.my_code(None).await.unwrap();
    let parsed = app_proto::friend_code::FriendCode::decode(&code).unwrap();
    assert_eq!(parsed.node_id, b.net.rendezvous().0);
    assert_ne!(parsed.node_id, b.id.device_pubkey());

    a.svc.add_friend(&code, "hi").await.unwrap();
    assert!(matches!(
        next_event(&mut b).await,
        FriendEvent::RequestReceived { .. }
    ));
    b.svc.accept(a.user()).await.unwrap();
    assert_eq!(
        next_event(&mut a).await,
        FriendEvent::Accepted { user: b.user() }
    );
    let fa = a.svc.list().await.unwrap();
    assert_eq!(fa[0].node_ids(), vec![b.id.device_pubkey()]);
    assert_eq!(fa[0].rendezvous(), Some(b.net.rendezvous().0));

    // Hello direkt an den Haupt-Endpoint: abgewiesen (ALPN).
    assert!(a.net.connect_hello(b.net.loopback_addr()).await.is_err());
}

impl Peer {
    async fn wait_outbox_empty(&self) {
        let end = Instant::now() + T;
        while Instant::now() < end {
            if self.outbox_len().await == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("Outbox nicht leer");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn blocked_user_cannot_send_new_request() {
    let mut a = Peer::start("Anna").await;
    let mut b = Peer::start("Ben").await;
    let _na = a.pump();
    let _nb = b.pump();
    a.hint(&b);
    b.hint(&a);

    b.svc.block(a.user()).await.unwrap();
    a.svc.add_friend(&b.code(), "bitte").await.unwrap();
    a.wait_outbox_empty().await; // Zustellung wurde quittiert (ohne zu verraten, dass blockiert)
    assert_eq!(b.status_of(a.user()).await, Some(FriendStatus::Blocked));
    assert!(
        b.events.try_recv().is_err(),
        "keine Benachrichtigung bei Blockierten"
    );

    // Nach dem Aufheben kommt eine neue Anfrage durch.
    b.svc.remove(a.user()).await.unwrap();
    a.svc.add_friend(&b.code(), "nochmal").await.unwrap();
    assert!(matches!(
        next_event(&mut b).await,
        FriendEvent::RequestReceived { .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn decline_is_delivered_and_mutual_requests_become_friends() {
    let mut a = Peer::start("Anna").await;
    let mut b = Peer::start("Ben").await;
    let _na = a.pump();
    let _nb = b.pump();
    a.hint(&b);
    b.hint(&a);

    a.svc.add_friend(&b.code(), "").await.unwrap();
    assert!(matches!(
        next_event(&mut b).await,
        FriendEvent::RequestReceived { .. }
    ));
    b.svc.decline(a.user()).await.unwrap();
    assert_eq!(
        next_event(&mut a).await,
        FriendEvent::Declined { user: b.user() }
    );
    a.wait_status(b.user(), None).await;
    b.wait_status(a.user(), None).await;

    // Gleichzeitige Anfragen in beide Richtungen: am Ende sind beide befreundet.
    let (ca, cb) = (a.code(), b.code());
    let (ra, rb) = tokio::join!(a.svc.add_friend(&cb, ""), b.svc.add_friend(&ca, ""));
    ra.unwrap();
    rb.unwrap();
    a.wait_status(b.user(), Some(FriendStatus::Accepted)).await;
    b.wait_status(a.user(), Some(FriendStatus::Accepted)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_codes_and_self_add_are_rejected() {
    let a = Peer::start("Anna").await;
    assert!(matches!(
        a.svc.add_friend("quatsch", "").await,
        Err(FriendsError::Code(_))
    ));
    assert!(matches!(
        a.svc.add_friend(&a.code(), "").await,
        Err(FriendsError::SelfAdd)
    ));
    let other = Identity::generate("X")
        .unwrap()
        .friend_code(None, 0)
        .encode()
        .unwrap();
    a.svc.add_friend(&other, "").await.unwrap();
    assert!(
        a.svc.add_friend(&other, "").await.is_ok(),
        "erneutes Eingeben ist harmlos"
    );
}

/// Bindung an die Geraete-ID selbst (ohne Rendezvous-Endpoint).
/// Macht `friend` bei `p` zum Freund (Anfrage mit Code, dann annehmen).
async fn make_friend(p: &mut Peer, friend: &Identity) {
    let req = HelloMessage::Request {
        cert: friend.cert().clone(),
        display_name: friend.display_name.clone(),
        note: String::new(),
        secret: p.secret(),
        binding: bind(friend),
        rotation: None,
    };
    let r = p
        .svc
        .process(friend.device_pubkey(), req, Instant::now())
        .await;
    assert_eq!(r, HelloReply::Received);
    assert!(matches!(
        next_event(p).await,
        FriendEvent::RequestReceived { .. }
    ));
    p.svc.accept(friend.user_pubkey()).await.unwrap();
}

/// Rotation von `old` auf `new`, unterschrieben mit dem Wiederherstellungsschluessel `rk`.
fn rotation(
    rk: &ed25519_dalek::SigningKey,
    old: &Identity,
    new: &Identity,
    next: app_proto::Hash,
) -> app_proto::recovery::Rotation {
    use ed25519_dalek::Signer;
    let (o, n) = (old.user_pubkey(), new.user_pubkey());
    let sig = rk.sign(&app_proto::recovery::Rotation::signing_bytes(&o, &n, &next));
    app_proto::recovery::Rotation {
        old: o,
        new: n,
        recovery: rk.verifying_key().to_bytes(),
        next,
        signature: app_proto::Signature(sig.to_bytes()),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rotation_moves_the_friendship_to_the_new_key() {
    use app_proto::recovery::commitment;
    let loose = ServiceConfig {
        limits: Limits {
            per_node_burst: 1e9,
            global_burst: 1e9,
            global_per_sec: 1e9,
            ..Limits::default()
        },
        ..fast_cfg()
    };
    let mut a = Peer::start_with(Identity::generate("Anna").unwrap(), free_port(), loose).await;
    let old = Identity::generate("Bob").unwrap();
    make_friend(&mut a, &old).await;
    a.svc
        .set_nickname(old.user_pubkey(), "Bobby")
        .await
        .unwrap();
    let rk = ed25519_dalek::SigningKey::from_bytes(&[42; 32]);
    let rk_pub = rk.verifying_key().to_bytes();
    a.svc
        .note_recovery(old.user_pubkey(), commitment(&rk_pub))
        .await
        .unwrap();
    // Nur der erste Hash zaehlt: ein Dieb kann ihn nicht nachtraeglich ersetzen.
    a.svc
        .note_recovery(old.user_pubkey(), [9; 32])
        .await
        .unwrap();

    let new = Identity::generate("Bob").unwrap();
    let send = |rot: app_proto::recovery::Rotation| HelloMessage::Request {
        cert: new.cert().clone(),
        display_name: "Bob".into(),
        note: String::new(),
        secret: [0; app_proto::friend_code::SECRET_LEN],
        binding: bind(&new),
        rotation: Some(Box::new(rot)),
    };
    let now = Instant::now();
    // Falscher Wiederherstellungsschluessel: keine Rotation, ohne Code also ungueltig.
    let wrong = ed25519_dalek::SigningKey::from_bytes(&[43; 32]);
    let r = a
        .svc
        .process(
            new.device_pubkey(),
            send(rotation(&wrong, &old, &new, [1; 32])),
            now,
        )
        .await;
    assert_eq!(r, HelloReply::Invalid);
    // Rotation auf einen anderen Schluessel als den des Zertifikats.
    let other = Identity::generate("Eve").unwrap();
    let r = a
        .svc
        .process(
            new.device_pubkey(),
            send(rotation(&rk, &old, &other, [1; 32])),
            now,
        )
        .await;
    assert_eq!(r, HelloReply::Invalid);
    // Manipulierte Unterschrift.
    let mut bad = rotation(&rk, &old, &new, [1; 32]);
    bad.next = [2; 32];
    let r = a.svc.process(new.device_pubkey(), send(bad), now).await;
    assert_eq!(r, HelloReply::Invalid);

    let r = a
        .svc
        .process(
            new.device_pubkey(),
            send(rotation(&rk, &old, &new, [1; 32])),
            now,
        )
        .await;
    assert_eq!(r, HelloReply::Received);
    assert_eq!(
        next_event(&mut a).await,
        FriendEvent::Rotated {
            old: old.user_pubkey(),
            new: new.user_pubkey()
        }
    );
    let (o, n) = (old.user_pubkey(), new.user_pubkey());
    let (fo, fnew, rec) =
        a.db.call(move |c| {
            Ok((
                store::get(c, &o)?.unwrap(),
                store::get(c, &n)?.unwrap(),
                store::recovery_of(c, &n)?,
            ))
        })
        .unwrap();
    assert_eq!(fo.status, FriendStatus::Blocked);
    assert_eq!(fnew.status, FriendStatus::Accepted);
    assert_eq!(fnew.name, "Bobby");
    assert!(!fnew.verified);
    assert_eq!(fnew.node_ids(), vec![new.device_pubkey()]);
    assert_eq!(rec, Some([1; 32]));
    assert_eq!(a.outbox_len().await, 1, "Zusage an den neuen Schluessel");
}

#[tokio::test(flavor = "multi_thread")]
async fn own_rotation_needs_the_right_key_and_notifies_friends() {
    use crate::identity::{self, Dpapi, IdentityError, RecoveryKey};
    let mut a = Peer::start("Anna").await;
    let friend = Identity::generate("Bob").unwrap();
    make_friend(&mut a, &friend).await;
    let rk = RecoveryKey::generate().unwrap();
    let c = rk.commitment;
    a.db.call(move |conn| Ok(identity::set_recovery_commitment(conn, &c)))
        .unwrap()
        .unwrap();
    // Nur einmal einrichtbar.
    let again =
        a.db.call(move |conn| Ok(identity::set_recovery_commitment(conn, &[1; 32])))
            .unwrap();
    assert!(matches!(again, Err(IdentityError::RecoveryExists)));

    let me = Identity::generate("Anna").unwrap();
    let old_user = me.user_pubkey();
    let fu = friend.user_pubkey();
    let text = rk.text.clone();
    let (bad, garbage, next, loaded, stored, rot, due) =
        a.db.call(move |conn| {
            me.save(conn, &Dpapi).unwrap();
            let other = RecoveryKey::generate().unwrap();
            let bad = identity::rotate(conn, &Dpapi, &me, &other.text).err();
            let garbage = identity::rotate(conn, &Dpapi, &me, "CFR-AAAA").err();
            let next = identity::rotate(conn, &Dpapi, &me, &text);
            let loaded = Identity::load(conn, &Dpapi).map(|i| i.map(|i| i.user_pubkey()));
            Ok((
                bad,
                garbage,
                next,
                loaded,
                identity::recovery_commitment(conn),
                identity::own_rotation(conn),
                store::outbox_due(conn, i64::MAX)?,
            ))
        })
        .unwrap();
    assert!(matches!(bad, Some(IdentityError::WrongRecoveryKey)));
    assert!(matches!(garbage, Some(IdentityError::WrongRecoveryKey)));
    let next = next.unwrap();
    let new_user = loaded.unwrap().unwrap();
    assert_ne!(new_user, old_user);
    assert_eq!(stored.unwrap(), Some(next.commitment));
    let rot = rot.unwrap().unwrap();
    assert_eq!(
        (rot.old, rot.new, rot.next),
        (old_user, new_user, next.commitment)
    );
    assert!(app_net::verify_rotation(&rot));
    assert_eq!(
        rot.recovery,
        app_proto::recovery::decode_secret(&rk.text)
            .map(|s| ed25519_dalek::SigningKey::from_bytes(&s)
                .verifying_key()
                .to_bytes())
            .unwrap()
    );
    assert_eq!(due.len(), 1);
    assert_eq!((due[0].user, due[0].kind), (fu, store::OutboxKind::Rotate));
    // Der laufende Dienst (alter Schluessel) meldet die Rotation nicht.
    a.svc.deliver_due().await;
    assert_eq!(a.outbox_len().await, 1);
}

fn bind(id: &Identity) -> app_proto::Signature {
    app_net::sign_binding(&id.device_secret_bytes(), &id.device_pubkey())
}

fn fresh_request(
    name: &str,
    secret: [u8; app_proto::friend_code::SECRET_LEN],
) -> (PublicKey, HelloMessage) {
    let id = Identity::generate(name).unwrap();
    (
        id.device_pubkey(),
        HelloMessage::Request {
            cert: id.cert().clone(),
            display_name: name.into(),
            note: "x".repeat(200),
            secret,
            binding: bind(&id),
            rotation: None,
        },
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn spam_does_not_crash_or_fill_the_db() {
    let a = Peer::start("Anna").await;
    // 1000 Anfragen in einer Sekunde von lauter verschiedenen Fremden.
    let base = Instant::now();
    let mut counts = std::collections::HashMap::new();
    for i in 0..1000u64 {
        let (node, msg) = fresh_request(&format!("spam{i}"), a.secret());
        let r = a
            .svc
            .process(node, msg, base + Duration::from_millis(i))
            .await;
        *counts.entry(format!("{r:?}")).or_insert(0) += 1;
    }
    let stored =
        a.db.call(|c| store::count(c, FriendStatus::PendingIn))
            .unwrap();
    assert!(stored <= MAX_PENDING_IN, "{stored} gespeichert");
    assert!(counts["RateLimited"] >= 900, "{counts:?}");

    // Ohne Rate-Limit greift trotzdem die Obergrenze fuer ausstehende Anfragen.
    let loose = ServiceConfig {
        limits: Limits {
            per_node_burst: 1e9,
            global_burst: 1e9,
            global_per_sec: 1e9,
            ..Limits::default()
        },
        ..fast_cfg()
    };
    let b = Peer::start_with(Identity::generate("Ben").unwrap(), free_port(), loose).await;
    for i in 0..400u64 {
        let (node, msg) = fresh_request(&format!("flood{i}"), b.secret());
        b.svc.process(node, msg, base).await;
    }
    let stored =
        b.db.call(|c| store::count(c, FriendStatus::PendingIn))
            .unwrap();
    assert_eq!(stored, MAX_PENDING_IN);
    let total: i64 =
        b.db.call(|c| c.query_row("SELECT count(*) FROM friends", [], |r| r.get(0)))
            .unwrap();
    assert_eq!(total, MAX_PENDING_IN);
    // Volle Liste: die aeltesten fallen heraus, neue kommen weiter an (N-1).
    let names: Vec<String> =
        b.db.call(|c| store::list(c))
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
    assert!(names.contains(&"flood399".to_owned()));
    assert!(!names.contains(&"flood0".to_owned()));

    // Ohne das Geheimnis aus dem Code zaehlt keine Anfrage, und ein neuer Code macht den alten
    // wertlos.
    let (node, msg) = fresh_request("ohne", [0; app_proto::friend_code::SECRET_LEN]);
    assert_eq!(b.svc.process(node, msg, base).await, HelloReply::Invalid);
    let old = b.code();
    let new = b.svc.rotate_code(None).await.unwrap();
    assert_ne!(old, new);
    assert_eq!(b.svc.my_code(None).await.unwrap(), new);
    let (node, msg) = fresh_request("alt", b.secret());
    assert_eq!(b.svc.process(node, msg, base).await, HelloReply::Invalid);
    let code = app_proto::friend_code::FriendCode::decode(&new).unwrap();
    let (node, msg) = fresh_request("neu", code.secret);
    assert_eq!(b.svc.process(node, msg, base).await, HelloReply::Received);
}

#[tokio::test(flavor = "multi_thread")]
async fn forged_hello_messages_are_invalid() {
    let loose = ServiceConfig {
        limits: Limits {
            per_node_burst: 1e9,
            global_burst: 1e9,
            global_per_sec: 1e9,
            ..Limits::default()
        },
        ..fast_cfg()
    };
    let a = Peer::start_with(Identity::generate("Anna").unwrap(), free_port(), loose).await;
    let now = Instant::now();
    let id = Identity::generate("Mallory").unwrap();
    let secret = a.secret();
    let binding = bind(&id);
    let req = |cert: DeviceCert| HelloMessage::Request {
        cert,
        display_name: "M".into(),
        note: String::new(),
        secret,
        binding,
        rotation: None,
    };

    // Zertifikat gehoert nicht zur Verbindung (fremde Node-ID).
    let r = a.svc.process([9; 32], req(id.cert().clone()), now).await;
    assert_eq!(r, HelloReply::Invalid);
    // Manipuliertes Zertifikat.
    let mut bad = id.cert().clone();
    bad.created_at += 1;
    assert_eq!(
        a.svc.process(id.device_pubkey(), req(bad), now).await,
        HelloReply::Invalid
    );
    // Eigener User.
    let me = Identity::generate("Ich").unwrap();
    let r = a
        .svc
        .process(me.device_pubkey(), req(a.id.cert().clone()), now)
        .await;
    assert_eq!(r, HelloReply::Invalid);
    // Zu lange Namen.
    let long = HelloMessage::Request {
        cert: id.cert().clone(),
        display_name: "x".repeat(500),
        note: String::new(),
        secret,
        binding,
        rotation: None,
    };
    assert_eq!(
        a.svc.process(id.device_pubkey(), long, now).await,
        HelloReply::Invalid
    );
    // Accept/Decline ohne passende ausgehende Anfrage.
    let acc = HelloMessage::Accept {
        cert: id.cert().clone(),
        display_name: "M".into(),
        binding,
    };
    assert_eq!(
        a.svc.process(id.device_pubkey(), acc, now).await,
        HelloReply::Invalid
    );
    assert_eq!(
        a.svc
            .process(id.device_pubkey(), HelloMessage::Decline, now)
            .await,
        HelloReply::Invalid
    );
    assert_eq!(a.db.call(|c| store::list(c)).unwrap().len(), 0);

    // Blockierte erfahren nicht, dass sie blockiert sind (Antwort wie bei einer neuen Anfrage).
    a.svc.block(id.user_pubkey()).await.unwrap();
    assert_eq!(
        a.svc
            .process(id.device_pubkey(), req(id.cert().clone()), now)
            .await,
        HelloReply::Received
    );
    assert_eq!(
        a.db.call(|c| store::list(c)).unwrap()[0].status,
        FriendStatus::Blocked
    );
}

#[test]
fn rate_limiter_bursts_refills_and_bounds_memory() {
    let t0 = Instant::now();
    let l = Limits {
        max_tracked: 8,
        ..Limits::default()
    };
    let mut rl = RateLimiter::new(l, t0);
    let n = [1u8; 32];
    assert!((0..3).all(|_| rl.allow(&n, t0)));
    assert!(!rl.allow(&n, t0));
    assert!(
        rl.allow(&n, t0 + Duration::from_secs(11)),
        "Token nach 10 s"
    );

    // Speicher: viele verschiedene Knoten fuellen die Tabelle nicht ueber max_tracked.
    let mut rl = RateLimiter::new(
        Limits {
            global_burst: 1e9,
            global_per_sec: 1e9,
            ..l
        },
        t0,
    );
    let allowed = (0..100u8).filter(|i| rl.allow(&[*i; 32], t0)).count();
    assert_eq!(allowed, 8);
}

/// Macht A und B zu Freunden und baut die Peer-Verbindung auf.
async fn connected_friends() -> (
    Peer,
    Peer,
    mpsc::UnboundedReceiver<NetEvent>,
    mpsc::UnboundedReceiver<NetEvent>,
) {
    let mut a = Peer::start("Anna").await;
    let mut b = Peer::start("Ben").await;
    let mut na = a.pump();
    let nb = b.pump();
    a.hint(&b);
    b.hint(&a);
    a.svc.add_friend(&b.code(), "hi").await.unwrap();
    assert!(matches!(
        next_event(&mut b).await,
        FriendEvent::RequestReceived { .. }
    ));
    b.svc.accept(a.user()).await.unwrap();
    a.wait_status(b.user(), Some(FriendStatus::Accepted)).await;
    // Nur B waehlt: keine Doppelverbindung, deren Aufloesung sonst einen neuen Handshake
    // ausloesen (und den Test ohne den eigentlichen Fix bestehen lassen) koennte.
    b.net.add_target(a.net.loopback_addr());
    let up = tokio::time::timeout(T, async {
        while let Some(ev) = na.recv().await {
            if matches!(ev, NetEvent::PeerUp(_)) {
                return;
            }
        }
    })
    .await;
    assert!(up.is_ok(), "Peer-Verbindung kam nicht zustande");
    while b.net.peers().is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Stabil: dieselbe Verbindung steht nach kurzer Zeit noch.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!((a.net.peers().len(), b.net.peers().len()), (1, 1));
    (a, b, na, nb)
}

async fn wait_disconnected(p: &Peer) {
    let end = Instant::now() + Duration::from_secs(2);
    while !p.net.peers().is_empty() {
        assert!(Instant::now() < end, "Verbindung besteht noch");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn blocking_drops_the_live_connection_for_good() {
    let (a, b, _na, _nb) = connected_friends().await;
    a.svc.block(b.user()).await.unwrap();
    wait_disconnected(&a).await;
    wait_disconnected(&b).await;
    // B waehlt weiter neu (Backoff ab 100 ms), A muss jeden Versuch abweisen.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(a.net.peers().is_empty() && b.net.peers().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_a_friend_drops_the_live_connection() {
    let (a, b, _na, _nb) = connected_friends().await;
    a.svc.remove(b.user()).await.unwrap();
    wait_disconnected(&a).await;
    wait_disconnected(&b).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(a.net.peers().is_empty() && b.net.peers().is_empty());
}

use super::sync::{LogEvent, SyncConfig, SyncService};
use super::*;
use crate::storage::{open_connection, Db};
use app_net::NetEvent;
use app_proto::event::{ChannelId, ChannelKind};
use app_testkit::{random_key, TestNode};
use ed25519_dalek::{Signer, SigningKey};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------- Hilfen ----------

struct User {
    key: SigningKey,
    pk: PublicKey,
}

impl User {
    fn new() -> Self {
        let key = random_key();
        Self {
            pk: key.verifying_key().to_bytes(),
            key,
        }
    }
    fn signer(&self) -> impl Fn(&[u8]) -> Signature + '_ {
        |m| Signature(self.key.sign(m).to_bytes())
    }
}

struct Node {
    conn: Connection,
    clock: HlcClock,
    _tmp: tempfile::TempDir,
}

impl Node {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let conn = open_connection(&tmp.path().join("n.db")).unwrap();
        let clock = HlcClock::load(&conn).unwrap();
        Self {
            conn,
            clock,
            _tmp: tmp,
        }
    }

    fn make(
        &mut self,
        u: &User,
        space: Option<SpaceId>,
        kind: EventKind,
        now: u64,
    ) -> Result<EventEnvelope> {
        create_event(
            &mut self.conn,
            &self.clock,
            u.pk,
            &u.signer(),
            space,
            kind,
            now,
        )
    }

    fn take(&mut self, envs: Vec<EventEnvelope>, now: u64) -> BatchOutcome {
        let out = ingest_batch(&mut self.conn, envs, now).unwrap();
        if let Some(h) = out.accepted.iter().map(|e| e.body.hlc).max() {
            self.clock.observe(h);
        }
        out
    }

    fn dump(&self) -> Vec<String> {
        projection_dump(&self.conn).unwrap()
    }
}

/// Einseitiger Abgleich ohne Netz: `to` holt von `from`, was ihm fehlt (wie der Sync-Dienst).
fn pull(from: &Node, to: &mut Node, to_user: &PublicKey, now: u64) -> usize {
    let heads = heads_for(&from.conn, to_user).unwrap();
    let wants = wants_from(&to.conn, &heads).unwrap();
    let mut got = 0;
    for w in wants {
        let mut want = w;
        loop {
            let (evs, next) = read_want(&from.conn, to_user, &want).unwrap();
            got += to.take(evs, now).accepted.len();
            match next {
                Some(n) => want.from_seq = n,
                None => break,
            }
        }
    }
    got
}

const NOW: u64 = 1_800_000_000_000;

fn text_channel(n: &mut Node, admin: &User, space: SpaceId) -> ChannelId {
    let ch = [7u8; 16];
    n.make(
        admin,
        Some(space),
        EventKind::ChannelCreate {
            channel: ch,
            kind: ChannelKind::Text,
            name: "allgemein".into(),
            position: 0,
        },
        NOW + 1,
    )
    .unwrap();
    ch
}

fn msg(channel: ChannelId, body: &str) -> EventKind {
    EventKind::Message {
        channel,
        body: body.into(),
        reply_to: None,
    }
}

// ---------- Grundfunktionen ----------

#[test]
fn space_lifecycle_and_projection() {
    let (a, b) = (User::new(), User::new());
    let mut n = Node::new();
    let space = n
        .make(
            &a,
            None,
            EventKind::SpaceCreate {
                name: "Runde".into(),
            },
            NOW,
        )
        .unwrap()
        .body
        .space_id;
    n.make(
        &a,
        Some(space),
        EventKind::MemberAdd { user: b.pk },
        NOW + 1,
    )
    .unwrap();
    let ch = text_channel(&mut n, &a, space);
    let m1 = n
        .make(&b, Some(space), msg(ch, "hallo"), NOW + 2)
        .unwrap()
        .id();
    n.make(
        &b,
        Some(space),
        EventKind::MessageEdit {
            target: m1,
            body: "hallo welt".into(),
        },
        NOW + 3,
    )
    .unwrap();
    n.make(
        &a,
        Some(space),
        EventKind::Reaction {
            target: m1,
            emoji: "👍".into(),
        },
        NOW + 4,
    )
    .unwrap();
    // Fremde Nachricht bearbeiten: nicht erlaubt.
    let r = n.make(
        &a,
        Some(space),
        EventKind::MessageEdit {
            target: m1,
            body: "gefaelscht".into(),
        },
        NOW + 5,
    );
    assert!(matches!(r, Err(Error::Reject(Reject::Unauthorized(_)))));
    // Admin darf loeschen.
    n.make(
        &a,
        Some(space),
        EventKind::MessageDelete { target: m1 },
        NOW + 6,
    )
    .unwrap();
    let body: String = n
        .conn
        .query_row("SELECT body FROM messages WHERE event_id = ?1", [m1], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(body, "");
    assert_eq!(list_spaces(&n.conn, &a.pk).unwrap()[0].name, "Runde");
    let v = space_view(&n.conn, &space).unwrap();
    assert_eq!(v.members.len(), 2);
    assert!(v.members[0].admin && v.members[0].user == a.pk);
    assert_eq!(v.channels.len(), 1);
    // Nicht-Admin darf keine Channels anlegen, Nicht-Mitglied nichts.
    let bad = n.make(
        &b,
        Some(space),
        EventKind::ChannelCreate {
            channel: [9; 16],
            kind: ChannelKind::Voice,
            name: "x".into(),
            position: 1,
        },
        NOW + 7,
    );
    assert!(bad.is_err());
    let stranger = User::new();
    assert!(matches!(
        n.make(&stranger, Some(space), msg(ch, "hi"), NOW + 8),
        Err(Error::Reject(Reject::NotMember))
    ));
    // Abgelehnte lokale Events hinterlassen nichts.
    assert_eq!(event_count(&n.conn).unwrap(), 7);
}

#[test]
fn forged_and_invalid_events_are_rejected() {
    let (a, b) = (User::new(), User::new());
    let mut n = Node::new();
    let space = n
        .make(&a, None, EventKind::SpaceCreate { name: "S".into() }, NOW)
        .unwrap()
        .body
        .space_id;
    n.make(
        &a,
        Some(space),
        EventKind::MemberAdd { user: b.pk },
        NOW + 1,
    )
    .unwrap();
    let ch = text_channel(&mut n, &a, space);
    let good = n.make(&b, Some(space), msg(ch, "echt"), NOW + 2).unwrap();

    let mut other = Node::new();

    // Manipulierter Inhalt.
    let mut tampered = good.clone();
    tampered.body.kind = msg(ch, "manipuliert");
    assert_eq!(verify(&tampered, NOW + 10), Err(Reject::BadSignature));
    // Falsche Signatur (anderer Schluessel).
    let mut wrong = good.clone();
    wrong.signature = Signature(User::new().key.sign(&good.body.signing_bytes()).to_bytes());
    assert_eq!(verify(&wrong, NOW + 10), Err(Reject::BadSignature));
    // Zukunft.
    let mut future = n.make(&b, Some(space), msg(ch, "z"), NOW + 3).unwrap();
    future.body.hlc.wall_ms = NOW + 3 * MAX_FUTURE_MS;
    assert!(verify(&future, NOW + 10).is_err());

    let out = other.take(vec![tampered, wrong, future.clone()], NOW + 10);
    let all = all_events(&n.conn, &space).unwrap();
    assert!(out.accepted.is_empty());
    assert_eq!(event_count(&other.conn).unwrap(), 0);

    // Nicht-Mitglied: gueltig signiert, aber ohne Einladung.
    let mut env_n = Node::new();
    let sp2 = env_n
        .make(&a, None, EventKind::SpaceCreate { name: "S2".into() }, NOW)
        .unwrap();
    let stranger = User::new();
    let body = EventBody {
        space_id: sp2.body.space_id,
        author: stranger.pk,
        seq: 1,
        hlc: Hlc {
            wall_ms: NOW + 5,
            counter: 0,
        },
        kind: msg(ch, "rein"),
    };
    let forged = EventEnvelope {
        signature: stranger.signer()(&body.signing_bytes()),
        body,
    };
    let out = env_n.take(vec![forged], NOW + 10);
    assert!(out.accepted.is_empty());
    assert!(out.rejected.contains(&Reject::NotMember));

    // Space-ID, die nicht zum Gruender passt (Hijack-Versuch).
    let body = EventBody {
        space_id: space,
        author: stranger.pk,
        seq: 1,
        hlc: Hlc {
            wall_ms: NOW - 1,
            counter: 0,
        },
        kind: EventKind::SpaceCreate {
            name: "Mein".into(),
        },
    };
    let hijack = EventEnvelope {
        signature: stranger.signer()(&body.signing_bytes()),
        body,
    };
    let mut fresh = Node::new();
    let out = fresh.take(vec![hijack], NOW + 10);
    assert!(out.accepted.is_empty());

    // Luecke: Event mit seq 3 ohne 2 bleibt draussen, mit 1..2 zusammen kommt alles an.
    let mut third = Node::new();
    let out = third.take(all[2..].to_vec(), NOW + 10);
    assert!(out.accepted.is_empty() && out.gaps);
    let out = third.take(all.clone(), NOW + 10);
    assert_eq!(out.accepted.len(), all.len());
    assert_eq!(third.dump(), n.dump());

    // Zweites Event mit derselben (Autor, seq) aber anderem Inhalt.
    let body = EventBody {
        seq: good.body.seq,
        kind: msg(ch, "doppelt"),
        ..good.body.clone()
    };
    let twin = EventEnvelope {
        signature: b.signer()(&body.signing_bytes()),
        body,
    };
    let out = third.take(vec![twin], NOW + 10);
    assert!(out.accepted.is_empty());
    assert!(out.rejected.contains(&Reject::Equivocation));
    // Wiederholte Lieferung ist harmlos.
    let out = third.take(all, NOW + 10);
    assert!(out.duplicates > 0);
    assert!(out.accepted.is_empty());
}

#[test]
fn projection_rebuild_equals_incremental() {
    let (a, b) = (User::new(), User::new());
    let mut n = Node::new();
    let space = n
        .make(&a, None, EventKind::SpaceCreate { name: "R".into() }, NOW)
        .unwrap()
        .body
        .space_id;
    n.make(
        &a,
        Some(space),
        EventKind::MemberAdd { user: b.pk },
        NOW + 1,
    )
    .unwrap();
    let ch = text_channel(&mut n, &a, space);
    let mut ids = Vec::new();
    for i in 0..30u64 {
        let u = if i % 2 == 0 { &a } else { &b };
        ids.push(
            n.make(u, Some(space), msg(ch, &format!("m{i}")), NOW + 10 + i)
                .unwrap()
                .id(),
        );
    }
    n.make(
        &b,
        Some(space),
        EventKind::MessageEdit {
            target: ids[1],
            body: "neu".into(),
        },
        NOW + 50,
    )
    .unwrap();
    n.make(
        &a,
        Some(space),
        EventKind::Reaction {
            target: ids[3],
            emoji: "x".into(),
        },
        NOW + 51,
    )
    .unwrap();
    n.make(
        &a,
        Some(space),
        EventKind::MessageDelete { target: ids[4] },
        NOW + 52,
    )
    .unwrap();
    n.make(
        &a,
        Some(space),
        EventKind::FileShare {
            channel: ch,
            blob_hash: [1; 32],
            name: "a.txt".into(),
            size: 5,
            mime: "text/plain".into(),
            thumb_hash: None,
        },
        NOW + 53,
    )
    .unwrap();
    let before = n.dump();
    assert!(before.len() > 30);
    // Tabellen leeren und neu aufbauen ergibt denselben Zustand.
    project::reset(&n.conn, &space).unwrap();
    assert_ne!(n.dump(), before);
    rebuild_all(&n.conn, 1).unwrap();
    assert_eq!(n.dump(), before);
}

#[test]
fn clock_is_monotonic_and_causal() {
    let c = HlcClock::default();
    let a = c.send(100);
    let b = c.send(100);
    let d = c.send(90);
    assert!(a < b && b < d);
    c.observe(Hlc {
        wall_ms: 500,
        counter: 3,
    });
    assert!(
        c.send(200)
            > Hlc {
                wall_ms: 500,
                counter: 3
            }
    );
}

// ---------- Eigenschaftstest: Konvergenz ----------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[allow(clippy::needless_range_loop)]
fn converge_run(seed: u64) {
    const N: usize = 4;
    let users: Vec<User> = (0..N).map(|_| User::new()).collect();
    let mut nodes: Vec<Node> = (0..N).map(|_| Node::new()).collect();
    let mut rng = Rng(seed);
    let mut now = NOW;
    let admin = &users[0];

    // Gruendung und Einladungen auf Knoten 0, dann zufaellig verteilt.
    let space = nodes[0]
        .make(
            admin,
            None,
            EventKind::SpaceCreate { name: "P".into() },
            now,
        )
        .unwrap()
        .body
        .space_id;
    for u in &users[1..] {
        now += 1;
        nodes[0]
            .make(admin, Some(space), EventKind::MemberAdd { user: u.pk }, now)
            .unwrap();
    }
    let chans: Vec<ChannelId> = (0..3u8).map(|i| [i + 1; 16]).collect();
    for (i, ch) in chans.iter().enumerate() {
        now += 1;
        let kind = if i == 2 {
            ChannelKind::Voice
        } else {
            ChannelKind::Text
        };
        nodes[0]
            .make(
                admin,
                Some(space),
                EventKind::ChannelCreate {
                    channel: *ch,
                    kind,
                    name: format!("c{i}"),
                    position: i as u32,
                },
                now,
            )
            .unwrap();
    }

    for _ in 0..400 {
        now += 1 + rng.below(3) as u64;
        let i = rng.below(N);
        match rng.below(10) {
            // Zufaelliger, einseitiger Abgleich (Trennungen = lange keine Abgleiche).
            0..=2 => {
                let j = rng.below(N);
                if i != j {
                    let (from, to) = pick2(&mut nodes, i, j);
                    pull(from, to, &users[j].pk, now);
                }
            }
            3 | 4 => {
                let ch = chans[rng.below(2)];
                let _ = nodes[i].make(&users[i], Some(space), msg(ch, &format!("n{now}")), now);
            }
            5 => {
                if let Some(t) = random_message(&nodes[i], &mut rng) {
                    let _ = nodes[i].make(
                        &users[i],
                        Some(space),
                        EventKind::MessageEdit {
                            target: t,
                            body: format!("e{now}"),
                        },
                        now,
                    );
                }
            }
            6 => {
                if let Some(t) = random_message(&nodes[i], &mut rng) {
                    let _ = nodes[i].make(
                        &users[i],
                        Some(space),
                        EventKind::MessageDelete { target: t },
                        now,
                    );
                }
            }
            7 => {
                if let Some(t) = random_message(&nodes[i], &mut rng) {
                    let kind = if rng.below(3) == 0 {
                        EventKind::ReactionRemove {
                            target: t,
                            emoji: "a".into(),
                        }
                    } else {
                        EventKind::Reaction {
                            target: t,
                            emoji: "a".into(),
                        }
                    };
                    let _ = nodes[i].make(&users[i], Some(space), kind, now);
                }
            }
            8 => {
                // Admin-Aktionen von beliebigen Knoten (Nicht-Admins werden ignoriert).
                let ch = chans[rng.below(3)];
                let _ = nodes[i].make(
                    &users[i],
                    Some(space),
                    EventKind::ChannelUpdate {
                        channel: ch,
                        name: Some(format!("u{now}")),
                        position: None,
                    },
                    now,
                );
            }
            _ => {
                let _ = nodes[i].make(
                    &users[i],
                    Some(space),
                    EventKind::ChannelDelete {
                        channel: chans[rng.below(3)],
                    },
                    now,
                );
            }
        }
    }

    // Netz heilt: alle gleichen sich mehrfach paarweise ab.
    for _ in 0..3 {
        for i in 0..N {
            for j in 0..N {
                if i != j {
                    let (from, to) = pick2(&mut nodes, i, j);
                    pull(from, to, &users[j].pk, now + 1000);
                }
            }
        }
    }
    let d0 = nodes[0].dump();
    let c0 = event_count(&nodes[0].conn).unwrap();
    assert!(c0 > 20, "Seed {seed}: zu wenige Events ({c0})");
    for (k, n) in nodes.iter().enumerate().skip(1) {
        assert_eq!(
            event_count(&n.conn).unwrap(),
            c0,
            "Seed {seed}: Eventzahl Knoten {k}"
        );
        assert_eq!(n.dump(), d0, "Seed {seed}: Projektion Knoten {k}");
    }
    // Und die Projektion entspricht dem Neuaufbau aus dem Log.
    rebuild_all(&nodes[0].conn, 1).unwrap();
    assert_eq!(nodes[0].dump(), d0, "Seed {seed}: Neuaufbau");
}

fn pick2(nodes: &mut [Node], from: usize, to: usize) -> (&Node, &mut Node) {
    if from < to {
        let (l, r) = nodes.split_at_mut(to);
        (&l[from], &mut r[0])
    } else {
        let (l, r) = nodes.split_at_mut(from);
        (&r[0], &mut l[to])
    }
}

fn random_message(n: &Node, rng: &mut Rng) -> Option<[u8; 32]> {
    let ids: Vec<[u8; 32]> = n
        .conn
        .prepare("SELECT event_id FROM messages ORDER BY event_id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    (!ids.is_empty()).then(|| ids[rng.below(ids.len())])
}

#[test]
fn random_events_and_partitions_converge() {
    for seed in 1..=12u64 {
        converge_run(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    }
}

// ---------- Netzwerk ----------

struct NetNode {
    t: TestNode,
    db: Db,
    svc: SyncService,
    log: tokio::sync::mpsc::UnboundedReceiver<LogEvent>,
    _tmp: tempfile::TempDir,
}

async fn net_node(name: &str, cfg: SyncConfig) -> NetNode {
    let mut t = TestNode::spawn(name).await;
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open_file(&tmp.path().join("n.db")).unwrap();
    let key = t.user_key.clone();
    let signer: sync::Signer = Arc::new(move |m| Signature(key.sign(m).to_bytes()));
    let (svc, log) = SyncService::start(db.clone(), t.net.clone(), t.user, signer, cfg)
        .await
        .unwrap();
    let mut rx = std::mem::replace(&mut t.rx, tokio::sync::mpsc::channel(1).1);
    let s = svc.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                NetEvent::PeerUp(p) => s.on_peer_up(p),
                NetEvent::SyncStream { from, send, recv } => s.on_sync_stream(from, send, recv),
                _ => {}
            }
        }
    });
    NetNode {
        t,
        db,
        svc,
        log,
        _tmp: tmp,
    }
}

async fn eventually(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < end, "Zeitueberschreitung: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn count(db: &Db) -> i64 {
    db.call(|c| event_count(c)).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn peers_sync_on_connect_and_receive_live_events() {
    let cfg = SyncConfig::default();
    let a = net_node("A", cfg).await;
    let b = net_node("B", cfg).await;
    a.t.allow(&b.t);
    b.t.allow(&a.t);

    // A legt vor der Verbindung Space, Channel und Einladung an.
    let env = a
        .svc
        .publish(
            None,
            EventKind::SpaceCreate {
                name: "Netz".into(),
            },
        )
        .await
        .unwrap();
    let space = env.body.space_id;
    a.svc
        .publish(Some(space), EventKind::MemberAdd { user: b.t.user })
        .await
        .unwrap();
    let ch = [5u8; 16];
    a.svc
        .publish(
            Some(space),
            EventKind::ChannelCreate {
                channel: ch,
                kind: ChannelKind::Text,
                name: "t".into(),
                position: 0,
            },
        )
        .await
        .unwrap();

    a.t.net.add_target(b.t.net.loopback_addr());
    b.t.net.add_target(a.t.net.loopback_addr());
    eventually("B erhaelt den Space", Duration::from_secs(15), || {
        count(&b.db) == 3
    })
    .await;

    // Live: B schreibt, A sieht es sofort (Push, kein Warten auf den naechsten Abgleich).
    b.svc
        .publish(Some(space), msg(ch, "hallo A"))
        .await
        .unwrap();
    eventually("A erhaelt B-Nachricht", Duration::from_secs(10), || {
        count(&a.db) == 4
    })
    .await;
    let (da, db_) = (
        a.db.call(|c| projection_dump(c)).unwrap(),
        b.db.call(|c| projection_dump(c)).unwrap(),
    );
    assert_eq!(da, db_);
    let mut log = a.log;
    assert!(matches!(log.try_recv(), Ok(LogEvent::SpacesChanged(_))));

    // Zustellstatus: A erfaehrt, dass B die neue Nachricht von A hat.
    let m = a.svc.publish(Some(space), msg(ch, "von A")).await.unwrap();
    let (author, seq, bob) = (m.body.author, m.body.seq as i64, b.t.user);
    eventually(
        "A weiss, dass B die Nachricht hat",
        Duration::from_secs(10),
        || {
            a.db.call(move |c| {
                Ok(c.query_row(
                    "SELECT seq FROM peer_acks WHERE peer_user = ?1 AND author = ?2",
                    rusqlite::params![bob, author],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0))
            })
            .unwrap()
                >= seq
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_peer_catches_up_and_forged_events_are_dropped() {
    let cfg = SyncConfig {
        resync_every: Duration::from_millis(500),
        ..SyncConfig::default()
    };
    let a = net_node("A", cfg).await;
    let b = net_node("B", cfg).await;
    a.t.allow(&b.t);
    b.t.allow(&a.t);
    let space = a
        .svc
        .publish(None, EventKind::SpaceCreate { name: "S".into() })
        .await
        .unwrap()
        .body
        .space_id;
    a.svc
        .publish(Some(space), EventKind::MemberAdd { user: b.t.user })
        .await
        .unwrap();
    let ch = [5u8; 16];
    a.svc
        .publish(
            Some(space),
            EventKind::ChannelCreate {
                channel: ch,
                kind: ChannelKind::Text,
                name: "t".into(),
                position: 0,
            },
        )
        .await
        .unwrap();
    for i in 0..300 {
        a.svc
            .publish(Some(space), msg(ch, &format!("alt {i}")))
            .await
            .unwrap();
    }
    // B war offline und holt alles beim ersten Kontakt nach.
    a.t.net.add_target(b.t.net.loopback_addr());
    eventually("B holt 303 Events", Duration::from_secs(20), || {
        count(&b.db) == 303
    })
    .await;
    assert_eq!(
        a.db.call(|c| projection_dump(c)).unwrap(),
        b.db.call(|c| projection_dump(c)).unwrap()
    );
}

/// Messung (Release): `cargo test -p app-core --release sync_100k -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "Messung, nur im Release-Build sinnvoll"]
async fn sync_100k_events_between_two_nodes() {
    // Echte Zeit, weil Empfaenger Events aus der fernen Zukunft ablehnen.
    let now0 = sync::now_ms() - 1_000_000_000;
    let total: usize = std::env::var("TOTAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let (author, member) = (User::new(), User::new());
    let tmp = tempfile::tempdir().unwrap();
    let mut a_conn = open_connection(&tmp.path().join("a.db")).unwrap();
    let clock = HlcClock::load(&a_conn).unwrap();
    let signer = author.signer();
    let space = create_event(
        &mut a_conn,
        &clock,
        author.pk,
        &signer,
        None,
        EventKind::SpaceCreate {
            name: "Gross".into(),
        },
        now0,
    )
    .unwrap()
    .body
    .space_id;
    create_event(
        &mut a_conn,
        &clock,
        author.pk,
        &signer,
        Some(space),
        EventKind::MemberAdd { user: member.pk },
        now0 + 1,
    )
    .unwrap();
    let ch = [3u8; 16];
    create_event(
        &mut a_conn,
        &clock,
        author.pk,
        &signer,
        Some(space),
        EventKind::ChannelCreate {
            channel: ch,
            kind: ChannelKind::Text,
            name: "t".into(),
            position: 0,
        },
        now0 + 2,
    )
    .unwrap();
    // Viele Events direkt per Ingest erzeugen (schneller als einzeln mit Transaktion).
    let mut envs = Vec::with_capacity(total);
    for i in 0..total as u64 - 3 {
        let body = EventBody {
            space_id: space,
            author: author.pk,
            seq: 4 + i,
            hlc: Hlc {
                wall_ms: now0 + 10 + i,
                counter: 0,
            },
            kind: msg(ch, &format!("Nachricht {i}")),
        };
        envs.push(EventEnvelope {
            signature: signer(&body.signing_bytes()),
            body,
        });
    }
    for chunk in envs.chunks(5000) {
        ingest_batch(&mut a_conn, chunk.to_vec(), now0 + 1_000_000).unwrap();
    }
    println!("Vorbereitung: {} Events", event_count(&a_conn).unwrap());
    drop(a_conn);

    let cfg = SyncConfig {
        stream_timeout: Duration::from_secs(120),
        ..SyncConfig::default()
    };
    let mut a = TestNode::spawn_with("A", author.key.clone(), random_key(), None, None).await;
    let mut b = TestNode::spawn_with("B", member.key.clone(), random_key(), None, None).await;
    a.allow(&b);
    b.allow(&a);
    let a_db = Db::open_file(&tmp.path().join("a.db")).unwrap();
    let tmp_b = tempfile::tempdir().unwrap();
    let b_db = Db::open_file(&tmp_b.path().join("b.db")).unwrap();
    let mk = |k: SigningKey| -> sync::Signer { Arc::new(move |m| Signature(k.sign(m).to_bytes())) };
    let (sa, _la) = SyncService::start(
        a_db.clone(),
        a.net.clone(),
        a.user,
        mk(author.key.clone()),
        cfg,
    )
    .await
    .unwrap();
    let (sb, _lb) = SyncService::start(
        b_db.clone(),
        b.net.clone(),
        b.user,
        mk(member.key.clone()),
        cfg,
    )
    .await
    .unwrap();
    for (n, s) in [(&mut a, sa), (&mut b, sb)] {
        let mut rx = std::mem::replace(&mut n.rx, tokio::sync::mpsc::channel(1).1);
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    NetEvent::PeerUp(p) => s.on_peer_up(p),
                    NetEvent::SyncStream { from, send, recv } => s.on_sync_stream(from, send, recv),
                    _ => {}
                }
            }
        });
    }
    let t0 = Instant::now();
    a.net.add_target(b.net.loopback_addr());
    b.net.add_target(a.net.loopback_addr());
    let mut last = Instant::now();
    eventually("100k Events", Duration::from_secs(60), || {
        let n = count(&b_db);
        if last.elapsed() > Duration::from_secs(2) {
            println!("  B hat {n} Events nach {:?}", t0.elapsed());
            last = Instant::now();
        }
        n == total as i64
    })
    .await;
    let dt = t0.elapsed();
    println!("Sync von {total} Events: {dt:?}");
    assert!(dt < Duration::from_secs(10), "zu langsam: {dt:?}");
}

/// M1-09: Admin plus 5 Freunde, einer ist beim Einladen offline; ein unberechtigtes Event
/// (gueltig signiert, aber kein Admin) wird von allen ignoriert.
#[tokio::test(flavor = "multi_thread")]
async fn admin_and_five_friends_see_same_channels() {
    let cfg = SyncConfig {
        resync_every: Duration::from_millis(500),
        ..SyncConfig::default()
    };
    let admin = net_node("Admin", cfg).await;
    let mut members = Vec::new();
    for i in 0..5 {
        let m = net_node(&format!("M{i}"), cfg).await;
        admin.t.allow(&m.t);
        m.t.allow(&admin.t);
        members.push(m);
    }
    let space = admin
        .svc
        .publish(
            None,
            EventKind::SpaceCreate {
                name: "Gruppe".into(),
            },
        )
        .await
        .unwrap()
        .body
        .space_id;
    for m in &members {
        admin
            .svc
            .publish(Some(space), EventKind::MemberAdd { user: m.t.user })
            .await
            .unwrap();
    }
    for (i, name) in ["Lobby", "Zocken", "AFK"].iter().enumerate() {
        admin
            .svc
            .publish(
                Some(space),
                EventKind::ChannelCreate {
                    channel: [i as u8 + 1; 16],
                    kind: ChannelKind::Voice,
                    name: (*name).into(),
                    position: i as u32,
                },
            )
            .await
            .unwrap();
    }
    // Ein Mitglied darf lokal keinen Channel anlegen.
    let denied = members[0]
        .svc
        .publish(
            Some(space),
            EventKind::ChannelCreate {
                channel: [99; 16],
                kind: ChannelKind::Voice,
                name: "x".into(),
                position: 9,
            },
        )
        .await;
    assert!(denied.is_err());

    // Nur 4 sind online; M4 (offline) kommt spaeter.
    for m in &members[..4] {
        admin.t.net.add_target(m.t.net.loopback_addr());
        m.t.net.add_target(admin.t.net.loopback_addr());
    }
    let expected = 1 + 5 + 3;
    for m in &members[..4] {
        eventually("Mitglied sieht den Space", Duration::from_secs(20), || {
            count(&m.db) == expected
        })
        .await;
    }

    // Boeswilliger Client: gueltig signiertes, aber unberechtigtes Event direkt in die eigene DB.
    let evil_key = members[1].t.user_key.clone();
    let evil_user = members[1].t.user;
    let evil = members[1]
        .db
        .call(move |c| {
            let seq = head(c, &space, &evil_user)? + 1;
            let body = EventBody {
                space_id: space,
                author: evil_user,
                seq,
                hlc: Hlc {
                    wall_ms: sync::now_ms(),
                    counter: 0,
                },
                kind: EventKind::ChannelCreate {
                    channel: [66; 16],
                    kind: ChannelKind::Voice,
                    name: "boese".into(),
                    position: 0,
                },
            };
            let env = EventEnvelope {
                signature: Signature(evil_key.sign(&body.signing_bytes()).to_bytes()),
                body,
            };
            ingest_batch(c, vec![env.clone()], sync::now_ms())?;
            Ok(env)
        })
        .unwrap();
    members[1].svc_push_for_test(evil).await;

    // Der Offline-Freund geht online und bekommt den Space.
    admin.t.net.add_target(members[4].t.net.loopback_addr());
    members[4].t.net.add_target(admin.t.net.loopback_addr());
    // Alle haben am Ende dieselbe Projektion; das boese Event ist ueberall gespeichert oder
    // nirgends, aber wirkt nie.
    let want = admin.db.call(|c| projection_dump(c)).unwrap();
    assert!(!want.iter().any(|l| l.contains("boese")));
    for m in &members {
        eventually("Projektion gleich", Duration::from_secs(30), || {
            m.db.call(|c| projection_dump(c)).unwrap() == want
        })
        .await;
        let v = m.db.call(move |c| space_view(c, &space)).unwrap();
        let names: Vec<_> = v.channels.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Lobby", "Zocken", "AFK"]);
        assert_eq!(v.members.len(), 6);
    }
}

impl NetNode {
    /// Test: ein selbst gebautes Event an verbundene Peers schicken (wie ein veraenderter Client).
    async fn svc_push_for_test(&self, env: EventEnvelope) {
        self.svc.push_for_test(vec![env]).await;
    }
}

#[test]
fn concurrent_edit_and_delete_end_in_the_same_state_everywhere() {
    let (admin, bob) = (User::new(), User::new());
    let (mut a, mut b, mut c) = (Node::new(), Node::new(), Node::new());
    let space = a
        .make(
            &admin,
            None,
            EventKind::SpaceCreate { name: "R".into() },
            NOW,
        )
        .unwrap()
        .body
        .space_id;
    a.make(
        &admin,
        Some(space),
        EventKind::MemberAdd { user: bob.pk },
        NOW + 1,
    )
    .unwrap();
    let ch = text_channel(&mut a, &admin, space);
    let m = a
        .make(&bob, Some(space), msg(ch, "alt"), NOW + 2)
        .unwrap()
        .id();
    // b kennt den Stand, dann arbeiten a und b getrennt weiter.
    // Zwei Durchgaenge: die Reihenfolge der Autoren ist beliebig (wie beim Sync-Dienst).
    pull(&a, &mut b, &admin.pk, NOW + 3);
    pull(&a, &mut b, &admin.pk, NOW + 3);
    a.make(
        &bob,
        Some(space),
        EventKind::MessageEdit {
            target: m,
            body: "neu".into(),
        },
        NOW + 10,
    )
    .unwrap();
    b.make(
        &admin,
        Some(space),
        EventKind::MessageDelete { target: m },
        NOW + 11,
    )
    .unwrap();
    // c bekommt zuerst das Loeschen, dann die Bearbeitung; a und b gleichen sich gegenseitig ab.
    pull(&a, &mut c, &admin.pk, NOW + 20);
    pull(&a, &mut c, &admin.pk, NOW + 20);
    pull(&b, &mut c, &admin.pk, NOW + 21);
    pull(&b, &mut c, &admin.pk, NOW + 21);
    pull(&a, &mut c, &admin.pk, NOW + 22);
    pull(&a, &mut c, &admin.pk, NOW + 22);
    pull(&b, &mut a, &admin.pk, NOW + 23);
    pull(&b, &mut a, &admin.pk, NOW + 23);
    pull(&a, &mut b, &admin.pk, NOW + 24);
    pull(&a, &mut b, &admin.pk, NOW + 24);
    pull(&b, &mut c, &admin.pk, NOW + 25);
    pull(&b, &mut c, &admin.pk, NOW + 25);
    pull(&a, &mut c, &admin.pk, NOW + 26);
    pull(&a, &mut c, &admin.pk, NOW + 26);
    assert_eq!(a.dump(), b.dump());
    assert_eq!(a.dump(), c.dump());
    for n in [&a, &b, &c] {
        let (body, del): (String, i64) = n
            .conn
            .query_row(
                "SELECT body, deleted FROM messages WHERE event_id = ?1",
                [m],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((body.as_str(), del), ("", 1));
    }
}

#[test]
fn direct_message_space_syncs_and_both_sides_find_it() {
    use super::dm::{dm_index, DM_CHANNEL, DM_NAME};
    let (a, b) = (User::new(), User::new());
    let (mut na, mut nb) = (Node::new(), Node::new());
    let space = na
        .make(
            &a,
            None,
            EventKind::SpaceCreate {
                name: DM_NAME.into(),
            },
            NOW,
        )
        .unwrap()
        .body
        .space_id;
    na.make(
        &a,
        Some(space),
        EventKind::MemberAdd { user: b.pk },
        NOW + 1,
    )
    .unwrap();
    let ch = [8u8; 16];
    na.make(
        &a,
        Some(space),
        EventKind::ChannelCreate {
            channel: ch,
            kind: ChannelKind::Text,
            name: DM_CHANNEL.into(),
            position: 0,
        },
        NOW + 2,
    )
    .unwrap();
    // Vor dem Abgleich kennt B nichts; danach findet jede Seite das Gegenueber.
    assert!(dm_index(&nb.conn, &b.pk).unwrap().is_empty());
    pull(&na, &mut nb, &b.pk, NOW + 3);
    assert_eq!(dm_index(&na.conn, &a.pk).unwrap()[&b.pk], (space, Some(ch)));
    assert_eq!(dm_index(&nb.conn, &b.pk).unwrap()[&a.pk], (space, Some(ch)));
    // B antwortet; A bekommt es beim naechsten Abgleich.
    nb.make(&b, Some(space), msg(ch, "hi"), NOW + 4).unwrap();
    pull(&nb, &mut na, &a.pk, NOW + 5);
    assert_eq!(na.dump(), nb.dump());
}

// ---------- M2-08: viele Knoten, Ausfaelle, falsch gehende Uhren ----------

/// Wie `pull`, aber die Verbindung kann mitten im Abgleich abreissen: dann kommt nur ein
/// Anfang des Batches an und der Abgleich bricht ab.
fn pull_lossy(from: &Node, to: &mut Node, to_user: &PublicKey, now: u64, rng: &mut Rng) {
    let heads = heads_for(&from.conn, to_user).unwrap();
    let wants = wants_from(&to.conn, &heads).unwrap();
    for w in wants {
        let mut want = w;
        loop {
            let (mut evs, next) = read_want(&from.conn, to_user, &want).unwrap();
            let cut = rng.below(8) == 0;
            if cut {
                let keep = rng.below(evs.len() + 1);
                evs.truncate(keep);
            }
            to.take(evs, now);
            match next {
                Some(n) if !cut => want.from_seq = n,
                _ => break,
            }
        }
        if rng.below(10) == 0 {
            return;
        }
    }
}

const HOUR_MS: i64 = 3_600_000;

fn skewed(now: u64, skew: i64) -> u64 {
    (now as i64 + skew) as u64
}

/// 20 Knoten, Messwerte aus dem Plan: 10.000 Nachrichten, Ausfaelle, abgebrochene Abgleiche,
/// Uhren um bis zu +-1 Stunde verstellt. Danach muss ueberall dasselbe stehen.
#[allow(clippy::needless_range_loop)]
fn big_run(nodes_n: usize, messages: usize, seed: u64) {
    let users: Vec<User> = (0..nodes_n).map(|_| User::new()).collect();
    let mut nodes: Vec<Node> = (0..nodes_n).map(|_| Node::new()).collect();
    let mut rng = Rng(seed);
    let skew: Vec<i64> = (0..nodes_n)
        .map(|i| [0, HOUR_MS, -HOUR_MS][i % 3])
        .collect();
    let mut now = NOW;
    let space = nodes[0]
        .make(
            &users[0],
            None,
            EventKind::SpaceCreate { name: "G".into() },
            now,
        )
        .unwrap()
        .body
        .space_id;
    for u in &users[1..] {
        now += 1;
        nodes[0]
            .make(
                &users[0],
                Some(space),
                EventKind::MemberAdd { user: u.pk },
                now,
            )
            .unwrap();
    }
    let ch = text_channel(&mut nodes[0], &users[0], space);
    for j in 1..nodes_n {
        let (from, to) = pick2(&mut nodes, 0, j);
        pull(from, to, &users[j].pk, now + 10);
    }

    let mut down = vec![false; nodes_n];
    let mut created = 0usize;
    while created < messages {
        now += 1 + rng.below(3) as u64;
        // Knoten fallen aus und kommen wieder.
        let k = rng.below(nodes_n);
        if rng.below(20) == 0 {
            down[k] = !down[k];
        }
        let i = rng.below(nodes_n);
        if rng.below(10) < 6 {
            // Schreiben geht auch offline.
            let t = skewed(now, skew[i]);
            nodes[i]
                .make(&users[i], Some(space), msg(ch, &format!("m{created}")), t)
                .unwrap();
            created += 1;
        } else {
            let j = rng.below(nodes_n);
            if i != j && !down[i] && !down[j] {
                let t = skewed(now, skew[i]);
                let (from, to) = pick2(&mut nodes, j, i);
                pull_lossy(from, to, &users[i].pk, t, &mut rng);
            }
        }
    }

    // Netz heilt: alle zu Knoten 0, dann Knoten 0 zu allen.
    let t = skewed(now, 0) + 1000;
    for _ in 0..2 {
        for j in 1..nodes_n {
            let (from, to) = pick2(&mut nodes, j, 0);
            pull(from, to, &users[0].pk, t);
        }
        for j in 1..nodes_n {
            let (from, to) = pick2(&mut nodes, 0, j);
            pull(from, to, &users[j].pk, t);
        }
    }

    let want_events = 1 + (nodes_n as i64 - 1) + 1 + messages as i64;
    let d0 = nodes[0].dump();
    for (k, n) in nodes.iter().enumerate() {
        assert_eq!(
            event_count(&n.conn).unwrap(),
            want_events,
            "Knoten {k}: Events fehlen oder doppelt"
        );
        let (rows, distinct): (i64, i64) = n
            .conn
            .query_row(
                "SELECT count(*), count(DISTINCT event_id) FROM messages",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (rows, distinct),
            (messages as i64, messages as i64),
            "Knoten {k}"
        );
        assert_eq!(n.dump(), d0, "Knoten {k}: Projektion weicht ab");
    }
    // Gleiche Reihenfolge auf allen Knoten, auch bei verstellten Uhren.
    let me = users[0].pk;
    let order = |n: &Node| -> Vec<[u8; 32]> {
        let mut all = Vec::new();
        let mut before = None;
        loop {
            let p = super::chat::page(&n.conn, &space, &ch, before.as_ref(), 500, &me).unwrap();
            if p.is_empty() {
                break;
            }
            before = Some(p[0].cursor.clone());
            all.splice(0..0, p.into_iter().map(|m| m.id));
        }
        all
    };
    let o0 = order(&nodes[0]);
    assert_eq!(o0.len(), messages);
    for n in &nodes[1..] {
        assert_eq!(order(n), o0);
    }
}

#[test]
fn twenty_nodes_with_outages_and_skewed_clocks_converge() {
    // In Debug-Builds dauert der volle Lauf lange; hier 200 statt 10.000 Nachrichten, voll siehe unten.
    big_run(20, 200, 0x5eed_0001);
}

#[test]
#[ignore = "10.000 Nachrichten, mit --release --ignored ausfuehren"]
fn twenty_nodes_ten_thousand_messages_converge() {
    let t = Instant::now();
    big_run(20, 10_000, 0x5eed_0002);
    println!("20 Knoten, 10.000 Nachrichten: {:?}", t.elapsed());
}

#[test]
fn skewed_clocks_keep_causal_order() {
    // A geht eine Stunde vor, B eine Stunde nach. B antwortet auf A, A antwortet auf B.
    let (a, b) = (User::new(), User::new());
    let (mut na, mut nb) = (Node::new(), Node::new());
    let space = na
        .make(
            &a,
            None,
            EventKind::SpaceCreate { name: "U".into() },
            skewed(NOW, HOUR_MS),
        )
        .unwrap()
        .body
        .space_id;
    na.make(
        &a,
        Some(space),
        EventKind::MemberAdd { user: b.pk },
        skewed(NOW, HOUR_MS) + 1,
    )
    .unwrap();
    let ch = text_channel(&mut na, &a, space);
    let mut order = Vec::new();
    for round in 0..20u64 {
        let ta = skewed(NOW, HOUR_MS) + 10 + round * 10;
        let tb = skewed(NOW, -HOUR_MS) + 10 + round * 10;
        pull(&na, &mut nb, &b.pk, tb);
        let e = nb
            .make(&b, Some(space), msg(ch, &format!("b{round}")), tb)
            .unwrap();
        order.push(e.id());
        pull(&nb, &mut na, &a.pk, ta);
        let e = na
            .make(&a, Some(space), msg(ch, &format!("a{round}")), ta)
            .unwrap();
        order.push(e.id());
    }
    pull(&na, &mut nb, &b.pk, skewed(NOW, 0));
    // Wer geantwortet hat, steht immer nach dem, worauf er geantwortet hat.
    for n in [&na, &nb] {
        let p = super::chat::page(&n.conn, &space, &ch, None, 100, &a.pk).unwrap();
        let seen: Vec<_> = p.iter().map(|m| m.id).collect();
        assert_eq!(seen, order);
    }
}

#[test]
fn foreign_spaces_stay_invisible_until_i_am_added() {
    // A legt einen Space mit B an und teilt eine Datei. C bekommt die Events (z. B. von A
    // zugeschickt), ist aber nicht Mitglied: Space, Datei und Ungelesenes bleiben unsichtbar,
    // und C gibt die Datei auch an B nicht heraus.
    let (a, b, c) = (User::new(), User::new(), User::new());
    let mut na = Node::new();
    let space = na
        .make(
            &a,
            None,
            EventKind::SpaceCreate {
                name: "Fremd".into(),
            },
            NOW,
        )
        .unwrap()
        .body
        .space_id;
    na.make(
        &a,
        Some(space),
        EventKind::MemberAdd { user: b.pk },
        NOW + 1,
    )
    .unwrap();
    let ch = text_channel(&mut na, &a, space);
    let h = [0x42u8; 32];
    na.make(
        &a,
        Some(space),
        EventKind::FileShare {
            channel: ch,
            blob_hash: h,
            name: "geheim.pdf".into(),
            size: 10,
            mime: "application/pdf".into(),
            thumb_hash: None,
        },
        NOW + 2,
    )
    .unwrap();
    na.make(&a, Some(space), msg(ch, "hallo @c"), NOW + 3)
        .unwrap();

    let mut nc = Node::new();
    let out = nc.take(all_events(&na.conn, &space).unwrap(), NOW + 10);
    assert!(!out.accepted.is_empty(), "gespeichert wird weiterhin");
    let hidden = |n: &Node| {
        list_spaces(&n.conn, &c.pk).unwrap().is_empty()
            && super::chat::recent_attachments(&n.conn, &c.pk, 10)
                .unwrap()
                .is_empty()
            && !super::chat::blob_allowed(&n.conn, &c.pk, &b.pk, &h).unwrap()
            && super::unread::space_unread(&n.conn, &c.pk, "c")
                .unwrap()
                .is_empty()
            && crate::notify::incoming(&n.conn, &c.pk, "c", 0)
                .unwrap()
                .0
                .is_empty()
    };
    assert!(hidden(&nc));

    // Erst mit der Einladung durch den Admin wird der Space sichtbar.
    let add = na
        .make(
            &a,
            Some(space),
            EventKind::MemberAdd { user: c.pk },
            NOW + 4,
        )
        .unwrap();
    nc.take(vec![add], NOW + 10);
    assert!(!hidden(&nc));
    assert_eq!(list_spaces(&nc.conn, &c.pk).unwrap()[0].space_id, space);
    assert!(super::chat::blob_allowed(&nc.conn, &c.pk, &b.pk, &h).unwrap());
}

#[test]
fn only_valid_members_can_feed_events_into_the_log() {
    // A (Admin) laedt B ein. B ist normales Mitglied und versucht, einen Wegwerf-Schluessel X
    // anzumelden; spaeter entfernt A den B. Ein dritter Knoten D darf weder Events von X noch
    // Events, die B nach dem Entfernen geschrieben hat, speichern (und damit weiterreichen).
    let (a, b, x) = (User::new(), User::new(), User::new());
    let mut na = Node::new();
    let space = na
        .make(&a, None, EventKind::SpaceCreate { name: "S".into() }, NOW)
        .unwrap()
        .body
        .space_id;
    na.make(
        &a,
        Some(space),
        EventKind::MemberAdd { user: b.pk },
        NOW + 1,
    )
    .unwrap();
    let ch = text_channel(&mut na, &a, space);
    let mut nb = Node::new();
    pull(&na, &mut nb, &b.pk, NOW + 2);

    // B "laedt" X ein: B darf das nicht (kein Admin). Das Event selbst stammt von einem
    // Mitglied und wird gespeichert, wirkt aber nicht; X bleibt draussen.
    let fake_add = forge(&b, space, 1, NOW + 3, EventKind::MemberAdd { user: x.pk });
    assert_eq!(nb.take(vec![fake_add.clone()], NOW + 3).accepted.len(), 1);
    let from_x = forge(&x, space, 1, NOW + 4, msg(ch, "Wegwerf"));
    // B schreibt vor dem Entfernen (kommt spaet an) und danach (ohne es zu wissen).
    let before = nb
        .make(&b, Some(space), msg(ch, "vorher"), NOW + 5)
        .unwrap();
    let remove = na
        .make(
            &a,
            Some(space),
            EventKind::MemberRemove { user: b.pk },
            NOW + 10,
        )
        .unwrap();
    let after = nb
        .make(&b, Some(space), msg(ch, "nachher"), NOW + 20)
        .unwrap();

    let mut nd = Node::new();
    nd.take(all_events(&na.conn, &space).unwrap(), NOW + 30);
    let out = nd.take(vec![fake_add.clone(), from_x], NOW + 30);
    assert_eq!(
        out.accepted,
        vec![fake_add],
        "nur das (wirkungslose) Event von B"
    );
    assert!(
        out.rejected.contains(&Reject::NotMember),
        "{:?}",
        out.rejected
    );
    // Die Folge von B ist ab hier luecklos: vorher, nachher.
    let out = nd.take(vec![before.clone(), after], NOW + 30);
    assert_eq!(
        out.accepted,
        vec![before],
        "Nachzuegler vor dem Entfernen zaehlt"
    );
    assert!(out
        .rejected
        .iter()
        .any(|r| matches!(r, Reject::Unauthorized(_))));
    assert!(!out.gaps);
    let authors: Vec<PublicKey> = nd
        .conn
        .prepare("SELECT DISTINCT author FROM events")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(!authors.contains(&x.pk));
    let _ = remove;

    // Wer selbst austritt, kann danach ebenfalls nichts mehr einspeisen.
    let c = User::new();
    na.make(
        &a,
        Some(space),
        EventKind::MemberAdd { user: c.pk },
        NOW + 40,
    )
    .unwrap();
    let mut nc = Node::new();
    pull(&na, &mut nc, &c.pk, NOW + 41);
    let leave = nc
        .make(
            &c,
            Some(space),
            EventKind::MemberRemove { user: c.pk },
            NOW + 42,
        )
        .unwrap();
    let late = forge(&c, space, leave.body.seq + 1, NOW + 43, msg(ch, "zurueck?"));
    nd.take(all_events(&na.conn, &space).unwrap(), NOW + 50);
    let out = nd.take(vec![leave.clone(), late], NOW + 50);
    assert_eq!(out.accepted, vec![leave]);
}

/// Signiert ein Event von Hand (ohne die Pruefungen von `create_event`).
fn forge(u: &User, space: SpaceId, seq: u64, wall_ms: u64, kind: EventKind) -> EventEnvelope {
    let body = EventBody {
        space_id: space,
        author: u.pk,
        seq,
        hlc: Hlc {
            wall_ms,
            counter: 0,
        },
        kind,
    };
    EventEnvelope {
        signature: u.signer()(&body.signing_bytes()),
        body,
    }
}

#[test]
fn a_member_cannot_push_the_clocks_far_into_the_future() {
    // Ein Mitglied mit vorgestellter Uhr zieht die HLC aller Empfaenger nach. Das ist nur bis
    // MAX_FUTURE_MS moeglich; ein Tag (frueher erlaubt) wird abgelehnt.
    let (a, b) = (User::new(), User::new());
    let mut na = Node::new();
    let space = na
        .make(&a, None, EventKind::SpaceCreate { name: "S".into() }, NOW)
        .unwrap()
        .body
        .space_id;
    na.make(
        &a,
        Some(space),
        EventKind::MemberAdd { user: b.pk },
        NOW + 1,
    )
    .unwrap();
    let ch = text_channel(&mut na, &a, space);
    let mut nb = Node::new();
    pull(&na, &mut nb, &b.pk, NOW + 2);
    assert!(MAX_FUTURE_MS <= 3 * HOUR_MS as u64);
    let day = forge(&a, space, 4, NOW + 24 * HOUR_MS as u64, msg(ch, "morgen"));
    let out = nb.take(vec![day], NOW + 3);
    assert_eq!(out.rejected, vec![Reject::FutureHlc]);
    let ok = forge(&a, space, 4, NOW + 2 * HOUR_MS as u64, msg(ch, "gleich"));
    assert_eq!(nb.take(vec![ok], NOW + 3).accepted.len(), 1);
    let mine = nb
        .make(&b, Some(space), msg(ch, "antwort"), NOW + 4)
        .unwrap();
    assert!(mine.body.hlc.wall_ms <= NOW + 3 + MAX_FUTURE_MS);
}

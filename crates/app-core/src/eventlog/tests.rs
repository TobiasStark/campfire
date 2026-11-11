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
            self.clock.observe(h, now);
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
    let out = third.take(vec![twin.clone()], NOW + 10);
    assert!(out.accepted.is_empty());
    assert!(out.rejected.contains(&Reject::Equivocation));
    // N-14: beide Fassungen bleiben als Beweis, der Autor ist markiert.
    let space = good.body.space_id;
    assert_eq!(
        super::equivocators(&third.conn, &space).unwrap(),
        vec![b.pk]
    );
    // Heads mit derselben seq, aber anderer Event-ID: genau dieses Event anfordern ...
    let mut fresh = Node::new();
    fresh.take(all.clone(), NOW + 10);
    let tip_heads = |tip| {
        vec![SpaceHeads {
            space_id: space,
            authors: vec![(b.pk, good.body.seq, tip)],
            invite: None,
        }]
    };
    let w = super::wants_from(&fresh.conn, &tip_heads(twin.id())).unwrap();
    assert_eq!(
        w.iter().map(|w| (w.from_seq, w.to_seq)).collect::<Vec<_>>(),
        vec![(good.body.seq, good.body.seq)]
    );
    assert!(super::wants_from(&fresh.conn, &tip_heads(good.id()))
        .unwrap()
        .is_empty());
    // ... und ist die Abweichung belegt, nicht immer wieder.
    fresh.take(vec![twin], NOW + 10);
    assert!(super::wants_from(&fresh.conn, &tip_heads([0; 32]))
        .unwrap()
        .is_empty());
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
    c.observe(
        Hlc {
            wall_ms: 500,
            counter: 3,
        },
        200,
    );
    assert!(
        c.send(200)
            > Hlc {
                wall_ms: 500,
                counter: 3
            }
    );
}

#[test]
fn clock_survives_a_full_counter_and_caps_foreign_future() {
    let c = HlcClock::default();
    c.observe(
        Hlc {
            wall_ms: 1000,
            counter: u32::MAX,
        },
        1000,
    );
    let next = c.send(1000);
    assert_eq!(
        next,
        Hlc {
            wall_ms: 1001,
            counter: 0
        }
    );
    assert!(c.send(1000) > next);

    // Ein Tag in der Zukunft zieht die eigene Uhr hoechstens um MAX_FUTURE_MS vor.
    let c = HlcClock::default();
    c.observe(
        Hlc {
            wall_ms: 10_000 + 24 * 3_600_000,
            counter: 7,
        },
        10_000,
    );
    assert!(c.send(10_000).wall_ms <= 10_000 + MAX_FUTURE_MS);
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

/// Zustellstand der neuesten eigenen Nachricht im Channel `ch` (acked, total).
fn delivery_of_last(db: &Db, space: SpaceId, ch: [u8; 16], me: PublicKey) -> Option<(u32, u32)> {
    db.call(move |c| crate::eventlog::chat::page(c, &space, &ch, None, 50, &me))
        .unwrap()
        .iter()
        .rev()
        .find(|m| m.author == me)
        .and_then(|m| m.delivery)
        .map(|d| (d.acked, d.total))
}

/// Ob seit dem letzten Aufruf `DeliveryChanged` fuer `space` gemeldet wurde.
fn saw_delivery(log: &mut tokio::sync::mpsc::UnboundedReceiver<LogEvent>, space: SpaceId) -> bool {
    let mut seen = false;
    while let Ok(ev) = log.try_recv() {
        if matches!(&ev, LogEvent::DeliveryChanged(s) if s.contains(&space)) {
            seen = true;
        }
    }
    seen
}

/// Beide Seiten sehen "zugestellt" fuer ihre eigenen Nachrichten, ohne dass danach noch eine
/// Nachricht kommt, und ohne auf den naechsten Abgleich (60 s) zu warten. Die Oberflaeche wird
/// per `DeliveryChanged` benachrichtigt.
#[tokio::test(flavor = "multi_thread")]
async fn sender_learns_delivery_without_further_messages() {
    let cfg = SyncConfig::default();
    let mut a = net_node("A", cfg).await;
    let mut b = net_node("B", cfg).await;
    a.t.allow(&b.t);
    b.t.allow(&a.t);
    let space = a
        .svc
        .publish(None, EventKind::SpaceCreate { name: "DM".into() })
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
    // A schreibt, solange B noch nicht verbunden ist.
    a.svc
        .publish(Some(space), msg(ch, "bist du da?"))
        .await
        .unwrap();
    assert_eq!(delivery_of_last(&a.db, space, ch, a.t.user), Some((0, 1)));

    a.t.net.add_target(b.t.net.loopback_addr());
    b.t.net.add_target(a.t.net.loopback_addr());
    eventually("B holt alles nach", Duration::from_secs(15), || {
        count(&b.db) == 4
    })
    .await;
    let mut a_told = false;
    eventually(
        "A sieht: B hat die Nachricht",
        Duration::from_secs(10),
        || {
            a_told |= saw_delivery(&mut a.log, space);
            delivery_of_last(&a.db, space, ch, a.t.user) == Some((1, 1)) && a_told
        },
    )
    .await;

    // Jetzt antwortet B live: B muss "zugestellt" sehen, obwohl A nichts mehr schreibt.
    saw_delivery(&mut b.log, space);
    b.svc.publish(Some(space), msg(ch, "ja!")).await.unwrap();
    let mut b_told = false;
    eventually(
        "B sieht: A hat die Antwort",
        Duration::from_secs(10),
        || {
            b_told |= saw_delivery(&mut b.log, space);
            delivery_of_last(&b.db, space, ch, b.t.user) == Some((1, 1)) && b_told
        },
    )
    .await;
    assert_eq!(count(&a.db), 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_peer_catches_up_and_forged_events_are_dropped() {
    let cfg = SyncConfig {
        resync_every: Duration::from_millis(500),
        min_resync: Duration::from_millis(100),
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
        min_resync: Duration::from_millis(100),
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
/// Uhrenabweichung in den Tests: jede Uhr bis 25 min vor oder nach (zusammen unter
/// `MAX_FUTURE_MS`).
const SKEW_MS: i64 = 25 * 60_000;

fn skewed(now: u64, skew: i64) -> u64 {
    (now as i64 + skew) as u64
}

/// 20 Knoten, Messwerte aus dem Plan: 10.000 Nachrichten, Ausfaelle, abgebrochene Abgleiche,
/// Uhren um bis zu +-25 Minuten verstellt. Danach muss ueberall dasselbe stehen.
#[allow(clippy::needless_range_loop)]
fn big_run(nodes_n: usize, messages: usize, seed: u64) {
    let users: Vec<User> = (0..nodes_n).map(|_| User::new()).collect();
    let mut nodes: Vec<Node> = (0..nodes_n).map(|_| Node::new()).collect();
    let mut rng = Rng(seed);
    let skew: Vec<i64> = (0..nodes_n)
        .map(|i| [0, SKEW_MS, -SKEW_MS][i % 3])
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
    // A geht 25 Minuten vor, B 25 Minuten nach. B antwortet auf A, A antwortet auf B.
    let (a, b) = (User::new(), User::new());
    let (mut na, mut nb) = (Node::new(), Node::new());
    let space = na
        .make(
            &a,
            None,
            EventKind::SpaceCreate { name: "U".into() },
            skewed(NOW, SKEW_MS),
        )
        .unwrap()
        .body
        .space_id;
    na.make(
        &a,
        Some(space),
        EventKind::MemberAdd { user: b.pk },
        skewed(NOW, SKEW_MS) + 1,
    )
    .unwrap();
    let ch = text_channel(&mut na, &a, space);
    let mut order = Vec::new();
    for round in 0..20u64 {
        let ta = skewed(NOW, SKEW_MS) + 10 + round * 10;
        let tb = skewed(NOW, -SKEW_MS) + 10 + round * 10;
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
    assert_eq!(list_spaces(&nc.conn, &c.pk).unwrap()[0].space_id, space);
    // Mitglied, aber erst nach Zustimmung sichtbar (M-2); A ist kein Freund von C, also
    // erscheint nicht einmal eine Einladung.
    let lists = super::consent::space_lists(&nc.conn, &c.pk).unwrap();
    assert!(lists.shown.is_empty() && lists.invites.is_empty());
    assert!(super::chat::recent_attachments(&nc.conn, &c.pk, 10)
        .unwrap()
        .is_empty());
    nc.conn
        .execute(
            "INSERT INTO friends (user_pubkey, name, status) VALUES (?1, 'a', 'accepted')",
            [a.pk],
        )
        .unwrap();
    let lists = super::consent::space_lists(&nc.conn, &c.pk).unwrap();
    assert_eq!(lists.invites.len(), 1);
    super::consent::accept(&nc.conn, &space).unwrap();
    assert!(!hidden(&nc));
    assert!(!crate::notify::incoming(&nc.conn, &c.pk, "c", 0)
        .unwrap()
        .0
        .is_empty());
    // Ausgeliefert wird erst, wenn die Datei ueber diesen Space zu C kam (H-1).
    assert!(!super::chat::blob_allowed(&nc.conn, &c.pk, &b.pk, &h).unwrap());
    super::chat::add_blob_origin(&nc.conn, &h, &space).unwrap();
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
    // B schreibt vor dem Entfernen (A kennt es, D bekommt es spaet) und danach (ohne es zu
    // wissen).
    let before = nb
        .make(&b, Some(space), msg(ch, "vorher"), NOW + 5)
        .unwrap();
    pull(&nb, &mut na, &a.pk, NOW + 6);
    // F kennt alles bis hier, aber die Entfernung noch nicht.
    let mut nf = Node::new();
    // Zweimal: je nach Schluesselreihenfolge kommen die Events von B vor der Einladung an.
    pull(&na, &mut nf, &a.pk, NOW + 7);
    pull(&na, &mut nf, &a.pk, NOW + 7);
    assert_eq!(head(&nf.conn, &space, &b.pk).unwrap(), 2);
    let remove = na
        .make(
            &a,
            Some(space),
            EventKind::MemberRemove {
                user: b.pk,
                last_seq: 0,
            },
            NOW + 10,
        )
        .unwrap();
    assert!(
        matches!(
            remove.body.kind,
            EventKind::MemberRemove { last_seq: 2, .. }
        ),
        "Obergrenze = hoechste bekannte seq von B"
    );
    let after = nb
        .make(&b, Some(space), msg(ch, "nachher"), NOW + 20)
        .unwrap();
    // M-1: B datiert ein neues Event in die Zeit vor dem Entfernen zurueck.
    let backdated = forge(&b, space, 3, NOW + 8, msg(ch, "rueckdatiert"));
    // Rueckwaerts laufender HLC je Autor wird nie angenommen.
    let reversed = forge(&b, space, 3, NOW + 4, msg(ch, "rueckwaerts"));
    assert!(matches!(
        nf.take(vec![reversed], NOW + 30).rejected[..],
        [Reject::Unauthorized(_)]
    ));
    // F kennt die Entfernung noch nicht und nimmt das rueckdatierte Event an ...
    assert_eq!(nf.take(vec![backdated.clone()], NOW + 30).accepted.len(), 1);
    // ... und wirft es weg, sobald die Entfernung ankommt.
    assert_eq!(nf.take(vec![remove.clone()], NOW + 30).accepted.len(), 1);
    assert_eq!(head(&nf.conn, &space, &b.pk).unwrap(), 2);
    assert!(!all_events(&nf.conn, &space)
        .unwrap()
        .iter()
        .any(|e| e.id() == backdated.id()));
    assert!(!nf.dump().iter().any(|l| l.contains("rueckdatiert")));
    // Wer die Entfernung schon kennt, nimmt es gar nicht erst an.
    assert!(matches!(
        na.take(vec![backdated], NOW + 30).rejected[..],
        [Reject::Unauthorized(_)]
    ));

    // D bekommt das Log von A, aber die Events von B erst spaet.
    let mut nd = Node::new();
    let mut from_a = all_events(&na.conn, &space).unwrap();
    from_a.retain(|e| e.body.author != b.pk);
    nd.take(from_a, NOW + 30);
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
            EventKind::MemberRemove {
                user: c.pk,
                last_seq: 0,
            },
            NOW + 42,
        )
        .unwrap();
    assert!(matches!(
        leave.body.kind,
        EventKind::MemberRemove { last_seq, .. } if last_seq == leave.body.seq
    ));
    // Ein Austritt mit falscher Obergrenze ist ungueltig.
    let bad_leave = forge(
        &c,
        space,
        leave.body.seq,
        NOW + 42,
        EventKind::MemberRemove {
            user: c.pk,
            last_seq: 99,
        },
    );
    let mut ne = Node::new();
    ne.take(all_events(&na.conn, &space).unwrap(), NOW + 50);
    assert!(matches!(
        ne.take(vec![bad_leave], NOW + 50).rejected[..],
        [Reject::Malformed]
    ));
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
    assert!(MAX_FUTURE_MS <= HOUR_MS as u64);
    let day = forge(&a, space, 4, NOW + 24 * HOUR_MS as u64, msg(ch, "morgen"));
    let out = nb.take(vec![day], NOW + 3);
    assert_eq!(out.rejected, vec![Reject::FutureHlc]);
    let ok = forge(&a, space, 4, NOW + HOUR_MS as u64 / 2, msg(ch, "gleich"));
    assert_eq!(nb.take(vec![ok], NOW + 3).accepted.len(), 1);
    let mine = nb
        .make(&b, Some(space), msg(ch, "antwort"), NOW + 4)
        .unwrap();
    assert!(mine.body.hlc.wall_ms <= NOW + 3 + MAX_FUTURE_MS);
}

#[test]
fn gate_admits_only_invited_spaces_pauses_pending_ones_and_purges_after_leaving() {
    // M-3: Ein Space, in den C nie eingeladen wurde, kommt gar nicht erst ins Log. Mit der
    // Einladung (in den Heads) laedt C die Kette des Gruenders bis zur Einladung, danach pausiert
    // ein nicht angenommener Space an der Obergrenze. Nach dem Austritt wird alles geloescht,
    // sobald ein Mitglied den Austritt bestaetigt hat.
    let (a, c) = (User::new(), User::new());
    let mut na = Node::new();
    let space = na
        .make(&a, None, EventKind::SpaceCreate { name: "S".into() }, NOW)
        .unwrap()
        .body
        .space_id;
    let ch = text_channel(&mut na, &a, space);
    for i in 0..5 {
        na.make(&a, Some(space), msg(ch, &format!("m{i}")), NOW + 10 + i)
            .unwrap();
    }
    let opts = IngestOpts {
        me: Some(c.pk),
        pending_cap: 4,
        ..IngestOpts::default()
    };
    let mut nc = Node::new();
    let sync = |na: &Node, nc: &mut Node, now: u64| {
        let heads = heads_for(&na.conn, &c.pk).unwrap();
        let wants = gate::wants_for(&nc.conn, &c.pk, &heads, now, opts.pending_cap).unwrap();
        let mut got = 0;
        for w in wants {
            let (evs, _) = read_want(&na.conn, &c.pk, &w).unwrap();
            got += ingest_batch_with(&mut nc.conn, evs, now, opts)
                .unwrap()
                .accepted
                .len();
        }
        got
    };

    // Nicht eingeladen: aufgedraengte Events werden abgelehnt, angefordert wird nichts.
    let out = ingest_batch_with(
        &mut nc.conn,
        all_events(&na.conn, &space).unwrap(),
        NOW + 20,
        opts,
    )
    .unwrap();
    assert!(out.accepted.is_empty());
    assert!(out.rejected.contains(&Reject::NotMember));
    assert_eq!(sync(&na, &mut nc, NOW + 20), 0);

    // Eingeladen: die ganze Kette des Gruenders bis zur Einladung (ohne Obergrenze).
    na.make(
        &a,
        Some(space),
        EventKind::MemberAdd { user: c.pk },
        NOW + 30,
    )
    .unwrap();
    assert!(heads_for(&na.conn, &c.pk).unwrap()[0].invite.is_some());
    assert_eq!(sync(&na, &mut nc, NOW + 31), 8);
    assert_eq!(list_spaces(&nc.conn, &c.pk).unwrap().len(), 1);

    // Nicht angenommen und ueber der Obergrenze: weitere Events warten.
    na.make(&a, Some(space), msg(ch, "danach"), NOW + 40)
        .unwrap();
    assert_eq!(sync(&na, &mut nc, NOW + 41), 0);
    let pushed = all_events(&na.conn, &space).unwrap();
    let out = ingest_batch_with(&mut nc.conn, pushed, NOW + 41, opts).unwrap();
    assert_eq!(out.rejected, vec![Reject::Quota]);
    consent::accept(&nc.conn, &space).unwrap();
    assert_eq!(sync(&na, &mut nc, NOW + 42), 1);

    // Austritt: geloescht wird erst, wenn A ihn hat.
    let leave = nc
        .make(
            &c,
            Some(space),
            EventKind::MemberRemove {
                user: c.pk,
                last_seq: 0,
            },
            NOW + 50,
        )
        .unwrap();
    assert!(consent::purge_left(&nc.conn, &c.pk).unwrap().is_empty());
    assert_eq!(na.take(vec![leave], NOW + 51).accepted.len(), 1);
    let heads = heads_for(&na.conn, &c.pk).unwrap();
    assert!(
        heads[0].invite.is_none(),
        "keine Einladung mehr nach dem Austritt"
    );
    record_acks(&nc.conn, &a.pk, &heads).unwrap();
    assert_eq!(consent::purge_left(&nc.conn, &c.pk).unwrap(), vec![space]);
    assert!(all_events(&nc.conn, &space).unwrap().is_empty());
    assert!(list_spaces(&nc.conn, &c.pk).unwrap().is_empty());
    // Danach kommt nichts mehr hinein.
    na.make(&a, Some(space), msg(ch, "noch da?"), NOW + 60)
        .unwrap();
    assert_eq!(sync(&na, &mut nc, NOW + 61), 0);
    let out = ingest_batch_with(
        &mut nc.conn,
        all_events(&na.conn, &space).unwrap(),
        NOW + 61,
        opts,
    )
    .unwrap();
    assert!(out.accepted.is_empty());
}

#[test]
fn reversed_batches_ingest_linearly_and_rebuilds_can_be_deferred() {
    // M-4: Ein Stapel in umgekehrter Reihenfolge brauchte frueher einen Durchlauf pro Event
    // (quadratisch). Jetzt wird je Autor nach seq sortiert: alles in einem Durchlauf.
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
    for i in 0..200u64 {
        na.make(&a, Some(space), msg(ch, "x"), NOW + 1000 + i)
            .unwrap();
    }
    let mut evs = all_events(&na.conn, &space).unwrap();
    evs.reverse();
    let mut nb = Node::new();
    let out = nb.take(evs, NOW + 5000);
    assert_eq!(out.accepted.len(), 203);
    assert!(!out.gaps && out.rejected.is_empty());
    assert_eq!(nb.dump(), na.dump());

    // Ein aelteres Event (B schrieb offline frueh) bei vorhandenen neueren: der Neuaufbau wird
    // gemeldet statt sofort ausgefuehrt; der Aufrufer entprellt ihn.
    let early = forge(&b, space, 1, NOW + 500, msg(ch, "frueh"));
    let opts = IngestOpts {
        defer_rebuild: true,
        ..IngestOpts::default()
    };
    let out = ingest_batch_with(&mut nb.conn, vec![early.clone()], NOW + 5000, opts).unwrap();
    assert_eq!(out.accepted.len(), 1);
    assert_eq!(out.dirty, [space].into_iter().collect());
    rebuild_space(&nb.conn, &space, (NOW / 1000) as i64).unwrap();
    na.take(vec![early], NOW + 5000);
    assert_eq!(nb.dump(), na.dump());
    // Das neueste Event braucht keinen Neuaufbau.
    let newest = forge(&b, space, 2, NOW + 9000, msg(ch, "neu"));
    let out = ingest_batch_with(&mut nb.conn, vec![newest], NOW + 9000, opts).unwrap();
    assert!(out.dirty.is_empty());
}

// ---------- Fluten (Datenbank blockieren) ----------

/// Gefaelschte oder ungefragte Events von `signer` fuer einen erfundenen Space.
fn junk_events(signer: &SigningKey, n: usize, forge: bool) -> Vec<EventEnvelope> {
    let author = signer.verifying_key().to_bytes();
    (1..=n as u64)
        .map(|seq| {
            let body = EventBody {
                space_id: [7; 16],
                author,
                seq,
                hlc: Hlc {
                    wall_ms: sync::now_ms(),
                    counter: 0,
                },
                kind: EventKind::SpaceCreate {
                    name: "x".repeat(40),
                },
            };
            let signature = if forge {
                Signature([1; 64])
            } else {
                Signature(signer.sign(&body.signing_bytes()).to_bytes())
            };
            EventEnvelope { signature, body }
        })
        .collect()
}

/// Zwei verbundene Knoten; `a` spielt den Angreifer.
async fn connected_pair() -> (NetNode, NetNode) {
    let cfg = SyncConfig {
        resync_every: Duration::from_secs(3600),
        ..SyncConfig::default()
    };
    let a = net_node("A", cfg).await;
    let b = net_node("B", cfg).await;
    a.t.allow(&b.t);
    b.t.allow(&a.t);
    a.t.net.add_target(b.t.net.loopback_addr());
    b.t.net.add_target(a.t.net.loopback_addr());
    let a_dev = a.t.device;
    eventually("verbunden", Duration::from_secs(15), || {
        b.t.net.peers().iter().any(|p| p.device == a_dev)
    })
    .await;
    (a, b)
}

/// Schickt Event-Pakete ueber einen rohen Sync-Stream, bis der Empfaenger ihn schliesst.
/// Gibt zurueck, wie viele Pakete durchgingen.
async fn flood(a: &NetNode, to: PublicKey, batch: Vec<EventEnvelope>, max: usize) -> usize {
    let Ok((mut send, mut recv)) = a.t.net.open_sync(&to).await else {
        return 0;
    };
    let msg = app_proto::sync::SyncMessage::Events(batch);
    let mut sent = 0;
    for _ in 0..max {
        if app_net::write_frame(&mut send, &msg).await.is_err() {
            break;
        }
        sent += 1;
    }
    let _ = send.finish();
    // Der Empfaenger bricht ab: das Lesen endet, ohne dass er etwas schickt.
    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        app_net::read_frame::<app_proto::sync::SyncMessage>(&mut recv),
    )
    .await;
    sent
}

/// Ein Freund schickt massenhaft Events mit gefaelschten Signaturen. Die eigene Datenbank
/// bleibt bedienbar, der Freund wird gesperrt, nichts wird gespeichert.
#[tokio::test(flavor = "multi_thread")]
async fn forged_event_flood_does_not_block_the_database() {
    let (a, b) = connected_pair().await;
    let forged = junk_events(&a.t.user_key, 4000, true);

    // Waehrend der Flut misst B, wie lange eigene Datenbankzugriffe dauern.
    let probe_db = b.db.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = stop.clone();
    let probe = tokio::task::spawn_blocking(move || {
        let mut worst = Duration::ZERO;
        while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
            let t = Instant::now();
            probe_db
                .call(|c| c.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0)))
                .unwrap();
            worst = worst.max(t.elapsed());
            std::thread::sleep(Duration::from_millis(5));
        }
        worst
    });

    let d = b.t.device;
    let s = tokio::join!(
        flood(&a, d, forged.clone(), 50),
        flood(&a, d, forged.clone(), 50),
        flood(&a, d, forged.clone(), 50),
        flood(&a, d, forged.clone(), 50),
    );
    let sent = [s.0, s.1, s.2, s.3];
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let worst = probe.await.unwrap();

    // The penalty is set by the receiving task; on a slow runner it can land after the
    // senders have given up, so wait for it instead of checking once.
    eventually("Angreifer gesperrt", Duration::from_secs(20), || {
        b.svc.is_penalized(&a.t.device)
    })
    .await;
    assert_eq!(count(&b.db), 0);
    assert!(
        worst < Duration::from_millis(500),
        "DB blockiert: {worst:?}"
    );
    // Nach der ersten Faelschung ist Schluss; QUIC puffert nur wenige Pakete.
    assert!(sent.iter().all(|&n| n < 50), "nicht abgebrochen: {sent:?}");
    // Gesperrt: weitere Streams werden sofort verworfen.
    assert!(flood(&a, b.t.device, forged, 50).await < 50);
}

/// Gueltig signierte, aber nicht angeforderte Events ueber dem Push-Kontingent: Abbruch und
/// Sperre, bevor die Datenbank sie anfasst.
#[tokio::test(flavor = "multi_thread")]
async fn unrequested_events_beyond_the_push_allowance_get_the_peer_blocked() {
    let (a, b) = connected_pair().await;
    flood(&a, b.t.device, junk_events(&a.t.user_key, 300, false), 1).await;
    eventually("gesperrt", Duration::from_secs(5), || {
        b.svc.is_penalized(&a.t.device)
    })
    .await;
    assert_eq!(count(&b.db), 0);
}

// ---------- Speichergrenze ----------

fn pruned_by(conn: &Connection) -> HashMap<PublicKey, Vec<i64>> {
    let mut st = conn
        .prepare("SELECT author, seq FROM events WHERE pruned = 1 ORDER BY seq")
        .unwrap();
    let mut out: HashMap<PublicKey, Vec<i64>> = HashMap::new();
    for row in st
        .query_map([], |r| Ok((r.get::<_, PublicKey>(0)?, r.get::<_, i64>(1)?)))
        .unwrap()
    {
        let (a, s) = row.unwrap();
        out.entry(a).or_default().push(s);
    }
    out
}

fn messages_of(conn: &Connection, author: &PublicKey) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM messages WHERE author = ?1",
        [author],
        |r| r.get(0),
    )
    .unwrap()
}

/// Ein Spammer fuellt den Speicher: aufgeraeumt wird zuerst bei ihm (aelteste zuerst), samt
/// Reaktionen anderer auf seine Nachrichten. Die anderen behalten ihre Historie. Ein neues
/// Mitglied bekommt die Platzhalter, die Folge des Spammers bleibt fuer es lueckenlos.
#[test]
fn storage_limit_prunes_the_biggest_author_first_and_keeps_chains_intact() {
    let (admin, bob, spam, carol) = (User::new(), User::new(), User::new(), User::new());
    let mut n = Node::new();
    let space = n
        .make(
            &admin,
            None,
            EventKind::SpaceCreate { name: "S".into() },
            NOW,
        )
        .unwrap()
        .body
        .space_id;
    let ch = text_channel(&mut n, &admin, space);
    for u in [&bob, &spam, &carol] {
        n.make(
            &admin,
            Some(space),
            EventKind::MemberAdd { user: u.pk },
            NOW + 2,
        )
        .unwrap();
    }
    let mut t = NOW + 10;
    for i in 0..5 {
        t += 1;
        n.make(&admin, Some(space), msg(ch, &format!("admin {i}")), t)
            .unwrap();
    }
    for i in 0..20 {
        t += 1;
        n.make(&bob, Some(space), msg(ch, &format!("bob {i}")), t)
            .unwrap();
    }
    let big = "x".repeat(2000);
    let mut spam_ids = Vec::new();
    for _ in 0..300 {
        t += 1;
        spam_ids.push(n.make(&spam, Some(space), msg(ch, &big), t).unwrap().id());
    }
    // Bob reagiert auf die erste Spam-Nachricht.
    t += 1;
    n.make(
        &bob,
        Some(space),
        EventKind::Reaction {
            target: spam_ids[0],
            emoji: "x".into(),
        },
        t,
    )
    .unwrap();

    let before = prune::stored_bytes(&n.conn).unwrap();
    let limit = before / 2;
    let out = prune::enforce(&mut n.conn, &admin.pk, limit).unwrap();
    assert!(out.pruned > 0);
    assert!(out.spaces.contains(&space));
    assert!(prune::stored_bytes(&n.conn).unwrap() <= limit);

    let by = pruned_by(&n.conn);
    // Nur Spam und Bobs Reaktion darauf; beim Spammer die aeltesten, lueckenlos ab dem ersten.
    assert!(!by.contains_key(&admin.pk));
    assert_eq!(by[&bob.pk].len(), 1, "nur Bobs Reaktion");
    let seqs = &by[&spam.pk];
    assert_eq!(seqs[0], 1);
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1));
    assert!((seqs.len() as i64) < 300, "nicht mehr als noetig");
    assert_eq!(messages_of(&n.conn, &admin.pk), 5);
    assert_eq!(messages_of(&n.conn, &bob.pk), 20);
    assert_eq!(messages_of(&n.conn, &spam.pk), 300 - seqs.len() as i64);
    let reactions: i64 = n
        .conn
        .query_row("SELECT count(*) FROM reactions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(reactions, 0);

    // Zweiter Lauf unter der Grenze: nichts zu tun.
    assert_eq!(
        prune::enforce(&mut n.conn, &admin.pk, limit)
            .unwrap()
            .pruned,
        0
    );

    // Carol kommt neu dazu und bekommt alles, Platzhalter eingeschlossen.
    let mut c = Node::new();
    for _ in 0..3 {
        pull(&n, &mut c, &carol.pk, t + 100);
    }
    assert_eq!(event_count(&c.conn).unwrap(), event_count(&n.conn).unwrap());
    assert_eq!(c.dump(), n.dump());
    // Die Folge des Spammers geht bei Carol weiter.
    t += 1;
    n.make(&spam, Some(space), msg(ch, "weiter"), t).unwrap();
    pull(&n, &mut c, &carol.pk, t + 100);
    assert_eq!(
        messages_of(&c.conn, &spam.pk),
        messages_of(&n.conn, &spam.pk)
    );
}

/// Eigene Nachrichten kommen zuletzt dran, auch wenn sie am meisten Platz belegen.
#[test]
fn storage_limit_prunes_own_messages_last() {
    let (me, bob) = (User::new(), User::new());
    let mut n = Node::new();
    let space = n
        .make(&me, None, EventKind::SpaceCreate { name: "S".into() }, NOW)
        .unwrap()
        .body
        .space_id;
    let ch = text_channel(&mut n, &me, space);
    n.make(
        &me,
        Some(space),
        EventKind::MemberAdd { user: bob.pk },
        NOW + 2,
    )
    .unwrap();
    let mut t = NOW + 10;
    for _ in 0..100 {
        t += 1;
        n.make(&me, Some(space), msg(ch, &"m".repeat(3000)), t)
            .unwrap();
    }
    for _ in 0..50 {
        t += 1;
        n.make(&bob, Some(space), msg(ch, &"b".repeat(1000)), t)
            .unwrap();
    }
    let used = prune::stored_bytes(&n.conn).unwrap();
    prune::enforce(&mut n.conn, &me.pk, used - 20_000).unwrap();
    let by = pruned_by(&n.conn);
    assert!(by.contains_key(&bob.pk));
    assert!(!by.contains_key(&me.pk));
}

/// Platzhalter gibt es nur fuer Nachrichten-Arten: ein "aufgeraeumtes" MemberAdd wird
/// abgelehnt, sonst liesse sich eine Einladung oder Entfernung verstecken.
#[test]
fn stubs_of_membership_events_are_rejected() {
    let (admin, bob) = (User::new(), User::new());
    let mut n = Node::new();
    let create = n
        .make(
            &admin,
            None,
            EventKind::SpaceCreate { name: "S".into() },
            NOW,
        )
        .unwrap();
    let space = create.body.space_id;
    let add = n
        .make(
            &admin,
            Some(space),
            EventKind::MemberAdd { user: bob.pk },
            NOW + 1,
        )
        .unwrap();
    let mut stub = add.clone();
    stub.body.kind = add.body.kind.pruned();
    assert_eq!(stub.id(), add.id());
    assert_eq!(verify(&stub, NOW + 2), Err(Reject::Malformed));
    let mut m = Node::new();
    let out = m.take(vec![create, stub], NOW + 2);
    assert_eq!(out.accepted.len(), 1);
    // Einen Platzhalter selbst erzeugen geht auch nicht.
    assert!(n
        .make(
            &admin,
            Some(space),
            EventKind::Pruned {
                tag: app_proto::event::tag::MESSAGE,
                content: [0; 32],
            },
            NOW + 3,
        )
        .is_err());
}

use super::*;
use app_net::Access;
use app_testkit::{free_udp_port, random_key, TestNode};
use data_encoding::HEXLOWER;
use ed25519_dalek::SigningKey;
use std::process::{Command, Stdio};
use std::time::Instant;

const T: Duration = Duration::from_secs(10);

fn p(status: PresenceStatus) -> Presence {
    Presence {
        status,
        voice: None,
        streaming: false,
    }
}

struct P {
    net: Net,
    svc: PresenceService,
    ev: mpsc::UnboundedReceiver<PresenceEvent>,
    local: Arc<LocalState>,
    user: PublicKey,
    device: PublicKey,
}

async fn node(
    user_key: SigningKey,
    device_key: SigningKey,
    port: Option<u16>,
    allow: &[PublicKey],
    is_member: MemberCheck,
) -> P {
    let bind = port.map(|p| format!("127.0.0.1:{p}").parse().unwrap());
    let n = TestNode::spawn_with("n", user_key, device_key, bind, None).await;
    for u in allow {
        n.dir.set(*u, Access::Allowed);
    }
    let TestNode {
        net,
        mut rx,
        user,
        device,
        ..
    } = n;
    let (svc, ev) = PresenceService::new(net.clone(), p(PresenceStatus::Online), is_member);
    let s = svc.clone();
    tokio::spawn(async move {
        while let Some(e) = rx.recv().await {
            s.on_net_event(&e);
        }
    });
    let local = Arc::new(LocalState::new(
        svc.clone(),
        p(PresenceStatus::Online),
        DEFAULT_AWAY_AFTER,
    ));
    P {
        net,
        svc,
        ev,
        local,
        user,
        device,
    }
}

fn all() -> MemberCheck {
    Arc::new(|_, _| true)
}

async fn wait_for(
    svc: &PresenceService,
    user: PublicKey,
    mut f: impl FnMut(Option<Presence>) -> bool,
    limit: Duration,
) -> Duration {
    let t0 = Instant::now();
    while t0.elapsed() < limit {
        if f(svc.visible(&user)) {
            return t0.elapsed();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "Bedingung nicht erreicht, Zustand: {:?}",
        svc.visible(&user)
    );
}

#[test]
fn auto_away_only_affects_online() {
    let a = Duration::from_secs(600);
    use PresenceStatus::*;
    assert_eq!(
        effective_status(Online, Duration::from_secs(599), a),
        Online
    );
    assert_eq!(effective_status(Online, a, a), Away);
    assert_eq!(effective_status(DoNotDisturb, a * 10, a), DoNotDisturb);
    assert_eq!(effective_status(Invisible, a * 10, a), Invisible);
    assert_eq!(effective_status(Away, Duration::ZERO, a), Away);
}

#[cfg(windows)]
#[test]
fn idle_time_is_plausible() {
    assert!(idle_time() < Duration::from_secs(60 * 60 * 24 * 40));
}

#[tokio::test(flavor = "multi_thread")]
async fn status_changes_arrive_fast_and_invisible_looks_offline() {
    let (ua, da, ub, db) = (random_key(), random_key(), random_key(), random_key());
    let (ua_pub, ub_pub) = (ua.verifying_key().to_bytes(), ub.verifying_key().to_bytes());
    let a = node(ua, da, None, &[ub_pub], all()).await;
    let mut b = node(ub, db, None, &[ua_pub], all()).await;
    a.net.add_target(b.net.loopback_addr());

    // Initialer Zustand kommt beim Verbinden an.
    wait_for(
        &b.svc,
        a.user,
        |s| matches!(&s, Some(x) if x.status == PresenceStatus::Online),
        T,
    )
    .await;
    assert_eq!(
        b.ev.recv().await,
        Some(PresenceEvent::Changed { user: a.user })
    );
    assert_eq!(a.svc.online_users().len(), 1);

    // Aenderungen in <= 2 s.
    let idle = Duration::ZERO;
    let ch = [
        (PresenceStatus::DoNotDisturb, None, false),
        (PresenceStatus::Online, Some(([1; 16], [2; 16])), false),
        (PresenceStatus::Online, Some(([1; 16], [2; 16])), true),
    ];
    for (status, voice, streaming) in ch {
        a.local.update(
            |x| {
                *x = Presence {
                    status,
                    voice,
                    streaming,
                }
            },
            idle,
        );
        let want = Presence {
            status,
            voice,
            streaming,
        };
        let dt = wait_for(
            &b.svc,
            a.user,
            |s| s.as_ref() == Some(&want),
            Duration::from_secs(2),
        )
        .await;
        assert!(dt < Duration::from_secs(2), "{dt:?}");
    }

    // Unsichtbar: die Verbindung steht, aber der Peer sieht "offline".
    a.local
        .update(|x| x.status = PresenceStatus::Invisible, idle);
    wait_for(&b.svc, a.user, |s| s.is_none(), Duration::from_secs(2)).await;
    assert!(b.net.is_connected(&a.device));
    assert!(b.svc.online_users().is_empty());
    // Und wieder sichtbar.
    a.local.update(|x| x.status = PresenceStatus::Online, idle);
    wait_for(&b.svc, a.user, |s| s.is_some(), Duration::from_secs(2)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn only_space_members_learn_the_voice_channel() {
    let (ua, da, ub, db) = (random_key(), random_key(), random_key(), random_key());
    let (ua_pub, ub_pub) = (ua.verifying_key().to_bytes(), ub.verifying_key().to_bytes());
    let secret: SpaceId = [7; 16];
    let open: SpaceId = [8; 16];
    // B ist nur in `open` Mitglied.
    let check: MemberCheck = Arc::new(move |s, u| *u == ub_pub && *s == open);
    let a = node(ua, da, None, &[ub_pub], check).await;
    let b = node(ub, db, None, &[ua_pub], all()).await;
    a.net.add_target(b.net.loopback_addr());
    wait_for(&b.svc, a.user, |s| s.is_some(), T).await;

    let idle = Duration::ZERO;
    a.local.update(
        |x| {
            x.voice = Some((secret, [1; 16]));
            x.streaming = true;
            x.status = PresenceStatus::DoNotDisturb;
        },
        idle,
    );
    // Der Status kommt an, der fremde Channel und der Stream nicht.
    let seen = wait_for(
        &b.svc,
        a.user,
        |s| matches!(&s, Some(x) if x.status == PresenceStatus::DoNotDisturb),
        Duration::from_secs(2),
    )
    .await;
    assert!(seen < Duration::from_secs(2));
    let got = b.svc.visible(&a.user).unwrap();
    assert_eq!((got.voice, got.streaming), (None, false));

    // Im eigenen Space sieht B beides.
    a.local.update(|x| x.voice = Some((open, [2; 16])), idle);
    wait_for(
        &b.svc,
        a.user,
        |s| matches!(&s, Some(x) if x.voice == Some((open, [2; 16])) && x.streaming),
        Duration::from_secs(2),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_away_task_switches_and_returns() {
    let (ua, da, ub, db) = (random_key(), random_key(), random_key(), random_key());
    let (ua_pub, ub_pub) = (ua.verifying_key().to_bytes(), ub.verifying_key().to_bytes());
    let a = node(ua, da, None, &[ub_pub], all()).await;
    let b = node(ub, db, None, &[ua_pub], all()).await;
    a.net.add_target(b.net.loopback_addr());
    wait_for(&b.svc, a.user, |s| s.is_some(), T).await;

    let idle = Arc::new(Mutex::new(Duration::ZERO));
    let src = idle.clone();
    let local = Arc::new(LocalState::new(
        a.svc.clone(),
        p(PresenceStatus::Online),
        Duration::from_secs(60),
    ));
    let h = local.spawn_auto_away(Duration::from_millis(50), move || *src.lock().unwrap());

    *idle.lock().unwrap() = Duration::from_secs(61);
    wait_for(
        &b.svc,
        a.user,
        |s| matches!(&s, Some(x) if x.status == PresenceStatus::Away),
        Duration::from_secs(2),
    )
    .await;
    *idle.lock().unwrap() = Duration::ZERO;
    wait_for(
        &b.svc,
        a.user,
        |s| matches!(&s, Some(x) if x.status == PresenceStatus::Online),
        Duration::from_secs(2),
    )
    .await;
    h.abort();
}

fn key_from_env(name: &str) -> SigningKey {
    let v = HEXLOWER
        .decode(std::env::var(name).unwrap().as_bytes())
        .unwrap();
    SigningKey::from_bytes(&v.try_into().unwrap())
}

fn pub_from_env(name: &str) -> PublicKey {
    HEXLOWER
        .decode(std::env::var(name).unwrap().as_bytes())
        .unwrap()
        .try_into()
        .unwrap()
}

/// Kindprozess: ein Knoten, der "streamt" und sich mit dem Elternprozess verbindet.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "wird nur vom Absturztest als Kindprozess gestartet"]
async fn crash_child() {
    if std::env::var_os("CHILD_USER").is_none() {
        return;
    }
    let port: u16 = std::env::var("CHILD_PORT").unwrap().parse().unwrap();
    let parent_user = pub_from_env("PARENT_USER");
    let parent_dev = pub_from_env("PARENT_DEV");
    let parent_port: u16 = std::env::var("PARENT_PORT").unwrap().parse().unwrap();
    let c = node(
        key_from_env("CHILD_USER"),
        key_from_env("CHILD_DEV"),
        Some(port),
        &[parent_user],
        all(),
    )
    .await;
    c.local.update(|x| x.streaming = true, Duration::ZERO);
    let addr = iroh::EndpointAddr::new(iroh::EndpointId::from_bytes(&parent_dev).unwrap())
        .with_ip_addr(format!("127.0.0.1:{parent_port}").parse().unwrap());
    c.net.add_target(addr);
    println!("ready");
    tokio::time::sleep(Duration::from_secs(3600)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crashed_peer_counts_as_offline_within_30s() {
    use std::io::{BufRead, BufReader};
    let (pu, pd, cu, cd) = (random_key(), random_key(), random_key(), random_key());
    let (parent_port, child_port) = (free_udp_port(), free_udp_port());
    let cu_pub = cu.verifying_key().to_bytes();
    let parent = node(pu.clone(), pd.clone(), Some(parent_port), &[cu_pub], all()).await;

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "presence::tests::crash_child",
            "--include-ignored",
            "--nocapture",
        ])
        .env("CHILD_USER", HEXLOWER.encode(&cu.to_bytes()))
        .env("CHILD_DEV", HEXLOWER.encode(&cd.to_bytes()))
        .env("CHILD_PORT", child_port.to_string())
        .env("PARENT_USER", HEXLOWER.encode(&parent.user))
        .env("PARENT_DEV", HEXLOWER.encode(&parent.device))
        .env("PARENT_PORT", parent_port.to_string())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    while !lines.next().unwrap().unwrap().contains("ready") {}

    wait_for(
        &parent.svc,
        cu_pub,
        |s| matches!(&s, Some(x) if x.streaming),
        T,
    )
    .await;
    child.kill().unwrap(); // harter Abbruch, keine saubere Trennung
    child.wait().unwrap();
    let dt = wait_for(
        &parent.svc,
        cu_pub,
        |s| s.is_none(),
        Duration::from_secs(30),
    )
    .await;
    println!("Absturz erkannt nach {dt:?}");
    assert!(dt < Duration::from_secs(30));
}

use app_net::{Access, NetEvent, PathKind};
use app_proto::control::ControlMessage;
use app_testkit::{free_udp_port, random_key, TestNode};
use bytes::Bytes;
use std::time::{Duration, Instant};

const T: Duration = Duration::from_secs(10);

fn is_up(dev: [u8; 32]) -> impl FnMut(&NetEvent) -> Option<()> {
    move |e| matches!(e, NetEvent::PeerUp(i) if i.device == dev).then_some(())
}

#[tokio::test(flavor = "multi_thread")]
async fn three_nodes_connect_and_exchange_all_channels() {
    let mut a = TestNode::spawn("Anna").await;
    let mut b = TestNode::spawn("Ben").await;
    let mut c = TestNode::spawn("Carl").await;
    for (x, y) in [(&a, &b), (&a, &c), (&b, &c)] {
        x.allow(y);
        y.allow(x);
    }
    // Dreieck: A->B, B->C, C->A
    a.net.add_target(b.net.loopback_addr());
    b.net.add_target(c.net.loopback_addr());
    c.net.add_target(a.net.loopback_addr());

    let (da, db, dc) = (a.device, b.device, c.device);
    for (n, others) in [(&mut a, [db, dc]), (&mut b, [da, dc]), (&mut c, [da, db])] {
        let mut seen = std::collections::HashSet::new();
        while seen.len() < 2 {
            let d = n
                .wait_for(T, |e| match e {
                    NetEvent::PeerUp(i) => Some(i.device),
                    _ => None,
                })
                .await
                .expect("PeerUp");
            assert!(others.contains(&d));
            seen.insert(d);
        }
        assert_eq!(n.net.peers().len(), 2);
    }
    assert_eq!(a.net.path_kind(&b.device), PathKind::Direct);

    // Control A -> B
    a.net
        .send_control(&b.device, ControlMessage::Ping(7))
        .await
        .unwrap();
    let got = b
        .wait_for(T, |e| match e {
            NetEvent::Control {
                from,
                msg: ControlMessage::Ping(n),
            } => Some((from.display_name.clone(), *n)),
            _ => None,
        })
        .await
        .expect("Control");
    assert_eq!(got, ("Anna".to_string(), 7));

    // Datagramm B -> C
    b.net
        .send_datagram(&c.device, Bytes::from_static(b"voice"))
        .unwrap();
    let d = c
        .wait_for(T, |e| match e {
            NetEvent::Datagram { from, data } => Some((*from, data.clone())),
            _ => None,
        })
        .await
        .expect("Datagramm");
    assert_eq!(d, (b.device, Bytes::from_static(b"voice")));
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_and_video_streams_are_routed() {
    let mut a = TestNode::spawn("A").await;
    let mut b = TestNode::spawn("B").await;
    a.allow(&b);
    b.allow(&a);
    a.net.add_target(b.net.loopback_addr());
    a.wait_for(T, is_up(b.device)).await.unwrap();
    b.wait_for(T, is_up(a.device)).await.unwrap();

    let (mut s, _r) = a.net.open_sync(&b.device).await.unwrap();
    s.write_all(b"sync!").await.unwrap();
    s.finish().unwrap();
    let mut recv = b
        .wait_for(T, |e| match e {
            NetEvent::SyncStream { .. } => Some(()),
            _ => None,
        })
        .await;
    assert!(recv.take().is_some());

    // Video: unidirektionaler Stream B -> A
    let mut v = b.net.open_video(&a.device).await.unwrap();
    v.write_all(b"frame").await.unwrap();
    v.finish().unwrap();
    let got = a
        .wait_for(T, |e| match e {
            NetEvent::VideoStream { from, .. } => Some(*from),
            _ => None,
        })
        .await;
    assert_eq!(got, Some(b.device));
}

#[tokio::test(flavor = "multi_thread")]
async fn strangers_forged_and_blocked_devices_are_rejected() {
    let mut a = TestNode::spawn("A").await;

    // 1. Unbekannter User mit gueltigem Zertifikat.
    let stranger = TestNode::spawn("Fremder").await;
    stranger.allow(&a);
    stranger.net.add_target(a.net.loopback_addr());

    // 2. Gefaelschtes Zertifikat: gehoert zu einem anderen Geraeteschluessel.
    let other_device = random_key();
    let forged = TestNode::spawn_with(
        "Faelscher",
        random_key(),
        random_key(),
        None,
        Some(&other_device),
    )
    .await;
    a.dir.set(forged.user, Access::Allowed);
    forged.allow(&a);
    forged.net.add_target(a.net.loopback_addr());

    // 3. Geblockter User.
    let blocked = TestNode::spawn("Geblockt").await;
    a.dir.set(blocked.user, Access::Blocked);
    blocked.allow(&a);
    blocked.net.add_target(a.net.loopback_addr());

    let up = a
        .wait_for(Duration::from_secs(3), |e| {
            matches!(e, NetEvent::PeerUp(_)).then_some(())
        })
        .await;
    assert!(up.is_none(), "kein fremdes Geraet darf verbunden werden");
    assert!(a.net.peers().is_empty());
    for n in [&stranger, &forged, &blocked] {
        assert!(n.net.peers().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn client_also_checks_the_server() {
    // A kennt B, B kennt A nicht -> auch A darf nicht als verbunden gelten.
    let mut a = TestNode::spawn("A").await;
    let b = TestNode::spawn("B").await;
    a.allow(&b);
    a.net.add_target(b.net.loopback_addr());
    let up = a
        .wait_for(Duration::from_secs(3), |e| {
            matches!(e, NetEvent::PeerUp(_)).then_some(())
        })
        .await;
    assert!(up.is_none());
    assert!(b.net.peers().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnects_automatically_after_peer_restart() {
    let mut a = TestNode::spawn("A").await;
    let port = free_udp_port();
    let (b_user, b_dev) = (random_key(), random_key());
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    let b = TestNode::spawn_with("B", b_user.clone(), b_dev.clone(), Some(addr), None).await;
    a.allow(&b);
    b.allow(&a);
    a.net.add_target(b.net.loopback_addr());
    a.wait_for(T, is_up(b.device))
        .await
        .expect("erste Verbindung");

    // B verschwindet hart.
    b.net.shutdown().await;
    drop(b);
    a.wait_for(T, |e| matches!(e, NetEvent::PeerDown { .. }).then_some(()))
        .await
        .expect("PeerDown");
    assert!(a.net.peers().is_empty());

    // B kommt mit demselben Schluessel zurueck.
    let t0 = Instant::now();
    let mut b2 = TestNode::spawn_with("B", b_user, b_dev, Some(addr), None).await;
    b2.allow(&a);
    a.wait_for(T, is_up(b2.device)).await.expect("Reconnect");
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "Reconnect dauerte {:?}",
        t0.elapsed()
    );
    b2.wait_for(T, is_up(a.device)).await.expect("PeerUp auf B");
}

#[tokio::test(flavor = "multi_thread")]
async fn simultaneous_dials_end_in_one_working_connection() {
    let mut a = TestNode::spawn("A").await;
    let mut b = TestNode::spawn("B").await;
    a.allow(&b);
    b.allow(&a);
    a.net.add_target(b.net.loopback_addr());
    b.net.add_target(a.net.loopback_addr());
    a.wait_for(T, is_up(b.device)).await.unwrap();
    b.wait_for(T, is_up(a.device)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(a.net.peers().len(), 1);
    assert_eq!(b.net.peers().len(), 1);
    a.net
        .send_control(&b.device, ControlMessage::Ping(1))
        .await
        .unwrap();
    b.net
        .send_control(&a.device, ControlMessage::Ping(2))
        .await
        .unwrap();
    let ping = |want: u64| {
        move |e: &NetEvent| {
            matches!(e, NetEvent::Control { msg: ControlMessage::Ping(n), .. } if *n == want)
                .then_some(())
        }
    };
    b.wait_for(T, ping(1)).await.expect("A->B");
    a.wait_for(T, ping(2)).await.expect("B->A");
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnect_user_drops_a_live_connection_and_keeps_it_out() {
    // Blockieren in der App: Allowlist aendern + disconnect_user. Die laufende Verbindung
    // muss sofort weg sein, und B (der weiter neu waehlt) darf nicht wieder hinein.
    let mut a = TestNode::spawn("A").await;
    let mut b = TestNode::spawn("B").await;
    a.allow(&b);
    b.allow(&a);
    // Nur B waehlt (wie ein Freund, der sich meldet): keine Doppelverbindung, deren
    // Aufloesung den Zustand waehrend des Tests veraendert.
    b.net.add_target(a.net.loopback_addr());
    a.wait_for(T, is_up(b.device)).await.expect("PeerUp auf A");
    b.wait_for(T, is_up(a.device)).await.expect("PeerUp auf B");

    a.dir.set(b.user, Access::Blocked);
    let t0 = Instant::now();
    assert_eq!(a.net.disconnect_user(&b.user), vec![b.device]);
    b.wait_for(T, |e| matches!(e, NetEvent::PeerDown { .. }).then_some(()))
        .await
        .expect("B wird getrennt");
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
    assert!(!a.net.is_connected(&b.device));

    // B versucht es weiter (Backoff ab 100 ms) – A weist jeden Versuch ab.
    let back = a
        .wait_for(Duration::from_secs(3), |e| {
            matches!(e, NetEvent::PeerUp(_)).then_some(())
        })
        .await;
    assert!(
        back.is_none(),
        "blockierter Nutzer darf nicht wieder verbinden"
    );
    assert!(a.net.peers().is_empty() && b.net.peers().is_empty());

    // Unbekannter Nutzer: nichts zu trennen.
    assert!(a.net.disconnect_user(&[0u8; 32]).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn strangers_cannot_tie_up_the_handshake() {
    // Ein Fremder darf `app/peer/1` oeffnen, aber (a) keinen riesigen Handshake-Frame ankuendigen
    // (der Puffer wird vor dem Empfang angelegt) und (b) nicht beliebig viele Handshakes offen
    // halten.
    let a = TestNode::spawn("A").await;
    let stranger = TestNode::spawn("Fremder").await;
    let ep = stranger.net.endpoint();

    // (a) 1 MB angekuendigt: A bricht sofort ab, statt 10 s auf die Daten zu warten.
    let conn = ep
        .connect(a.net.loopback_addr(), app_proto::alpn::PEER)
        .await
        .unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    send.write_all(&1_000_000u32.to_be_bytes()).await.unwrap();
    let t0 = Instant::now();
    tokio::time::timeout(Duration::from_secs(3), conn.closed())
        .await
        .expect("A schliesst die Verbindung");
    assert!(t0.elapsed() < Duration::from_secs(3));

    // (b) Viele haengende Handshakes: ab der Grenze weist A neue Verbindungen sofort ab.
    let mut hanging = Vec::new();
    let mut refused = 0;
    for _ in 0..80 {
        match tokio::time::timeout(
            Duration::from_secs(3),
            ep.connect(a.net.loopback_addr(), app_proto::alpn::PEER),
        )
        .await
        {
            Ok(Ok(c)) => hanging.push(c),
            _ => refused += 1,
        }
    }
    assert!(refused >= 80 - 64, "nur {refused} abgewiesen");
    assert!(hanging.len() <= 64, "{} offen", hanging.len());
    drop(hanging);
}

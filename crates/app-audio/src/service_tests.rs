use super::*;
use crate::{FRAME, RATE};
use app_testkit::TestNode;
use std::time::{Duration, Instant};

struct N {
    t: TestNode,
    svc: VoiceService,
}

fn tone(freq: f32, frame: usize) -> Vec<f32> {
    (0..FRAME)
        .map(|k| {
            let t = (frame * FRAME + k) as f32 / RATE as f32;
            0.2 * (2.0 * std::f32::consts::PI * freq * t).sin()
        })
        .collect()
}

fn power_at(x: &[f32], freq: f32) -> f32 {
    let w = 2.0 * std::f32::consts::PI * freq / RATE as f32;
    let (mut re, mut im) = (0.0f32, 0.0f32);
    for (i, s) in x.iter().enumerate() {
        re += s * (w * i as f32).cos();
        im += s * (w * i as f32).sin();
    }
    (re * re + im * im).sqrt() / x.len() as f32
}

async fn until(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(Instant::now() < end, "Zeitueberschreitung: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn channel_members_hear_each_other_and_outsiders_do_not() {
    // A, B, C sind Mitglieder; D ist befreundet, aber nicht im Space.
    let shared: Arc<Mutex<HashSet<PublicKey>>> = Arc::default();
    let mut nodes = Vec::new();
    for name in ["A", "B", "C", "D"] {
        let mut t = TestNode::spawn(name).await;
        let core = Arc::new(Mutex::new(VoiceCore::new(64).unwrap()));
        let sh = shared.clone();
        let check: MemberCheck = Arc::new(move |_, u| sh.lock().unwrap().contains(u));
        let (svc, _ev) = VoiceService::new(t.net.clone(), core, check);
        let mut rx = std::mem::replace(&mut t.rx, mpsc::channel(1).1);
        let s = svc.clone();
        tokio::spawn(async move {
            while let Some(e) = rx.recv().await {
                s.on_net_event(&e);
            }
        });
        nodes.push(N { t, svc });
    }
    for n in &nodes[..3] {
        shared.lock().unwrap().insert(n.t.user);
    }
    for i in 0..4 {
        for j in 0..4 {
            if i != j {
                nodes[i].t.allow(&nodes[j].t);
                if i < j {
                    nodes[i].t.net.add_target(nodes[j].t.net.loopback_addr());
                }
            }
        }
    }
    until("Mesh", || nodes.iter().all(|n| n.t.net.peers().len() == 3)).await;

    let (space, ch) = ([1u8; 16], [2u8; 16]);
    for n in &nodes {
        n.svc.join(space, ch); // auch D versucht es
    }
    until("Teilnehmerlisten", || {
        nodes[..3].iter().all(|n| n.svc.participants().len() == 2)
    })
    .await;
    assert!(nodes[3].svc.participants().is_empty());
    assert!(nodes[..3].iter().all(|n| n
        .svc
        .participants()
        .iter()
        .all(|p| p.user != nodes[3].t.user)));

    // A (440 Hz) und C (880 Hz) sprechen, B hoert beide; D hoert nichts.
    let mut got = Vec::new();
    let mut out = vec![0.0; FRAME];
    for i in 0..80usize {
        nodes[0].svc.submit_frame(&tone(440.0, i));
        nodes[2].svc.submit_frame(&tone(880.0, i));
        tokio::time::sleep(Duration::from_millis(20)).await;
        if i >= 20 {
            nodes[1].svc.core().lock().unwrap().mix(&mut out);
            got.extend_from_slice(&out);
        }
    }
    assert!(power_at(&got, 440.0) > 0.02, "A nicht zu hoeren");
    assert!(power_at(&got, 880.0) > 0.02, "C nicht zu hoeren");
    {
        let d = nodes[3].svc.core();
        let mut d = d.lock().unwrap();
        assert_eq!(d.peer_count(), 0);
        d.mix(&mut out);
        assert!(out.iter().all(|s| *s == 0.0));
    }
    let (_, lv) = nodes[1].svc.core().lock().unwrap().levels();
    assert_eq!(lv.len(), 2);

    // Stumm-Status kommt bei den anderen an.
    nodes[0].svc.set_state(true, false);
    let a_dev = nodes[0].t.device;
    until("Stumm-Status", || {
        nodes[1]
            .svc
            .participants()
            .iter()
            .any(|p| p.device == a_dev && p.muted)
    })
    .await;

    // Leave: B hat A nicht mehr in der Liste.
    nodes[0].svc.leave();
    until("Leave", || nodes[1].svc.participants().len() == 1).await;
    assert!(nodes[0].svc.participants().is_empty());

    // Verbindungsabbruch entfernt den Teilnehmer.
    nodes[2].t.net.shutdown().await;
    until("PeerDown", || nodes[1].svc.participants().is_empty()).await;
}

async fn trio() -> Vec<N> {
    // Alle befreundet, niemand ist Space-Mitglied: Anrufe brauchen keinen Space.
    let mut nodes = Vec::new();
    for name in ["A", "B", "C"] {
        let mut t = TestNode::spawn(name).await;
        let core = Arc::new(Mutex::new(VoiceCore::new(64).unwrap()));
        let check: MemberCheck = Arc::new(|_, _| false);
        let (svc, _ev) = VoiceService::new(t.net.clone(), core, check);
        let svc = svc.with_ring_timeout(Duration::from_millis(600));
        let mut rx = std::mem::replace(&mut t.rx, mpsc::channel(1).1);
        let s = svc.clone();
        tokio::spawn(async move {
            while let Some(e) = rx.recv().await {
                s.on_net_event(&e);
            }
        });
        nodes.push(N { t, svc });
    }
    for i in 0..3 {
        for j in 0..3 {
            if i != j {
                nodes[i].t.allow(&nodes[j].t);
                if i < j {
                    nodes[i].t.net.add_target(nodes[j].t.net.loopback_addr());
                }
            }
        }
    }
    until("Mesh", || nodes.iter().all(|n| n.t.net.peers().len() == 2)).await;
    nodes
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_call_ring_accept_talk_hangup() {
    let n = trio().await;
    let (a, b, c) = (&n[0], &n[1], &n[2]);
    a.svc.call(b.t.user).unwrap();
    until(
        "klingelt",
        || matches!(b.svc.call_state(), CallState::Incoming { user } if user == a.t.user),
    )
    .await;
    assert!(matches!(a.svc.call_state(), CallState::Outgoing { .. }));
    assert_eq!(c.svc.call_state(), CallState::Idle);
    // Wer schon klingelt, ist besetzt.
    assert!(a.svc.call(c.t.user).is_err());

    b.svc.accept_call();
    until("verbunden", || {
        matches!(a.svc.call_state(), CallState::Active { .. })
            && matches!(b.svc.call_state(), CallState::Active { .. })
            && a.svc.participants().len() == 1
            && b.svc.participants().len() == 1
    })
    .await;

    // Audio fliesst, C hoert nichts.
    let mut got = Vec::new();
    let mut out = vec![0.0; FRAME];
    for i in 0..60usize {
        a.svc.submit_frame(&tone(440.0, i));
        tokio::time::sleep(Duration::from_millis(20)).await;
        if i >= 20 {
            b.svc.core().lock().unwrap().mix(&mut out);
            got.extend_from_slice(&out);
        }
    }
    assert!(power_at(&got, 440.0) > 0.02);
    assert_eq!(c.svc.core().lock().unwrap().peer_count(), 0);

    // Auflegen beendet das Gespraech auf beiden Seiten.
    b.svc.hangup();
    until("aufgelegt", || {
        a.svc.call_state() == CallState::Idle && b.svc.call_state() == CallState::Idle
    })
    .await;
    assert!(a.svc.current().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_call_decline_cancel_timeout_and_busy() {
    let n = trio().await;
    let (a, b, c) = (&n[0], &n[1], &n[2]);

    // Ablehnen.
    a.svc.call(b.t.user).unwrap();
    until("klingelt", || {
        matches!(b.svc.call_state(), CallState::Incoming { .. })
    })
    .await;
    b.svc.decline_call();
    until("abgelehnt", || a.svc.call_state() == CallState::Idle).await;
    assert_eq!(
        a.svc.call_note(),
        Some(app_proto::i18n::tr("Declined", "Abgelehnt"))
    );

    // Anrufer legt vor dem Annehmen auf.
    a.svc.call(b.t.user).unwrap();
    until("klingelt 2", || {
        matches!(b.svc.call_state(), CallState::Incoming { .. })
    })
    .await;
    a.svc.cancel_call();
    until("abgebrochen", || b.svc.call_state() == CallState::Idle).await;
    assert_eq!(
        b.svc.call_note(),
        Some(app_proto::i18n::tr("Missed call", "Verpasster Anruf"))
    );

    // Keine Antwort: nach dem Timeout ist alles wieder frei.
    a.svc.call(b.t.user).unwrap();
    until("Zeitablauf", || {
        a.svc.call_state() == CallState::Idle && b.svc.call_state() == CallState::Idle
    })
    .await;
    assert_eq!(
        a.svc.call_note(),
        Some(app_proto::i18n::tr("No answer", "Keine Antwort"))
    );

    // Besetzt: B ist im Gespraech mit C, A ruft B an.
    c.svc.call(b.t.user).unwrap();
    until("C klingelt", || {
        matches!(b.svc.call_state(), CallState::Incoming { .. })
    })
    .await;
    b.svc.accept_call();
    until("B und C sprechen", || {
        matches!(c.svc.call_state(), CallState::Active { .. })
    })
    .await;
    a.svc.call(b.t.user).unwrap();
    until("besetzt", || a.svc.call_state() == CallState::Idle).await;
    assert_eq!(
        a.svc.call_note(),
        Some(app_proto::i18n::tr("Busy", "Besetzt"))
    );

    // Nicht erreichbar.
    n[2].t.net.shutdown().await;
    until("C weg", || b.svc.call_state() == CallState::Idle).await;
}

//! M3-03: Video-Fan-out ueber echte (Loopback-)Verbindungen.

use app_net::video::{read_video_frame, VideoSender};
use app_net::NetEvent;
use app_proto::control::ControlMessage;
use app_proto::video::{VideoCodec, VideoMsg};
use app_testkit::TestNode;
use bytes::Bytes;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const T: Duration = Duration::from_secs(10);
const FRAME_BYTES: usize = 20_000;

#[derive(Clone, Copy, Debug)]
struct Got {
    key: bool,
    at: Instant,
}

type Log = Arc<Mutex<Vec<Got>>>;

fn up(dev: [u8; 32]) -> impl FnMut(&NetEvent) -> Option<()> {
    move |e| matches!(e, NetEvent::PeerUp(i) if i.device == dev).then_some(())
}

/// Zuschauer: liest alle eingehenden Frames und merkt sich Ankunftszeit und Keyframe-Flag.
fn spawn_viewer(mut node: TestNode, log: Log) {
    tokio::spawn(async move {
        while let Some(ev) = node.rx.recv().await {
            if let NetEvent::VideoStream { mut recv, .. } = ev {
                let log = log.clone();
                tokio::spawn(async move {
                    if let Ok((h, _)) = read_video_frame(&mut recv, Duration::from_secs(2)).await {
                        log.lock().unwrap().push(Got {
                            key: h.is_key(),
                            at: Instant::now(),
                        });
                    }
                });
            }
        }
    });
}

async fn subscribe(node_net: &app_net::Net, to: [u8; 32]) {
    node_net
        .send_control(
            &to,
            ControlMessage::Video(VideoMsg::Subscribe {
                stream_id: 1,
                layer: 0,
                decoders: vec![VideoCodec::H264],
            }),
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn late_viewer_sees_keyframe_fast_and_stalled_viewer_does_not_hurt_others() {
    let mut a = TestNode::spawn("Sender").await;
    let mut b = TestNode::spawn("Fix").await;
    let mut c = TestNode::spawn("Haengt").await;
    let mut d = TestNode::spawn("Spaet").await;
    for v in [&b, &c, &d] {
        a.allow(v);
        v.allow(&a);
        v.net.add_target(a.net.loopback_addr());
    }
    let mut seen = std::collections::HashSet::new();
    while seen.len() < 3 {
        let dev = a
            .wait_for(T, |e| match e {
                NetEvent::PeerUp(i) => Some(i.device),
                _ => None,
            })
            .await
            .expect("PeerUp");
        seen.insert(dev);
    }
    for v in [&mut b, &mut c, &mut d] {
        v.wait_for(T, up(a.device)).await.expect("PeerUp");
    }

    let sender = VideoSender::new(a.net.clone(), 1, VideoCodec::H264);
    let want_key = Arc::new(AtomicBool::new(false));

    // Sender: Control-Nachrichten der Zuschauer verarbeiten.
    {
        let (sender, want_key) = (sender.clone(), want_key.clone());
        tokio::spawn(async move {
            while let Some(ev) = a.rx.recv().await {
                if let NetEvent::Control {
                    from,
                    msg: ControlMessage::Video(m),
                } = ev
                {
                    if sender.on_message(&from.device, &m) == Ok(true) {
                        want_key.store(true, Ordering::Release);
                    }
                }
            }
        });
    }

    let (log_b, log_d): (Log, Log) = (Log::default(), Log::default());
    let (b_net, c_net, d_net) = (b.net.clone(), c.net.clone(), d.net.clone());
    let a_dev = sender_dev(&b);
    spawn_viewer(b, log_b.clone());
    // C liest nie: der Knoten bleibt am Leben, nimmt aber nichts ab.
    let _stalled = c;
    spawn_viewer(d, log_d.clone());

    subscribe(&b_net, a_dev).await;
    subscribe(&c_net, a_dev).await;

    // Encoder-Simulation: 60 fps, Keyframe jede Sekunde oder auf Anforderung.
    let sender2 = sender.clone();
    let enc = tokio::spawn(async move {
        let data = Bytes::from(vec![7u8; FRAME_BYTES]);
        let mut tick = tokio::time::interval(Duration::from_millis(16));
        let (mut sent, mut skipped) = (0usize, 0usize);
        for i in 0..250u32 {
            tick.tick().await;
            let key = i % 60 == 0 || want_key.swap(false, Ordering::AcqRel);
            let out = sender2.send(&app_net::video::OutFrame {
                layer: 0,
                key,
                codec: VideoCodec::H264,
                ts: i * 16,
                data: data.clone(),
            });
            sent += out.sent;
            skipped += out.skipped;
            if out.want_key {
                want_key.store(true, Ordering::Release);
            }
        }
        (sent, skipped)
    });

    // D steigt nach 1,3 s ein, mitten zwischen zwei regulaeren Keyframes.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let joined = Instant::now();
    subscribe(&d_net, a_dev).await;

    let (sent, skipped) = enc.await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // D: erstes Bild ist ein Keyframe und kommt innerhalb von 1 s.
    let first_d = {
        let l = log_d.lock().unwrap();
        let mut v: Vec<Got> = l.clone();
        v.sort_by_key(|g| g.at);
        *v.first().expect("D hat nichts bekommen")
    };
    assert!(first_d.key, "erstes Bild von D ist kein Keyframe");
    let wait = first_d.at.duration_since(joined);
    assert!(wait <= Duration::from_secs(1), "D wartete {wait:?}");

    // B: laeuft fluessig weiter, obwohl C haengt.
    let mut at: Vec<Instant> = log_b.lock().unwrap().iter().map(|g| g.at).collect();
    at.sort();
    assert!(at.len() >= 150, "B bekam nur {} von 250 Frames", at.len());
    let max_gap = at.windows(2).map(|w| w[1] - w[0]).max().unwrap();
    assert!(
        max_gap < Duration::from_millis(300),
        "B stockte {max_gap:?}"
    );

    // C wurde abgekoppelt (Frames ausgelassen), ohne den Sender zu bremsen.
    assert!(
        skipped > 0,
        "der haengende Zuschauer wurde nicht abgekoppelt"
    );
    assert!(sent > 0);
}

fn sender_dev(any_viewer: &TestNode) -> [u8; 32] {
    // Das einzige verbundene Geraet eines Zuschauers ist der Sender.
    any_viewer.net.peers()[0].device
}

#[tokio::test(flavor = "multi_thread")]
async fn exhausted_budget_refuses_new_viewers_and_reports_lower_the_bitrate() {
    use app_net::rate::Budget;
    use app_proto::video::{ReceiverReport, RefuseReason};
    let a = TestNode::spawn("Sender").await;
    let sender = VideoSender::new(a.net.clone(), 1, VideoCodec::H264);
    // 3 Mbit/s Upload: 80 % = 2400 -> zwei Zuschauer zu je 1200, ein dritter nicht.
    sender.set_budget(Budget { upload_kbps: 3000 }, 0);
    let sub = VideoMsg::Subscribe {
        stream_id: 1,
        layer: 0,
        decoders: vec![VideoCodec::H264],
    };
    assert_eq!(sender.on_message(&[1; 32], &sub), Ok(true));
    assert_eq!(sender.on_message(&[2; 32], &sub), Ok(true));
    assert_eq!(
        sender.on_message(&[3; 32], &sub),
        Err(RefuseReason::Capacity)
    );
    // Ein Zuschauer, der schon drin ist, darf erneut abonnieren.
    assert_eq!(sender.on_message(&[2; 32], &sub), Ok(true));
    // Kein gemeinsamer Codec.
    let av1 = VideoMsg::Subscribe {
        stream_id: 1,
        layer: 0,
        decoders: vec![VideoCodec::Av1],
    };
    assert_eq!(
        sender.on_message(&[4; 32], &av1),
        Err(RefuseReason::NoCommonCodec)
    );
    assert!(sender.target_kbps() <= 1200);

    // Schlechte Berichte senken die Zielbitrate, aber nicht unter das Minimum.
    let a_one = Budget {
        upload_kbps: 20_000,
    };
    sender.set_budget(a_one, 0);
    sender.remove_viewer(&[2; 32]);
    let before = sender.target_kbps();
    let bad = ReceiverReport {
        stream_id: 1,
        interval_ms: 1000,
        received: 30,
        lost: 30,
        late: 0,
        jitter_ms: 0,
        kbps: 2000,
    };
    sender.on_message(&[1; 32], &VideoMsg::Report(bad)).unwrap();
    let ad = sender.adapt();
    let (layer, new) = ad.bitrates[0];
    assert_eq!(layer, 0);
    assert!(new < before && new >= 1000);
}

#[tokio::test(flavor = "multi_thread")]
async fn weak_viewer_moves_to_the_small_layer_while_the_others_keep_full_quality() {
    use app_net::rate::Budget;
    use app_proto::video::ReceiverReport;
    let a = TestNode::spawn("Sender").await;
    let sender = VideoSender::new(a.net.clone(), 1, VideoCodec::H264);
    sender.set_budget(
        Budget {
            upload_kbps: 30_000,
        },
        0,
    );
    let sub = |layer| VideoMsg::Subscribe {
        stream_id: 1,
        layer,
        decoders: vec![VideoCodec::H264],
    };
    // Ohne Simulcast gibt es nur Stufe 0, auch wenn der Zuschauer Stufe 1 will.
    assert_eq!(sender.on_message(&[1; 32], &sub(1)), Ok(true));
    assert_eq!(sender.viewer_layer(&[1; 32]), Some(0));
    assert_eq!(sender.on_message(&[2; 32], &sub(0)), Ok(true));
    assert!(sender.set_layers(2).is_empty());

    let report = |lost| {
        VideoMsg::Report(ReceiverReport {
            stream_id: 1,
            interval_ms: 1000,
            received: 60 - lost,
            lost,
            late: 0,
            jitter_ms: 0,
            kbps: 3000,
        })
    };
    let frame = |layer, key| app_net::video::OutFrame {
        layer,
        key,
        codec: VideoCodec::H264,
        ts: 0,
        data: Bytes::from_static(&[1; 10]),
    };
    let mut changed = Vec::new();
    for _ in 0..3 {
        sender.on_message(&[1; 32], &report(30)).unwrap();
        sender.on_message(&[2; 32], &report(0)).unwrap();
        let ad = sender.adapt();
        changed.extend(ad.key_layers);
    }
    // Nur der schwache Zuschauer ist auf Stufe 1 gelandet und braucht dort einen Keyframe.
    assert_eq!(sender.viewer_layer(&[1; 32]), Some(1));
    assert_eq!(sender.viewer_layer(&[2; 32]), Some(0));
    assert_eq!(changed, vec![1]);
    // Frames gehen nur an die Zuschauer ihrer Stufe; ein Delta-Frame ohne Keyframe wird
    // ausgelassen und fordert einen an.
    tokio::time::sleep(Duration::from_millis(350)).await;
    let out = sender.send(&frame(1, false));
    assert_eq!((out.sent, out.skipped), (0, 1));
    assert!(out.want_key);
    assert_eq!(sender.send(&frame(1, true)).sent, 1);
    assert_eq!(sender.send(&frame(0, true)).sent, 1);
    // Stufe 1 hat ihre eigene Zielbitrate unter der Obergrenze.
    assert!(sender.layer_target_kbps(1) <= app_net::rate::LAYER1_KBPS);

    // Simulcast aus: alle zurueck auf Stufe 0, dort ein Keyframe noetig.
    assert_eq!(sender.set_layers(1), vec![0]);
    assert_eq!(sender.viewer_layer(&[1; 32]), Some(0));
}

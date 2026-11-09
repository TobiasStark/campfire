//! M3-06: Bild vom Encoder ueber echte (Loopback-)Verbindungen bis zum Decoder.
//! Pruefen: Simulcast-Encoder -> VideoSender -> Netz -> ReceiverTracker -> Decoder, fuer beide
//! Stufen, mit Einstieg durch Keyframe.

use app_net::video::{read_video_frame, Arrival, OutFrame, ReceiverTracker, VideoSender};
use app_net::NetEvent;
use app_proto::control::ControlMessage;
use app_proto::video::{VideoCodec, VideoMsg};
use app_testkit::TestNode;
use app_video::decode::Decoder;
use app_video::encode::{probe, Bgra, Simulcast};
use app_video::select::{Codec, Resolution, StreamOptions};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const T: Duration = Duration::from_secs(10);

/// Was ein Zuschauer von einer Stufe gesehen hat.
#[derive(Default, Debug)]
struct Seen {
    decoded: usize,
    size: Option<(u32, u32)>,
    first_at: Option<Instant>,
    lost: u32,
}

fn pattern(w: u32, h: u32, n: u32) -> Vec<u8> {
    let mut v = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let o = ((y * w + x) * 4) as usize;
            v[o] = ((x + n * 3) % 256) as u8;
            v[o + 1] = ((y + n * 2) % 256) as u8;
            v[o + 2] = ((x ^ y) % 256) as u8;
            v[o + 3] = 255;
        }
    }
    v
}

/// Zuschauer: liest Frames, vermisst sie und dekodiert.
fn spawn_viewer(mut node: TestNode, seen: Arc<Mutex<Seen>>, key_wanted: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let start = Instant::now();
        let tracker = Arc::new(Mutex::new(ReceiverTracker::new(1, 0)));
        let dec = Arc::new(Mutex::new(None::<Decoder>));
        while let Some(ev) = node.rx.recv().await {
            let NetEvent::VideoStream { mut recv, .. } = ev else {
                continue;
            };
            let (seen, tracker, dec, key_wanted) = (
                seen.clone(),
                tracker.clone(),
                dec.clone(),
                key_wanted.clone(),
            );
            tokio::spawn(async move {
                let Ok((h, body)) = read_video_frame(&mut recv, Duration::from_secs(2)).await
                else {
                    return;
                };
                let arrival = tracker
                    .lock()
                    .unwrap()
                    .on_frame(&h, start.elapsed().as_millis() as u64);
                match arrival {
                    Arrival::Decode => {}
                    Arrival::DropAndRequestKey => {
                        key_wanted.store(true, Ordering::Release);
                        return;
                    }
                    Arrival::Drop => return,
                }
                let mut d = dec.lock().unwrap();
                if d.is_none() {
                    *d = Some(Decoder::new(Codec::H264, false).unwrap());
                }
                let frames = d.as_mut().unwrap().decode(&body).unwrap();
                let mut s = seen.lock().unwrap();
                for f in frames {
                    s.first_at.get_or_insert_with(Instant::now);
                    s.size = Some((f.w, f.h));
                    s.decoded += 1;
                }
                s.lost = tracker.lock().unwrap().report(1000).lost;
            });
        }
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn both_layers_reach_the_viewer_and_decode_at_their_own_size() {
    let Some(info) = probe()
        .iter()
        .find(|e| e.name == "h264_nvenc" || e.name == "libopenh264")
        .copied()
    else {
        return;
    };
    let mut a = TestNode::spawn("Sender").await;
    let mut hi = TestNode::spawn("Gross").await;
    let mut lo = TestNode::spawn("Klein").await;
    for v in [&hi, &lo] {
        a.allow(v);
        v.allow(&a);
        v.net.add_target(a.net.loopback_addr());
    }
    let mut n = 0;
    while n < 2 {
        a.wait_for(T, |e| matches!(e, NetEvent::PeerUp(_)).then_some(()))
            .await
            .expect("PeerUp");
        n += 1;
    }
    for v in [&mut hi, &mut lo] {
        v.wait_for(T, |e| matches!(e, NetEvent::PeerUp(_)).then_some(()))
            .await
            .expect("PeerUp");
    }

    let sender = VideoSender::new(a.net.clone(), 1, VideoCodec::H264);
    sender.set_layers(2);
    let want_key = [
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    ];
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
                        let l = sender.viewer_layer(&from.device).unwrap_or(0);
                        want_key[usize::from(l)].store(true, Ordering::Release);
                    }
                }
            }
        });
    }

    let (seen_hi, seen_lo) = (
        Arc::new(Mutex::new(Seen::default())),
        Arc::new(Mutex::new(Seen::default())),
    );
    let (hi_net, lo_net) = (hi.net.clone(), lo.net.clone());
    let a_dev = hi.net.peers()[0].device;
    let hi_key = Arc::new(AtomicBool::new(false));
    spawn_viewer(hi, seen_hi.clone(), hi_key);
    spawn_viewer(lo, seen_lo.clone(), Arc::new(AtomicBool::new(false)));
    let sub = |layer| {
        ControlMessage::Video(VideoMsg::Subscribe {
            stream_id: 1,
            layer,
            decoders: vec![VideoCodec::H264],
        })
    };
    let started = Instant::now();
    hi_net.send_control(&a_dev, sub(0)).await.unwrap();
    lo_net.send_control(&a_dev, sub(1)).await.unwrap();

    // Encoder: 1080p, 30 fps Quelle, zwei Stufen.
    let opts = StreamOptions {
        resolution: Resolution::P1080,
        fps: 30,
        simulcast: true,
        ..StreamOptions::default()
    };
    let mut sim = Simulcast::new(info, &opts);
    assert_eq!(sim.layer_count(), 2);
    let frames: Vec<Vec<u8>> = (0..8).map(|i| pattern(1920, 1080, i)).collect();
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    for i in 0..90u32 {
        tick.tick().await;
        for (l, flag) in want_key.iter().enumerate() {
            if flag.swap(false, Ordering::AcqRel) {
                sim.request_keyframe(l as u8);
            }
        }
        let img = Bgra {
            data: &frames[(i % 8) as usize],
            width: 1920,
            height: 1080,
            pitch: 1920 * 4,
        };
        let ts = i64::from(i) * 33;
        for (layer, p) in sim.encode(&img, ts).unwrap() {
            let out = sender.send(&OutFrame {
                layer,
                key: p.key,
                codec: VideoCodec::H264,
                ts: ts as u32,
                data: bytes::Bytes::from(p.data),
            });
            if out.want_key {
                want_key[usize::from(layer)].store(true, Ordering::Release);
            }
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (h, l) = (seen_hi.lock().unwrap(), seen_lo.lock().unwrap());
    println!("gross: {h:?}\nklein: {l:?}");
    assert_eq!(h.size, Some((1920, 1080)), "{h:?}");
    assert_eq!(l.size, Some((1280, 720)), "{l:?}");
    assert!(h.decoded >= 60, "grosse Stufe: nur {} Bilder", h.decoded);
    assert!(l.decoded >= 60, "kleine Stufe: nur {} Bilder", l.decoded);
    for s in [&*h, &*l] {
        let first = s.first_at.expect("nichts dekodiert");
        assert!(
            first.duration_since(started) < Duration::from_secs(2),
            "erstes Bild nach {:?}",
            first.duration_since(started)
        );
        assert_eq!(s.lost, 0, "{s:?}");
    }
}

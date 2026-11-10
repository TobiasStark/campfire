//! Decoder-Sandbox (M-10): Kindprozess ist eingeschraenkt, Antworten werden geprueft, ein
//! haengender Decoder wird beendet. Die Test-Exe ist zugleich der (falsche) Decoder-Prozess.

use app_video::frame::YuvFrame;
use app_video::sandbox::{self, DecodeFn, RemoteDecoder};
use app_video::select::Codec;
use std::process::Command;
use std::time::{Duration, Instant};

const DECODER: &str = "--fake-decoder";
const PROBE: &str = "--probe-lockdown";
#[cfg(feature = "ffmpeg")]
const REAL: &str = "--real-decoder";

fn frame(w: u32, h: u32) -> YuvFrame {
    YuvFrame {
        id: 0,
        ts: 0,
        w,
        h,
        y: vec![1; (w * h) as usize],
        uv: vec![2; (w * (h / 2)) as usize],
    }
}

/// Falscher Decoder: "hang" haengt, "big" liefert ein falsches Bild, "bad" lehnt ab.
fn fake_decoder() -> ! {
    let r = sandbox::serve(|_, _| {
        let f: DecodeFn = Box::new(|data| match data {
            b"hang" => {
                std::thread::sleep(Duration::from_secs(60));
                Ok(vec![])
            }
            b"big" => {
                let mut f = frame(4, 2);
                f.y.push(0);
                Ok(vec![f])
            }
            b"bad" => Err(()),
            _ => Ok(vec![frame(4, 2)]),
        });
        Some((f, false))
    });
    std::process::exit(i32::from(r.is_err()))
}

/// Prueft im Kindprozess, was nach `lockdown` noch geht. Exit-Code 0: nichts davon.
fn probe_lockdown(target: &str) -> ! {
    // Erst im Job, dann einschraenken (wie der Decoder-Prozess).
    std::thread::sleep(Duration::from_millis(300));
    if sandbox::lockdown().is_err() {
        std::process::exit(20);
    }
    if std::fs::write(target, b"x").is_ok() {
        std::process::exit(21);
    }
    if Command::new("cmd").args(["/c", "exit"]).status().is_ok() {
        std::process::exit(22);
    }
    std::process::exit(0)
}

fn me() -> Command {
    Command::new(std::env::current_exe().unwrap())
}

fn decoder() -> RemoteDecoder {
    let mut cmd = me();
    cmd.arg(DECODER);
    RemoteDecoder::spawn(cmd, Codec::H264, true).unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some(DECODER) => fake_decoder(),
        Some(PROBE) => probe_lockdown(&args[2]),
        #[cfg(feature = "ffmpeg")]
        Some(REAL) => sandbox::child_main(),
        _ => {}
    }
    #[cfg(feature = "ffmpeg")]
    real_decoder();

    // Normaler Ablauf.
    let mut d = decoder();
    assert!(!d.is_hardware());
    let f = d.decode(b"frame").unwrap();
    assert_eq!((f.len(), f[0].w, f[0].h, f[0].y.len()), (1, 4, 2, 8));
    // Abgelehntes Paket: Fehler, Prozess lebt weiter.
    assert!(d.decode(b"bad").is_err());
    assert!(d.is_alive());
    assert_eq!(d.decode(b"frame").unwrap().len(), 1);
    // Ungueltiges Bild: Prozess gilt als kompromittiert.
    assert!(d.decode(b"big").is_err());
    assert!(!d.is_alive());
    assert!(d.decode(b"frame").is_err());
    println!("protocol ok");

    // Haengender Decoder wird beendet.
    let mut d = decoder();
    d.set_timeout(Duration::from_millis(500));
    let t = Instant::now();
    assert!(d.decode(b"hang").is_err());
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    assert!(!d.is_alive());
    println!("watchdog ok");

    // Eingeschraenkter Prozess: kein Schreiben in Nutzerordner, keine Kindprozesse.
    let target = std::env::temp_dir().join(format!("campfire-sandbox-{}", std::process::id()));
    let mut cmd = me();
    cmd.arg(PROBE).arg(&target);
    let (mut child, _job) = sandbox::spawn_sandboxed(cmd).unwrap();
    let status = child.wait().unwrap();
    let _ = std::fs::remove_file(&target);
    assert_eq!(status.code(), Some(0), "lockdown probe");
    println!("lockdown ok");
}

/// Echter ffmpeg-Decoder im eingeschraenkten Prozess, mit und ohne d3d11va.
#[cfg(feature = "ffmpeg")]
fn real_decoder() {
    use app_video::encode::{probe, Bgra, Encoder};
    use app_video::select::{Resolution, StreamOptions};
    let info = probe()
        .iter()
        .find(|e| e.codec == Codec::H264)
        .copied()
        .expect("H.264-Encoder");
    let opts = StreamOptions {
        resolution: Resolution::Native,
        ..StreamOptions::default()
    };
    let (w, h) = (640u32, 360u32);
    let img = vec![128u8; (w * h * 4) as usize];
    let mut e = Encoder::new(info, opts);
    let mut packets = Vec::new();
    for n in 0..10i64 {
        let b = Bgra {
            data: &img,
            width: w,
            height: h,
            pitch: (w * 4) as usize,
        };
        packets.extend(e.encode(&b, n * 17).unwrap());
    }
    packets.extend(e.finish());
    for hw in [false, true] {
        let mut cmd = me();
        cmd.arg(REAL);
        let mut d = RemoteDecoder::spawn(cmd, Codec::H264, hw).unwrap();
        let mut frames = Vec::new();
        for p in &packets {
            frames.extend(d.decode(&p.data).unwrap());
        }
        let f = frames.last().expect("Bilder");
        assert_eq!((f.w, f.h), (w, h));
        println!(
            "real decoder ok ({})",
            if d.is_hardware() {
                "d3d11va"
            } else {
                "software"
            }
        );
    }
}

//! M0-07 Video-Transport-Spike: Encoder -> ein unidirektionaler QUIC-Stream pro Frame -> Decoder.
//! Quelle ist ein synthetisches 1080p-Testbild (Capture ist in M0-05 getestet).
//!
//! video_quic send [--encoder h264_nvenc] [--mbps 20] [--fps 60] [--secs 30] [--max-age 150] [--drop-every N]
//! video_quic recv <ID> [--secs 30]
//! Steuerung: Der Empfänger öffnet einen langlebigen uni-Stream zum Sender und schreibt pro
//! Keyframe-Anforderung ein Byte 'K'. Frames, die nicht innerhalb von --max-age ms bestätigt
//! sind, werden per Reset abgebrochen; danach wird ein Keyframe erzwungen.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use ffmpeg::{codec, encoder, format::Pixel, frame, picture, Dictionary, Packet};
use ffmpeg_next as ffmpeg;
use iroh::{endpoint::presets, Endpoint, EndpointId, RelayMode};

const ALPN: &[u8] = b"app/video/1";
const W: u32 = 1920;
const H: u32 = 1080;
const HDR: usize = 17; // seq u32, capture_ms u64, key u8, len u32

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn arg<T: std::str::FromStr>(a: &[String], n: &str, d: T) -> T {
    a.iter()
        .position(|x| x == n)
        .and_then(|i| a.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn open_encoder(name: &str, fps: i32, mbps: usize) -> Result<encoder::Video> {
    let codec = encoder::find_by_name(name).context("Encoder nicht im Build")?;
    let mut enc = codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()?;
    enc.set_width(W);
    enc.set_height(H);
    enc.set_format(if name.starts_with("lib") {
        Pixel::YUV420P
    } else {
        Pixel::NV12
    });
    enc.set_time_base((1, 1000));
    enc.set_frame_rate(Some((fps, 1)));
    let bps = mbps * 1_000_000;
    enc.set_bit_rate(bps);
    enc.set_max_b_frames(0);
    enc.set_gop((fps * 4) as u32);
    let mut o = Dictionary::new();
    if name.ends_with("_nvenc") {
        for (k, v) in [
            ("preset", "p1"),
            ("tune", "ll"),
            ("zerolatency", "1"),
            ("rc", "cbr"),
            ("delay", "0"),
            ("forced-idr", "1"),
        ] {
            o.set(k, v);
        }
    } else if name.ends_with("_amf") {
        for (k, v) in [
            ("usage", "ultralowlatency"),
            ("rc", "cbr"),
            ("quality", "speed"),
            ("forced_idr", "1"),
        ] {
            o.set(k, v);
        }
    }
    o.set("maxrate", &bps.to_string());
    o.set("bufsize", &(bps / fps as usize * 2).to_string());
    Ok(enc.open_with(o)?)
}

struct EncOut {
    seq: u32,
    cap_ms: u64,
    key: bool,
    data: Vec<u8>,
}

/// Synthetisches Bild: bewegtes Muster (Y), neutrale Farbe (UV).
fn fill(f: &mut frame::Video, t: u32, planar: bool) {
    let stride = f.stride(0);
    let y = f.data_mut(0);
    for row in 0..H as usize {
        let line = &mut y[row * stride..row * stride + W as usize];
        for (x, p) in line.iter_mut().enumerate() {
            *p = (((x >> 3) ^ (row >> 3)) as u32)
                .wrapping_mul(7)
                .wrapping_add(t * 5) as u8;
        }
    }
    let planes = if planar { 3 } else { 2 };
    for pl in 1..planes {
        f.data_mut(pl).fill(128);
    }
}

fn encoder_thread(
    name: String,
    fps: i32,
    mbps: usize,
    secs: u64,
    force_key: Arc<AtomicBool>,
    tx: tokio::sync::mpsc::UnboundedSender<EncOut>,
) -> Result<()> {
    let mut enc = open_encoder(&name, fps, mbps)?;
    let planar = name.starts_with("lib");
    let pix = if planar { Pixel::YUV420P } else { Pixel::NV12 };
    let mut fr = frame::Video::new(pix, W, H);
    let t0 = Instant::now();
    let mut n = 0u32;
    let mut seq = 0u32;
    let mut pending: std::collections::VecDeque<u64> = Default::default();
    while t0.elapsed() < Duration::from_secs(secs) {
        let due = t0 + Duration::from_micros(1_000_000 * n as u64 / fps as u64);
        if let Some(w) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(w);
        }
        let cap = now_ms();
        fill(&mut fr, n, planar);
        fr.set_pts(Some(t0.elapsed().as_millis() as i64));
        fr.set_kind(if force_key.swap(false, Ordering::Relaxed) {
            picture::Type::I
        } else {
            picture::Type::None
        });
        pending.push_back(cap);
        enc.send_frame(&fr)?;
        let mut pkt = Packet::empty();
        while enc.receive_packet(&mut pkt).is_ok() {
            let cap_ms = pending.pop_front().unwrap_or(cap);
            if let Some(d) = pkt.data() {
                let _ = tx.send(EncOut {
                    seq,
                    cap_ms,
                    key: pkt.is_key(),
                    data: d.to_vec(),
                });
                seq += 1;
            }
        }
        n += 1;
    }
    Ok(())
}

async fn run_send(a: &[String]) -> Result<()> {
    let name: String = arg(a, "--encoder", "h264_nvenc".to_string());
    let (mbps, fps, secs, max_age): (usize, i32, u64, u64) = (
        arg(a, "--mbps", 20),
        arg(a, "--fps", 60),
        arg(a, "--secs", 30),
        arg(a, "--max-age", 150),
    );
    let ep = Endpoint::builder(presets::N0)
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(RelayMode::Default)
        .bind()
        .await?;
    ep.online().await;
    println!("Deine ID:\n\n{}\n\nWarte auf Empfänger ...", ep.id());
    let conn = ep.accept().await.context("Endpoint geschlossen")?.await?;
    println!(
        "Verbunden. Sende {name} {W}x{H}@{fps} {mbps} Mbit/s für {secs} s, max-age {max_age} ms."
    );

    let drop_every: u32 = arg(a, "--drop-every", 0);
    let force_key = Arc::new(AtomicBool::new(false));
    // Steuer-Stream
    {
        let c = conn.clone();
        let fk = force_key.clone();
        tokio::spawn(async move {
            if let Ok(mut s) = c.accept_uni().await {
                let mut b = [0u8; 1];
                while let Ok(Some(_)) = s.read(&mut b).await {
                    if b[0] == b'K' {
                        fk.store(true, Ordering::Relaxed);
                    }
                }
            }
        });
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let enc_fk = force_key.clone();
    let enc = std::thread::spawn(move || encoder_thread(name, fps, mbps, secs, enc_fk, tx));

    let sent = Arc::new(AtomicU64::new(0));
    let stale = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let (s2, st2, b2) = (sent.clone(), stale.clone(), bytes.clone());
    let reporter = tokio::spawn(async move {
        let mut last = (0u64, 0u64);
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let (s, b) = (s2.load(Ordering::Relaxed), b2.load(Ordering::Relaxed));
            println!(
                "gesendet {s} Frames, {:.1} Mbit/s, zu alt (Reset) {}",
                (b - last.1) as f64 * 8.0 / 2e6,
                st2.load(Ordering::Relaxed)
            );
            last = (s, b);
        }
    });
    while let Some(f) = rx.recv().await {
        let c = conn.clone();
        let (fk, st, sent, bytes) = (
            force_key.clone(),
            stale.clone(),
            sent.clone(),
            bytes.clone(),
        );
        tokio::spawn(async move {
            let mut buf = Vec::with_capacity(HDR + f.data.len());
            buf.extend_from_slice(&f.seq.to_le_bytes());
            buf.extend_from_slice(&f.cap_ms.to_le_bytes());
            buf.push(f.key as u8);
            buf.extend_from_slice(&(f.data.len() as u32).to_le_bytes());
            buf.extend_from_slice(&f.data);
            let Ok(mut s) = c.open_uni().await else {
                return;
            };
            if drop_every > 0 && f.seq > 0 && f.seq % drop_every == 0 {
                // Test: Frame absichtlich verlieren (wie ein Reset wegen Überalterung)
                let _ = s.reset(2u32.into());
                st.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let budget = Duration::from_millis(
                max_age
                    .saturating_sub(now_ms().saturating_sub(f.cap_ms))
                    .max(1),
            );
            let ok = async {
                s.write_all(&buf).await.ok()?;
                s.finish().ok()?;
                s.stopped().await.ok()?;
                Some(())
            };
            if tokio::time::timeout(budget, ok)
                .await
                .ok()
                .flatten()
                .is_some()
            {
                sent.fetch_add(1, Ordering::Relaxed);
                bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
            } else {
                let _ = s.reset(1u32.into());
                st.fetch_add(1, Ordering::Relaxed);
                fk.store(true, Ordering::Relaxed);
            }
        });
    }
    reporter.abort();
    match enc.join() {
        Ok(r) => r?,
        Err(_) => anyhow::bail!("Encoder-Thread abgestürzt"),
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    println!(
        "\n=== Sender fertig === gesendet {} | zu alt/abgebrochen {}",
        sent.load(Ordering::Relaxed),
        stale.load(Ordering::Relaxed)
    );
    conn.close(0u32.into(), b"done");
    ep.close().await;
    Ok(())
}

struct Rx {
    seq: u32,
    cap_ms: u64,
    key: bool,
    data: Vec<u8>,
}

#[derive(Default)]
struct Counters {
    frames: u64,
    bytes: u64,
    gaps: u64,
    key_req: u64,
    discarded: u64,
}

fn decoder_thread(
    rx: mpsc::Receiver<Rx>,
    kreq: tokio::sync::mpsc::UnboundedSender<()>,
    stop: Arc<AtomicBool>,
) -> Result<(Counters, Vec<f64>)> {
    let codec = ffmpeg::decoder::find(codec::Id::H264).context("kein H264-Decoder")?;
    let mut dec = codec::context::Context::new_with_codec(codec)
        .decoder()
        .video()?;
    let mut c = Counters::default();
    let mut lat = vec![];
    let mut last: Option<u32> = None;
    let mut need_key = false;
    let mut last_req = Instant::now() - Duration::from_secs(1);
    let mut interval = (Instant::now(), 0u64, 0u64);
    let mut raw = frame::Video::empty();
    while !stop.load(Ordering::Relaxed) {
        let Ok(f) = rx.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        if let Some(l) = last {
            if f.seq.wrapping_sub(l) as i32 <= 0 {
                c.discarded += 1;
                continue;
            }
            if f.seq != l.wrapping_add(1) {
                c.gaps += (f.seq - l - 1) as u64;
                need_key = true;
            }
        }
        last = Some(f.seq);
        if need_key && !f.key {
            c.discarded += 1;
            if last_req.elapsed() > Duration::from_millis(300) {
                let _ = kreq.send(());
                c.key_req += 1;
                last_req = Instant::now();
            }
            continue;
        }
        if f.key {
            need_key = false;
        }
        let pkt = Packet::copy(&f.data);
        if dec.send_packet(&pkt).is_err() {
            need_key = true;
            continue;
        }
        while dec.receive_frame(&mut raw).is_ok() {
            c.frames += 1;
            c.bytes += f.data.len() as u64;
            lat.push(now_ms().saturating_sub(f.cap_ms) as f64);
        }
        if interval.0.elapsed() >= Duration::from_secs(2) {
            let secs = interval.0.elapsed().as_secs_f64();
            let n = lat.len().min(120);
            let mut recent = lat[lat.len() - n..].to_vec();
            recent.sort_by(|a, b| a.total_cmp(b));
            println!(
                "{:.1} fps, {:.1} Mbit/s, Lücken {}, Keyframe-Anf. {}, Latenz (letzte {n}) p50 {:.0} / max {:.0} ms",
                (c.frames - interval.1) as f64 / secs,
                (c.bytes - interval.2) as f64 * 8.0 / secs / 1e6,
                c.gaps,
                c.key_req,
                recent.get(n / 2).copied().unwrap_or(0.0),
                recent.last().copied().unwrap_or(0.0)
            );
            interval = (Instant::now(), c.frames, c.bytes);
        }
    }
    Ok((c, lat))
}

async fn run_recv(a: &[String]) -> Result<()> {
    let id: EndpointId = a
        .first()
        .context("ID fehlt")?
        .trim()
        .parse()
        .context("Ungültige ID")?;
    let secs: u64 = arg(a, "--secs", 30);
    let ep = Endpoint::builder(presets::N0)
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(RelayMode::Default)
        .bind()
        .await?;
    ep.online().await;
    let conn = tokio::time::timeout(Duration::from_secs(30), ep.connect(id, ALPN))
        .await
        .context("Zeitüberschreitung")??;
    println!("Verbunden.");
    // Steuer-Stream
    let (ktx, mut krx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut ctl = conn.open_uni().await?;
    ctl.write_all(b"H").await?;
    tokio::spawn(async move {
        while krx.recv().await.is_some() {
            if ctl.write_all(b"K").await.is_err() {
                break;
            }
        }
    });
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, drx) = mpsc::channel::<Rx>();
    let dstop = stop.clone();
    let dec = std::thread::spawn(move || decoder_thread(drx, ktx, dstop));
    let deadline = Instant::now() + Duration::from_secs(secs);
    let aborted = Arc::new(AtomicU64::new(0));
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(Ok(mut s)) = tokio::time::timeout(left, conn.accept_uni()).await else {
            break;
        };
        let (tx, ab) = (tx.clone(), aborted.clone());
        tokio::spawn(async move {
            match s.read_to_end(8 << 20).await {
                Ok(b) if b.len() >= HDR => {
                    let seq = u32::from_le_bytes(b[0..4].try_into().unwrap_or_default());
                    let cap_ms = u64::from_le_bytes(b[4..12].try_into().unwrap_or_default());
                    let _ = tx.send(Rx {
                        seq,
                        cap_ms,
                        key: b[12] != 0,
                        data: b[HDR..].to_vec(),
                    });
                }
                _ => {
                    ab.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop.store(true, Ordering::Relaxed);
    let (c, mut lat) = match dec.join() {
        Ok(r) => r?,
        Err(_) => anyhow::bail!("Decoder-Thread abgestürzt"),
    };
    lat.sort_by(|a, b| a.total_cmp(b));
    let p = |q: f64| {
        lat.get(((lat.len().max(1) - 1) as f64 * q) as usize)
            .copied()
            .unwrap_or(0.0)
    };
    println!("\n=== Empfänger fertig ===");
    println!("Frames dekodiert {} | Lücken {} | verworfen {} | Keyframe-Anforderungen {} | vom Sender abgebrochene Streams {}", c.frames, c.gaps, c.discarded, c.key_req, aborted.load(Ordering::Relaxed));
    println!("Latenz Capture->Decoder-Ausgang: p50 {:.0} ms, p95 {:.0} ms, p99 {:.0} ms, max {:.0} ms (Uhren müssen synchron sein)", p(0.5), p(0.95), p(0.99), p(1.0));
    conn.close(0u32.into(), b"done");
    ep.close().await;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("send") => run_send(&args[1..]).await,
        Some("recv") => run_recv(&args[1..]).await,
        _ => {
            println!("Aufruf: video_quic send [...] | video_quic recv <ID> [--secs N]");
            Ok(())
        }
    }
}

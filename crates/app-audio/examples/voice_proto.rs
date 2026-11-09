//! M0-03 Voice-Prototyp: Mikrofon -> Opus -> QUIC-Datagramme (iroh) -> Jitter-Buffer -> Lautsprecher.
//!
//! cargo run -p app-audio --release --example voice_proto -- wait
//! cargo run -p app-audio --release --example voice_proto -- connect <ID>
//! Optionen: --tone (440-Hz-Ton statt Mikrofon), --mute-out (kein Lautsprecher, nur Statistik),
//!           --loss N (N % der empfangenen Pakete verwerfen), --secs N (Dauer, 0 = bis Strg+C),
//!           --kbps N (Opus-Bitrate, Standard 64)

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use app_audio::{percentile, unix_ms32, Header, Receiver, FRAME, RATE};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use iroh::{
    endpoint::{presets, Connection},
    Endpoint, EndpointId, RelayMode,
};
use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapRb,
};

const ALPN: &[u8] = b"app/voice/1";

struct Opts {
    tone: bool,
    mute_out: bool,
    loss: f32,
    secs: u64,
    kbps: i32,
}

fn cpu_seconds() -> f64 {
    use windows::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetCurrentProcess, GetProcessTimes},
    };
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: gültiges Pseudo-Handle und gültige Ausgabeparameter.
    unsafe {
        if GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).is_err() {
            return 0.0;
        }
    }
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
    t(k) + t(u)
}

fn encoder_thread(
    conn: Connection,
    opts: &Opts,
    mut cons: Option<<HeapRb<f32> as Split>::Cons>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut enc = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip)?;
    enc.set_bitrate(opus::Bitrate::Bits(opts.kbps * 1000))?;
    enc.set_inband_fec(true)?;
    enc.set_packet_loss_perc(10)?;
    let mut seq = 0u32;
    let mut pcm = vec![0f32; FRAME];
    let mut phase = 0f32;
    let t0 = Instant::now();
    let mut sent = 0u64;
    while !stop.load(Ordering::Relaxed) {
        match cons.as_mut() {
            None => {
                // Ton: auf 20-ms-Raster takten
                let due = t0 + Duration::from_millis(20 * seq as u64);
                if let Some(w) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(w);
                }
                for s in pcm.iter_mut() {
                    *s = 0.2 * phase.sin();
                    phase += 2.0 * std::f32::consts::PI * 440.0 / RATE as f32;
                }
                phase %= 2.0 * std::f32::consts::PI;
            }
            Some(c) => {
                if c.occupied_len() < FRAME {
                    std::thread::sleep(Duration::from_millis(3));
                    continue;
                }
                c.pop_slice(&mut pcm);
            }
        }
        let payload = enc.encode_vec_float(&pcm, 1200)?;
        let h = Header {
            flags: 0,
            stream_id: 0,
            seq,
            ts: unix_ms32(),
        };
        if conn.send_datagram(h.write(&payload).into()).is_err() {
            break;
        }
        seq = seq.wrapping_add(1);
        sent += 1;
        let _ = sent;
    }
    Ok(())
}

fn pick_config(
    cfgs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    what: &str,
) -> Result<cpal::StreamConfig> {
    for c in cfgs {
        if c.min_sample_rate() <= RATE && c.max_sample_rate() >= RATE {
            return Ok(c.with_sample_rate(RATE).config());
        }
    }
    bail!("{what}: kein 48-kHz-Format verfügbar")
}

async fn session(conn: Connection, opts: Opts) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let rx = Arc::new(Mutex::new(Receiver::new()?));

    // --- Aufnahme
    let mut _in_stream = None;
    let mut cons = None;
    if !opts.tone {
        let host = cpal::default_host();
        let dev = host
            .default_input_device()
            .context("kein Mikrofon gefunden (oder --tone benutzen)")?;
        let cfg = pick_config(dev.supported_input_configs()?, "Mikrofon")?;
        let ch = cfg.channels as usize;
        let (mut prod, c) = HeapRb::<f32>::new(RATE as usize).split();
        cons = Some(c);
        let s = dev.build_input_stream(
            cfg,
            move |data: &[f32], _| {
                for f in data.chunks(ch) {
                    let _ = prod.try_push(f.iter().sum::<f32>() / ch as f32);
                }
            },
            |e| eprintln!("Mikrofon-Fehler: {e}"),
            None,
        )?;
        s.play()?;
        _in_stream = Some(s);
        println!("Mikrofon: {} Kanäle, 48 kHz", ch);
    }

    let enc_conn = conn.clone();
    let enc_stop = stop.clone();
    let (kbps, tone, mute_out, loss, secs) =
        (opts.kbps, opts.tone, opts.mute_out, opts.loss, opts.secs);
    let enc_opts = Opts {
        tone,
        mute_out,
        loss,
        secs,
        kbps,
    };
    let enc = std::thread::spawn(move || encoder_thread(enc_conn, &enc_opts, cons, enc_stop));

    // --- Wiedergabe
    let mut _out_stream = None;
    if mute_out {
        let rx = rx.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0f32; FRAME];
            let t0 = Instant::now();
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                n += 1;
                let due = t0 + Duration::from_millis(20 * n);
                if let Some(w) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(w);
                }
                if let Ok(mut r) = rx.lock() {
                    r.pull(&mut buf, 0.0);
                }
            }
        });
    } else {
        let host = cpal::default_host();
        let dev = host
            .default_output_device()
            .context("kein Lautsprecher gefunden (oder --mute-out benutzen)")?;
        let cfg = pick_config(dev.supported_output_configs()?, "Lautsprecher")?;
        let ch = cfg.channels as usize;
        let rx = rx.clone();
        let mut pending = vec![0f32; FRAME];
        let mut pos = FRAME;
        let mut active = false;
        let s = dev.build_output_stream(
            cfg,
            move |data: &mut [f32], _| {
                for f in data.chunks_mut(ch) {
                    if pos >= FRAME {
                        let queued = 0.0;
                        active = rx
                            .lock()
                            .map(|mut r| r.pull(&mut pending, queued))
                            .unwrap_or(false);
                        if !active {
                            pending.fill(0.0);
                        }
                        pos = 0;
                    }
                    let v = pending[pos];
                    pos += 1;
                    f.fill(v);
                }
                let _ = active;
            },
            |e| eprintln!("Lautsprecher-Fehler: {e}"),
            None,
        )?;
        s.play()?;
        _out_stream = Some(s);
    }

    // --- Empfang
    let rx_conn = conn.clone();
    let rx_recv = rx.clone();
    let rx_stop = stop.clone();
    let recv_task = tokio::spawn(async move {
        while !rx_stop.load(Ordering::Relaxed) {
            match rx_conn.read_datagram().await {
                Ok(d) => {
                    if loss > 0.0 && rand::random::<f32>() * 100.0 < loss {
                        continue;
                    }
                    if let Some((h, p)) = Header::parse(&d) {
                        if let Ok(mut r) = rx_recv.lock() {
                            r.push(h, p);
                        }
                    }
                }
                Err(e) => {
                    println!("Verbindung beendet: {e}");
                    break;
                }
            }
        }
    });

    // --- Statistik
    println!(
        "Läuft. Beenden mit Strg+C{}.",
        if secs > 0 {
            format!(" oder nach {secs} s")
        } else {
            String::new()
        }
    );
    let start = Instant::now();
    let mut last_cpu = cpu_seconds();
    let mut last_t = Instant::now();
    let mut cpu_samples = vec![];
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            _ = tokio::signal::ctrl_c() => break,
        }
        let now = Instant::now();
        let cpu = cpu_seconds();
        let pct = (cpu - last_cpu) / now.duration_since(last_t).as_secs_f64() * 100.0;
        (last_cpu, last_t) = (cpu, now);
        cpu_samples.push(pct);
        let (line, closed) = {
            let r = rx.lock().map_err(|_| anyhow::anyhow!("lock"))?;
            let s = &r.stats;
            let mut l = s.latency_ms.clone();
            (
                format!(
                    "{:>4}s rx={} verloren={} PLC={} FEC={} spät={} accel={} rebuf={} jitter={:.1}ms ziel={}fr lat p50/p95={:.0}/{:.0}ms cpu={:.1}%",
                    start.elapsed().as_secs(), s.received, s.lost_on_wire(), s.plc, s.fec, s.late,
                    s.accel, s.rebuffer, s.jitter_ms, s.target,
                    percentile(&mut l, 0.5), percentile(&mut l, 0.95), pct
                ),
                conn.close_reason().is_some(),
            )
        };
        println!("{line}");
        if closed || (secs > 0 && start.elapsed().as_secs() >= secs) {
            break;
        }
    }
    stop.store(true, Ordering::Relaxed);
    recv_task.abort();
    let _ = enc.join();

    let r = rx.lock().map_err(|_| anyhow::anyhow!("lock"))?;
    let s = &r.stats;
    let mut l = s.latency_ms.clone();
    cpu_samples.sort_by(|a, b| a.total_cmp(b));
    println!("\n=== Ergebnis ===");
    println!(
        "Pakete empfangen: {} | auf der Leitung verloren: {} | spät: {}",
        s.received,
        s.lost_on_wire(),
        s.late
    );
    println!(
        "Verdeckt: FEC {} | PLC {} | Neu-Puffern {} | Beschleunigt {}",
        s.fec, s.plc, s.rebuffer, s.accel
    );
    println!(
        "Latenz Aufnahme->Jitter-Buffer-Ausgang (Uhren beider Seiten müssen synchron sein): p50 {:.0} ms, p95 {:.0} ms, max {:.0} ms",
        percentile(&mut l, 0.5), percentile(&mut l, 0.95), percentile(&mut l, 1.0)
    );
    println!(
        "Jitter-Schätzung {:.1} ms, Puffer-Ziel {} Frames",
        s.jitter_ms, s.target
    );
    println!(
        "CPU (Median der Messungen, % eines Kerns): {:.1}",
        cpu_samples
            .get(cpu_samples.len() / 2)
            .copied()
            .unwrap_or(0.0)
    );
    conn.close(0u32.into(), b"done");
    Ok(())
}

async fn bind() -> Result<Endpoint> {
    Endpoint::builder(presets::N0)
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(RelayMode::Default)
        .bind()
        .await
        .context("Endpoint konnte nicht gestartet werden")
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |a: &mut Vec<String>, n: &str| -> bool {
        a.iter().position(|x| x == n).map(|i| a.remove(i)).is_some()
    };
    let val = |a: &mut Vec<String>, n: &str| -> Option<String> {
        let i = a.iter().position(|x| x == n)?;
        a.remove(i);
        (i < a.len()).then(|| a.remove(i))
    };
    let opts = Opts {
        tone: flag(&mut args, "--tone"),
        mute_out: flag(&mut args, "--mute-out"),
        loss: val(&mut args, "--loss")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0),
        secs: val(&mut args, "--secs")
            .and_then(|v| v.parse().ok())
            .unwrap_or(20),
        kbps: val(&mut args, "--kbps")
            .and_then(|v| v.parse().ok())
            .unwrap_or(64),
    };
    match args.first().map(String::as_str) {
        Some("wait") => {
            let ep = bind().await?;
            ep.online().await;
            println!("Deine ID:\n\n{}\n\nWarte auf Verbindung ...", ep.id());
            let incoming = ep.accept().await.context("Endpoint geschlossen")?;
            let conn = incoming.await?;
            println!("Verbunden.");
            session(conn, opts).await?;
            ep.close().await;
        }
        Some("connect") => {
            let id: EndpointId = args.get(1).context("ID fehlt")?.trim().parse().context("Ungültige ID")?;
            let ep = bind().await?;
            ep.online().await;
            let conn = tokio::time::timeout(Duration::from_secs(30), ep.connect(id, ALPN))
                .await
                .context("Zeitüberschreitung")??;
            println!("Verbunden.");
            session(conn, opts).await?;
            ep.close().await;
        }
        _ => println!("Aufruf: voice_proto wait|connect <ID> [--tone] [--mute-out] [--loss N] [--secs N] [--kbps N]"),
    }
    Ok(())
}

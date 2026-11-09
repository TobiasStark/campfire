//! Verbindungstest (M0-02): misst, ob zwei PCs direkt oder nur über ein Relay verbunden sind.
//!
//! Aufruf:
//!   nettest                 -> interaktiv (Doppelklick): Eingabe leer = warten, sonst ID
//!   nettest wait            -> warten, bis jemand sich verbindet
//!   nettest connect <ID>    -> zu <ID> verbinden und messen

use std::{
    io::Write as _,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use iroh::{
    endpoint::{presets, Connection},
    Endpoint, EndpointId, RelayMode,
};

const ALPN: &[u8] = b"app/nettest/1";
const TEST_SECS: u64 = 10;
const DIRECT_WAIT: Duration = Duration::from_secs(15);
const CHUNK: usize = 64 * 1024;

#[derive(Parser)]
#[command(about = "Verbindungstest für Campfire (M0-02)")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Warten, bis sich jemand verbindet
    Wait,
    /// Zu einer ID verbinden und messen
    Connect { id: String },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    None,
    Direct4,
    Direct6,
    Relay,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::None => "kein Pfad",
            Kind::Direct4 => "direkt IPv4",
            Kind::Direct6 => "direkt IPv6",
            Kind::Relay => "Relay",
        }
    }
}

#[derive(Clone, Copy)]
struct Sample {
    t_ms: u64,
    kind: Kind,
    rtt_ms: f64,
}

type Samples = Arc<Mutex<Vec<Sample>>>;

fn current_path(conn: &Connection) -> (Kind, f64) {
    let paths = conn.paths();
    let pick = paths
        .iter()
        .find(|p| p.is_selected())
        .or_else(|| paths.iter().next());
    match pick {
        None => (Kind::None, 0.0),
        Some(p) => {
            let kind = if p.is_relay() {
                Kind::Relay
            } else if p.is_ip() {
                match p.remote_addr() {
                    iroh::TransportAddr::Ip(sa) if sa.is_ipv6() => Kind::Direct6,
                    _ => Kind::Direct4,
                }
            } else {
                Kind::None
            };
            (kind, p.rtt().as_secs_f64() * 1000.0)
        }
    }
}

/// Fragt alle 200 ms die aktuelle Pfadart und RTT ab, bis die Verbindung schließt.
fn spawn_sampler(conn: Connection, start: Instant) -> Samples {
    let samples: Samples = Arc::default();
    let out = samples.clone();
    tokio::spawn(async move {
        loop {
            let (kind, rtt_ms) = current_path(&conn);
            if let Ok(mut v) = out.lock() {
                v.push(Sample {
                    t_ms: start.elapsed().as_millis() as u64,
                    kind,
                    rtt_ms,
                });
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                _ = conn.closed() => break,
            }
        }
    });
    samples
}

fn is_direct(k: Kind) -> bool {
    matches!(k, Kind::Direct4 | Kind::Direct6)
}

fn summarize(samples: &Samples) -> String {
    let v = samples.lock().map(|g| g.clone()).unwrap_or_default();
    let mut out = String::new();
    if v.is_empty() {
        return "  (keine Messpunkte)\n".into();
    }
    // Zeit bis direkt
    match v.iter().find(|s| is_direct(s.kind)) {
        Some(s) => out.push_str(&format!(
            "  Zeit bis direkte Verbindung: {:.1} s\n",
            s.t_ms as f64 / 1000.0
        )),
        None => out.push_str("  Zeit bis direkte Verbindung: NIE (nur Relay)\n"),
    }
    // Zeitverlauf (nur Wechsel)
    out.push_str("  Verlauf: ");
    let mut last = None;
    let mut parts = Vec::new();
    for s in &v {
        if last != Some(s.kind) {
            parts.push(format!("{:.1}s {}", s.t_ms as f64 / 1000.0, s.kind.label()));
            last = Some(s.kind);
        }
    }
    out.push_str(&parts.join(" -> "));
    out.push('\n');
    // Anteil und RTT je Pfadart
    for kind in [Kind::Direct4, Kind::Direct6, Kind::Relay] {
        let mut rtts: Vec<f64> = v
            .iter()
            .filter(|s| s.kind == kind && s.rtt_ms > 0.0)
            .map(|s| s.rtt_ms)
            .collect();
        let n = v.iter().filter(|s| s.kind == kind).count();
        if n == 0 {
            continue;
        }
        rtts.sort_by(|a, b| a.total_cmp(b));
        let pct = 100.0 * n as f64 / v.len() as f64;
        if rtts.is_empty() {
            out.push_str(&format!("  {}: {:.0} % der Zeit\n", kind.label(), pct));
        } else {
            out.push_str(&format!(
                "  {}: {:.0} % der Zeit, RTT min {:.1} / median {:.1} / max {:.1} ms\n",
                kind.label(),
                pct,
                rtts[0],
                rtts[rtts.len() / 2],
                rtts[rtts.len() - 1]
            ));
        }
    }
    out
}

async fn bind() -> Result<Endpoint> {
    let ep = Endpoint::builder(presets::N0)
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(RelayMode::Default)
        .bind()
        .await
        .context("Endpoint konnte nicht gestartet werden")?;
    Ok(ep)
}

fn mbit(bytes: u64, secs: f64) -> f64 {
    bytes as f64 * 8.0 / 1e6 / secs.max(0.001)
}

// ---------------------------------------------------------------- Server

async fn handle_conn(conn: Connection) {
    let remote = conn.remote_id();
    let start = Instant::now();
    let samples = spawn_sampler(conn.clone(), start);
    println!("\n>> Verbindung von {remote}");
    loop {
        let (mut send, mut recv) = match conn.accept_bi().await {
            Ok(s) => s,
            Err(_) => break,
        };
        let mut mode = [0u8; 1];
        if recv.read_exact(&mut mode).await.is_err() {
            break;
        }
        match mode[0] {
            b'U' => {
                // Client sendet, wir zählen und melden zurück.
                let t = Instant::now();
                let mut total = 0u64;
                let mut buf = vec![0u8; CHUNK];
                while let Ok(Some(n)) = recv.read(&mut buf).await {
                    total += n as u64;
                }
                let ms = t.elapsed().as_millis() as u64;
                let mut reply = [0u8; 16];
                reply[..8].copy_from_slice(&total.to_le_bytes());
                reply[8..].copy_from_slice(&ms.to_le_bytes());
                let _ = send.write_all(&reply).await;
                let _ = send.finish();
                // Stream am Leben lassen, bis der Client alles gelesen hat.
                let _ = send.stopped().await;
            }
            b'D' => {
                let buf = vec![0u8; CHUNK];
                let t = Instant::now();
                while t.elapsed() < Duration::from_secs(TEST_SECS) {
                    if send.write_all(&buf).await.is_err() {
                        break;
                    }
                }
                let _ = send.finish();
                let _ = send.stopped().await;
            }
            _ => break,
        }
    }
    let _ = conn.closed().await;
    println!(
        "<< Verbindung von {remote} beendet. Pfade (aus Sicht des Wartenden):\n{}",
        summarize(&samples)
    );
}

async fn run_wait() -> Result<()> {
    let ep = bind().await?;
    println!("Starte Endpoint ...");
    ep.online().await;
    println!("\n=== Warte auf Verbindungen ===");
    println!("Deine ID (an den Partner schicken):\n\n{}\n", ep.id());
    println!("Fenster offen lassen. Beenden mit Strg+C.");
    loop {
        tokio::select! {
            incoming = ep.accept() => {
                let Some(incoming) = incoming else { break };
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(conn) => handle_conn(conn).await,
                        Err(e) => println!("Eingehende Verbindung fehlgeschlagen: {e}"),
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    ep.close().await;
    Ok(())
}

// ---------------------------------------------------------------- Client

async fn run_connect(id: &str) -> Result<()> {
    let remote: EndpointId = id.trim().parse().context("Ungültige ID")?;
    let ep = bind().await?;
    println!("Starte Endpoint ...");
    ep.online().await;
    let my_id = ep.id();
    println!("Verbinde zu {remote} ...");
    let start = Instant::now();
    let conn = tokio::time::timeout(Duration::from_secs(30), ep.connect(remote, ALPN))
        .await
        .context("Zeitüberschreitung (30 s): Partner nicht erreichbar. Läuft dort 'wait'?")?
        .context("Verbindung fehlgeschlagen")?;
    let connect_ms = start.elapsed().as_millis();
    println!(
        "Verbunden nach {connect_ms} ms. Warte bis zu {} s auf direkten Pfad ...",
        DIRECT_WAIT.as_secs()
    );
    let samples = spawn_sampler(conn.clone(), start);

    let wait_start = Instant::now();
    while wait_start.elapsed() < DIRECT_WAIT {
        let (k, _) = current_path(&conn);
        if is_direct(k) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (k, _) = current_path(&conn);
    println!("Pfad vor Durchsatztest: {}", k.label());

    // Upload
    println!("Upload-Test ({TEST_SECS} s) ...");
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(b"U").await?;
    let buf = vec![0u8; CHUNK];
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(TEST_SECS) {
        send.write_all(&buf).await?;
    }
    send.finish()?;
    let mut reply = [0u8; 16];
    recv.read_exact(&mut reply)
        .await
        .context("Keine Antwort des Partners")?;
    let srv_bytes = u64::from_le_bytes(reply[..8].try_into()?);
    let srv_ms = u64::from_le_bytes(reply[8..].try_into()?);
    // Der Server misst vom ersten bis zum letzten Byte; das ist der echte Durchsatz.
    let up = mbit(srv_bytes, srv_ms as f64 / 1000.0);
    let (k_up, _) = current_path(&conn);

    // Download
    println!("Download-Test ({TEST_SECS} s) ...");
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(b"D").await?;
    send.finish()?;
    let t = Instant::now();
    let mut got = 0u64;
    let mut rbuf = vec![0u8; CHUNK];
    while let Some(n) = recv.read(&mut rbuf).await? {
        got += n as u64;
    }
    let down = mbit(got, t.elapsed().as_secs_f64());
    let (k_down, _) = current_path(&conn);

    conn.close(0u32.into(), b"done");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let block = format!(
        "----- NETTEST ERGEBNIS (bitte kopieren) -----\n\
         Ich:     {my_id}\n\
         Partner: {remote}\n\
         Verbindungsaufbau: {connect_ms} ms\n\
         {}\
         Upload   (ich -> Partner): {up:.1} Mbit/s  (Pfad: {})\n\
         Download (Partner -> ich): {down:.1} Mbit/s  (Pfad: {})\n\
         -----------------------------------------------",
        summarize(&samples),
        k_up.label(),
        k_down.label(),
    );
    println!("\n{block}");
    ep.close().await;
    Ok(())
}

// ---------------------------------------------------------------- main

fn pause() {
    print!("\nEnter drücken zum Beenden ...");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let interactive = cli.cmd.is_none();
    let res: Result<()> = async {
        match cli.cmd {
            Some(Cmd::Wait) => run_wait().await,
            Some(Cmd::Connect { id }) => run_connect(&id).await,
            None => {
                println!("Campfire Verbindungstest\n");
                print!("ID des Partners eingeben (leer lassen = ich warte auf den Partner): ");
                std::io::stdout().flush()?;
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                let line = line.trim();
                if line.is_empty() {
                    run_wait().await
                } else {
                    run_connect(line).await
                }
            }
        }
    }
    .await;
    if let Err(e) = res {
        println!("\nFEHLER: {e:#}");
        if interactive {
            pause();
        }
        std::process::exit(1);
    }
    if interactive {
        pause();
    }
}

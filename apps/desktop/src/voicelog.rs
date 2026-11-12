//! Sprach-Messprotokoll: schreibt waehrend eines Anrufs jede Sekunde die Empfangswerte, den Netzpfad
//! und die Audio-Thread-Messwerte in eine CSV-Datei, damit man echte Gespraeche mit Freunden
//! auswerten kann (lokal tritt das Stottern nicht auf).
//!
//! Pro Anruf eine Datei `voice-<unix-sekunden>.csv` in `<Logordner>\voice\`, dazu am Ende
//! `voice-<unix-sekunden>.summary.txt`. Es werden nur Zaehler und Zeiten geschrieben, keine
//! Audiodaten und keine vollstaendigen Schluessel (nur 3 Byte Hex je Teilnehmer).

use crate::core::Core;
use app_audio::RxSummary;
use app_net::PathKind;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const HEADER: &str =
    "t_s,peer,path,rtt_ms,received,played,late,plc,fec,expand,accel,rebuffer,stall,\
jitter_ms,target_ms,depth_ms,d_received,d_late,d_plc,d_expand,d_stall,out_ms,underruns,\
d_underruns,late_ticks,d_late_ticks,capture_burst,dgram_dropped,d_dgram_dropped";

pub fn dir() -> PathBuf {
    crate::diag::log_dir().join("voice")
}

/// Eine Messzeile: Zaehler (seit Anrufbeginn) plus Aenderung seit der Zeile davor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    pub t_s: f64,
    pub peer: String,
    pub path: &'static str,
    pub rtt_ms: Option<f64>,
    pub rx: RxSummary,
    pub d_received: u64,
    pub d_late: u64,
    pub d_plc: u64,
    pub d_expand: u64,
    pub d_stall: u64,
    pub out_ms: u32,
    pub underruns: u32,
    pub d_underruns: u32,
    pub late_ticks: u32,
    pub d_late_ticks: u32,
    pub capture_burst: u32,
    pub dgram_dropped: u64,
    pub d_dgram_dropped: u64,
}

impl Row {
    fn csv(&self) -> String {
        let r = &self.rx;
        let rtt = self.rtt_ms.map_or(String::new(), |v| format!("{v:.1}"));
        format!(
            "{:.1},{},{},{},{},{},{},{},{},{},{},{},{},{:.1},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            self.t_s,
            self.peer,
            self.path,
            rtt,
            r.received,
            r.played,
            r.late,
            r.plc,
            r.fec,
            r.expand,
            r.accel,
            r.rebuffer,
            r.stall,
            r.jitter_ms,
            r.target_ms,
            r.depth_ms,
            self.d_received,
            self.d_late,
            self.d_plc,
            self.d_expand,
            self.d_stall,
            self.out_ms,
            self.underruns,
            self.d_underruns,
            self.late_ticks,
            self.d_late_ticks,
            self.capture_burst,
            self.dgram_dropped,
            self.d_dgram_dropped,
        )
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i]
}

/// Auswertung einer Anrufdatei: wo und wie oft hat es gehakt?
pub fn summarize(rows: &[Row]) -> String {
    let mut out = String::new();
    let mut peers: Vec<&str> = rows.iter().map(|r| r.peer.as_str()).collect();
    peers.sort_unstable();
    peers.dedup();
    for peer in peers {
        let rs: Vec<&Row> = rows.iter().filter(|r| r.peer == peer).collect();
        let Some(last) = rs.last() else { continue };
        let secs = rs.len();
        // Sekunden, in denen nichts ankam, obwohl der Sprecher davor gesendet hat, zaehlen als
        // Netzluecke; Sekunden mit Ersatz-/Wartezaehlern als hoerbar gestoert.
        let bad = rs
            .iter()
            .filter(|r| r.d_plc > 0 || r.d_expand > 0 || r.d_stall > 0 || r.d_late > 0)
            .count();
        let silent = rs.iter().filter(|r| r.d_received == 0).count();
        let mut jit: Vec<f64> = rs.iter().map(|r| r.rx.jitter_ms).collect();
        jit.sort_by(f64::total_cmp);
        let mut rtt: Vec<f64> = rs.iter().filter_map(|r| r.rtt_ms).collect();
        rtt.sort_by(f64::total_cmp);
        let mut paths: HashMap<&str, usize> = HashMap::new();
        for r in &rs {
            *paths.entry(r.path).or_default() += 1;
        }
        let mut paths: Vec<_> = paths.into_iter().collect();
        paths.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        let worst = rs.iter().max_by_key(|r| r.d_plc + r.d_expand + r.d_late);
        let _ = writeln!(out, "Teilnehmer {peer}: {secs} Messungen (je 1 s)");
        let _ = writeln!(
            out,
            "  Pfad: {}",
            paths
                .iter()
                .map(|(p, n)| format!("{p} {n} s"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let _ = writeln!(
            out,
            "  RTT ms: min {:.0} · Median {:.0} · p95 {:.0} · max {:.0}",
            percentile(&rtt, 0.0),
            percentile(&rtt, 0.5),
            percentile(&rtt, 0.95),
            percentile(&rtt, 1.0)
        );
        let _ = writeln!(
            out,
            "  Jitter ms: Median {:.0} · p95 {:.0} · max {:.0}",
            percentile(&jit, 0.5),
            percentile(&jit, 0.95),
            percentile(&jit, 1.0)
        );
        let _ = writeln!(
            out,
            "  Pakete: {} empfangen · Verlust {:.1} % · {} zu spät · {} PLC · {} FEC · {} gewartet · {} Aussetzer · {} Pausen",
            last.rx.received,
            last.rx.loss_percent(),
            last.rx.late,
            last.rx.plc,
            last.rx.fec,
            last.rx.expand,
            last.rx.stall,
            last.rx.rebuffer.saturating_sub(last.rx.stall)
        );
        let _ = writeln!(
            out,
            "  Gestörte Sekunden: {bad} von {secs} ({:.0} %) · Sekunden ohne Pakete: {silent}",
            bad as f64 * 100.0 / secs.max(1) as f64
        );
        let _ = writeln!(
            out,
            "  Eigene Seite: {} Lautsprecher-Aussetzer · {} späte Audio-Thread-Ticks · Mikro-Schub max {} · {} Datagramme verworfen",
            last.underruns, last.late_ticks, rs.iter().map(|r| r.capture_burst).max().unwrap_or(0), last.dgram_dropped
        );
        if let Some(w) = worst.filter(|w| w.d_plc + w.d_expand + w.d_late > 0) {
            let _ = writeln!(
                out,
                "  Schlimmste Sekunde: t={:.0} s (PLC {}, gewartet {}, spät {}, Pfad {})",
                w.t_s, w.d_plc, w.d_expand, w.d_late, w.path
            );
        }
    }
    out
}

fn path_name(k: PathKind) -> &'static str {
    match k {
        PathKind::Direct => "direct",
        PathKind::Relay => "relay",
        PathKind::None => "none",
    }
}

/// Eine Aufnahme (ein Anruf): offene Datei plus alle Zeilen fuer die Zusammenfassung.
struct Session {
    start: Instant,
    csv: std::fs::File,
    summary_path: PathBuf,
    rows: Vec<Row>,
    prev: HashMap<String, Row>,
    prev_audio: (u32, u32),
    prev_dropped: u64,
}

impl Session {
    fn open() -> Option<Self> {
        let dir = dir();
        std::fs::create_dir_all(&dir).ok()?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut csv = std::fs::File::create(dir.join(format!("voice-{stamp}.csv"))).ok()?;
        writeln!(csv, "{HEADER}").ok()?;
        tracing::info!("Sprach-Messprotokoll: voice-{stamp}.csv");
        Some(Self {
            start: Instant::now(),
            csv,
            summary_path: dir.join(format!("voice-{stamp}.summary.txt")),
            rows: Vec::new(),
            prev: HashMap::new(),
            prev_audio: (0, 0),
            prev_dropped: 0,
        })
    }

    fn sample(&mut self, core: &Core) {
        let stats = core
            .voice
            .core()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rx_stats();
        let diag = core.audio_diag().unwrap_or_default();
        let dropped = core.net.datagrams_dropped();
        let t_s = self.start.elapsed().as_secs_f64();
        let (d_under, d_ticks) = (
            diag.underruns.saturating_sub(self.prev_audio.0),
            diag.late_ticks.saturating_sub(self.prev_audio.1),
        );
        let d_dropped = dropped.saturating_sub(self.prev_dropped);
        self.prev_audio = (diag.underruns, diag.late_ticks);
        self.prev_dropped = dropped;
        for (dev, rx) in stats {
            let peer: String = dev.iter().take(3).map(|b| format!("{b:02x}")).collect();
            let (kind, rtt) = core.net.path_info(&dev);
            let p = self.prev.get(&peer).map(|r| r.rx).unwrap_or_default();
            let row = Row {
                t_s,
                peer: peer.clone(),
                path: path_name(kind),
                rtt_ms: rtt.map(|d| d.as_secs_f64() * 1000.0),
                rx,
                d_received: rx.received.saturating_sub(p.received),
                d_late: rx.late.saturating_sub(p.late),
                d_plc: rx.plc.saturating_sub(p.plc),
                d_expand: rx.expand.saturating_sub(p.expand),
                d_stall: rx.stall.saturating_sub(p.stall),
                out_ms: diag.out_ms,
                underruns: diag.underruns,
                d_underruns: d_under,
                late_ticks: diag.late_ticks,
                d_late_ticks: d_ticks,
                capture_burst: diag.capture_burst,
                dgram_dropped: dropped,
                d_dgram_dropped: d_dropped,
            };
            let _ = writeln!(self.csv, "{}", row.csv());
            self.prev.insert(peer, row.clone());
            self.rows.push(row);
        }
        let _ = self.csv.flush();
    }

    fn finish(self) {
        if self.rows.is_empty() {
            return;
        }
        let text = summarize(&self.rows);
        tracing::info!("Sprach-Zusammenfassung:\n{text}");
        let _ = std::fs::write(&self.summary_path, text);
    }
}

/// Startet den Hintergrund-Task: pro Anruf eine Datei, 1 Messung pro Sekunde.
pub fn spawn(core: Core) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut session: Option<Session> = None;
        loop {
            tick.tick().await;
            let in_voice = core.voice.current().is_some();
            match (&mut session, in_voice) {
                (None, true) => session = Session::open(),
                (Some(s), true) => s.sample(&core),
                (Some(_), false) => {
                    if let Some(s) = session.take() {
                        s.finish();
                    }
                }
                (None, false) => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(t: f64, recv: u64, plc: u64, d_plc: u64) -> Row {
        Row {
            t_s: t,
            peer: "aabbcc".into(),
            path: "relay",
            rtt_ms: Some(40.0),
            rx: RxSummary {
                received: recv,
                played: recv,
                plc,
                jitter_ms: 8.0,
                ..Default::default()
            },
            d_received: 50,
            d_plc,
            ..Default::default()
        }
    }

    #[test]
    fn csv_has_one_field_per_header_column() {
        let cols = HEADER.split(',').count();
        assert_eq!(row(1.0, 50, 0, 0).csv().split(',').count(), cols);
    }

    #[test]
    fn summary_counts_disturbed_seconds() {
        let rows = vec![row(1.0, 50, 0, 0), row(2.0, 100, 3, 3), row(3.0, 150, 3, 0)];
        let s = summarize(&rows);
        assert!(s.contains("Gestörte Sekunden: 1 von 3"), "{s}");
        assert!(s.contains("Schlimmste Sekunde: t=2"), "{s}");
        assert!(s.contains("relay 3 s"), "{s}");
    }
}

//! Prozess-Loopback-Demo (M0-09).
//!   loopback_demo exclude-self [sek] [datei]   alles außer dieser Demo (Modus A)
//!   loopback_demo exclude <pid> [sek] [datei]  alles außer <pid>-Baum
//!   loopback_demo include <pid> [sek] [datei]  nur <pid>-Baum (Modus B)
use app_video::loopback::{capture_to_wav, Mode};
use std::path::PathBuf;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let pid_arg = |i: usize| a.get(i).and_then(|s| s.parse::<u32>().ok());
    let (mode, rest) = match (a.first().map(String::as_str), pid_arg(1)) {
        (Some("exclude-self"), _) => (Mode::ExcludeTree(std::process::id()), &a[1..]),
        (Some("exclude"), Some(p)) => (Mode::ExcludeTree(p), &a[2..]),
        (Some("include"), Some(p)) => (Mode::IncludeTree(p), &a[2..]),
        _ => {
            println!(
                "Aufruf: loopback_demo exclude-self|exclude <pid>|include <pid> [sek] [datei.wav]"
            );
            return;
        }
    };
    let secs: u64 = rest.first().and_then(|s| s.parse().ok()).unwrap_or(5);
    let path = PathBuf::from(
        rest.get(1)
            .cloned()
            .unwrap_or_else(|| "loopback.wav".into()),
    );
    println!("Modus {mode:?}, {secs} s -> {}", path.display());
    match capture_to_wav(mode, secs, &path) {
        Ok(s) => println!(
            "OK: {} Frames, Pakete {} (davon still {}), Peak {:.4}, RMS {:.5} => {}",
            s.frames,
            s.total_packets,
            s.silent_packets,
            s.peak,
            s.rms,
            if s.peak > 0.001 {
                "TON AUFGENOMMEN"
            } else {
                "STILLE"
            }
        ),
        Err(e) => println!("FEHLER: {e}"),
    }
}

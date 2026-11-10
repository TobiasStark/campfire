//! Logging und Diagnose (M1-19): rotierende Logdateien, Crash-Reports, Diagnose-Export.
//!
//! Der Export enthaelt Version, System, Updatestand und die letzten Logzeilen. Schluessel und
//! Nachrichteninhalte werden nie geloggt; zusaetzlich filtert `redact` lange Zeichenketten heraus.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};

static GUARD: OnceLock<WorkerGuard> = OnceLock::new();
/// Hoechstens so viele Crash-Dateien bleiben liegen (die neuesten) (I-10).
const MAX_CRASH_FILES: usize = 10;

pub fn log_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("TEMP"))
        .unwrap_or_default();
    PathBuf::from(base).join("Campfire").join("logs")
}

/// Richtet das Logging ein (7 Tagesdateien) und den Panic-Handler.
pub fn init() {
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(app) = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("campfire")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
    {
        let (w, guard) = tracing_appender::non_blocking(app);
        let _ = GUARD.set(guard);
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info,iroh=warn,quinn=warn".into());
        let _ = tracing_subscriber::fmt()
            .with_writer(w)
            .with_ansi(false)
            .with_env_filter(filter)
            .try_init();
    }
    prune_crashes(&dir, MAX_CRASH_FILES);
    install_panic_hook(dir);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Campfire gestartet");
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn crash_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("crash-"))
                })
                .collect()
        })
        .unwrap_or_default();
    // Name enthaelt die Unix-Zeit: numerisch sortieren, aelteste zuerst.
    v.sort_by_key(|p| {
        p.file_stem()
            .and_then(|n| n.to_str())
            .and_then(|n| n.trim_start_matches("crash-").parse::<u64>().ok())
            .unwrap_or(0)
    });
    v
}

/// Loescht alle bis auf die `keep` neuesten Crash-Dateien.
fn prune_crashes(dir: &Path, keep: usize) {
    let files = crash_files(dir);
    let n = files.len().saturating_sub(keep);
    for f in &files[..n] {
        let _ = std::fs::remove_file(f);
    }
}

fn install_panic_hook(dir: PathBuf) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let report = crash_report(info);
        tracing::error!("Absturz: {}", report.lines().next().unwrap_or(""));
        let _ = std::fs::write(dir.join(format!("crash-{}.txt", now_secs())), &report);
        prev(info);
    }));
}

fn crash_report(info: &std::panic::PanicHookInfo<'_>) -> String {
    let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "(no message)".into()
    };
    let at = info
        .location()
        .map(|l| format!("{}:{}", l.file(), l.line()))
        .unwrap_or_default();
    format!(
        "Panic: {msg}\nOrt: {at}\nThread: {}\nVersion: {}\nZeit (Unix): {}\n\nBacktrace:\n{}\n",
        std::thread::current().name().unwrap_or("?"),
        env!("CARGO_PKG_VERSION"),
        now_secs(),
        std::backtrace::Backtrace::force_capture()
    )
}

/// Ersetzt lange schluesselartige Zeichenketten (Base32/Base64/Hex ab 40 Zeichen),
/// age-Geheimschluessel, IP-Adressen und den Benutzerordner (enthaelt den Kontonamen).
pub fn redact(text: &str) -> String {
    let mut s = redact_ips(&redact_keys(text));
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let home = home.to_string_lossy();
        if home.len() > 3 {
            s = s.replace(home.as_ref(), "%USERPROFILE%");
        }
    }
    s
}

/// Ersetzt IPv4- und IPv6-Adressen durch `[IP]` (I-10). Uhrzeiten ("12:30:45") bleiben, weil
/// IPv6 mindestens drei Doppelpunkte oder "::" braucht.
fn redact_ips(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut tok = String::new();
    let flush = |tok: &mut String, out: &mut String| {
        if is_ip_like(tok) {
            out.push_str("[IP]");
        } else {
            out.push_str(tok);
        }
        tok.clear();
    };
    for ch in text.chars() {
        if ch.is_ascii_hexdigit() || matches!(ch, '.' | ':') {
            tok.push(ch);
        } else {
            flush(&mut tok, &mut out);
            out.push(ch);
        }
    }
    flush(&mut tok, &mut out);
    out
}

fn is_ip_like(tok: &str) -> bool {
    let t = tok.trim_end_matches(['.', ':']);
    // IPv4, optional mit Port.
    let host = match t.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') && p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => t,
    };
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    let colons = t.matches(':').count();
    (colons >= 3 || t.contains("::")) && t.chars().any(|c| c.is_ascii_hexdigit())
}

fn redact_keys(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut tok = String::new();
    let flush = |tok: &mut String, out: &mut String| {
        let secret = tok.len() >= 40 || tok.to_ascii_uppercase().starts_with("AGE-SECRET-KEY");
        if secret {
            out.push_str("[entfernt]");
        } else {
            out.push_str(tok);
        }
        tok.clear();
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '=' | '_' | '-') {
            tok.push(ch);
        } else {
            flush(&mut tok, &mut out);
            out.push(ch);
        }
    }
    flush(&mut tok, &mut out);
    out
}

fn tail(path: &Path, max: usize) -> String {
    let Ok(data) = std::fs::read(path) else {
        return String::new();
    };
    let start = data.len().saturating_sub(max);
    String::from_utf8_lossy(&data[start..]).into_owned()
}

/// Baut den Diagnosetext aus Logordner und Zusatzangaben.
pub fn build_report(dir: &Path, extra: &str) -> String {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    files.sort();
    let mut out = format!(
        "Campfire Diagnose\nVersion: {}\nSystem: {} {}\nZeit (Unix): {}\n{extra}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        now_secs()
    );
    let crashes = crash_files(dir);
    for f in crashes.iter().rev().take(3) {
        out.push_str(&format!("\n=== {} ===\n", f.display()));
        out.push_str(&tail(f, 20_000));
    }
    if let Some(f) = files.iter().rev().find(|f| {
        f.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("campfire"))
    }) {
        out.push_str(&format!("\n=== {} (Ende) ===\n", f.display()));
        out.push_str(&tail(f, 100_000));
    }
    redact(&out)
}

/// Schreibt die Diagnose nach Dokumente (neue Datei, nichts wird ueberschrieben) und gibt
/// den Pfad zurueck. Die Oberflaeche oeffnet sie danach zur Durchsicht vor dem Teilen.
pub fn export(extra: &str) -> std::io::Result<PathBuf> {
    use std::io::Write;
    let home = std::env::var_os("USERPROFILE").unwrap_or_default();
    let dir = PathBuf::from(home).join("Documents");
    let report = build_report(&log_dir(), extra);
    for n in 0..1000u32 {
        let name = if n == 0 {
            format!("Campfire-Diagnose-{}.txt", now_secs())
        } else {
            format!("Campfire-Diagnose-{}-{n}.txt", now_secs())
        };
        let path = dir.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(report.as_bytes())?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free file name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_contains_no_keys_but_keeps_useful_text() {
        let dir = std::env::temp_dir().join(format!("campfire-diag-{}", now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        let secret = "AGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ";
        let node = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
        std::fs::write(
            dir.join("campfire.2026-01-01.log"),
            format!(
                "INFO verbunden mit Freund\nWARN Schluessel {secret} gesehen\nINFO id {node}\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("crash-1.txt"), "Panic: kaputt\nOrt: a.rs:1\n").unwrap();
        let r = build_report(&dir, "Relay: Standard");
        assert!(r.contains("verbunden mit Freund"));
        assert!(r.contains("Panic: kaputt"));
        assert!(r.contains("Relay: Standard"));
        assert!(!r.contains("SECRET"), "{r}");
        assert!(!r.contains(node));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn panic_writes_crash_report_with_backtrace() {
        let dir = std::env::temp_dir().join(format!("campfire-crash-{}", now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        let before = std::panic::take_hook();
        install_panic_hook(dir.clone());
        let _ = std::panic::catch_unwind(|| panic!("absichtlicher Testabsturz"));
        let _ = std::panic::take_hook();
        std::panic::set_hook(before);
        let f = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().starts_with("crash-"))
            .expect("kein Crash-Report");
        let t = std::fs::read_to_string(f.path()).unwrap();
        assert!(t.contains("absichtlicher Testabsturz") && t.contains("Backtrace:"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ip_addresses_are_hidden_but_times_and_versions_stay() {
        let r = redact(
            "12:30:45 verbunden 192.168.1.20:4433 und [2001:db8::1]:443 fe80::1 v0.6.2 a.rs:12",
        );
        assert!(!r.contains("192.168"), "{r}");
        assert!(!r.contains("2001:db8"), "{r}");
        assert!(!r.contains("fe80"), "{r}");
        assert!(
            r.contains("12:30:45") && r.contains("v0.6.2") && r.contains("a.rs:12"),
            "{r}"
        );
    }

    #[test]
    fn only_the_newest_crash_files_are_kept() {
        let dir = std::env::temp_dir().join(format!("campfire-prune-{}", now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        for i in [5u64, 100, 20, 3] {
            std::fs::write(dir.join(format!("crash-{i}.txt")), "x").unwrap();
        }
        prune_crashes(&dir, 2);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["crash-100.txt", "crash-20.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redact_keeps_short_words_and_paths() {
        assert_eq!(
            redact("Fehler bei C:\\a\\b.rs:12"),
            "Fehler bei C:\\a\\b.rs:12"
        );
    }
}

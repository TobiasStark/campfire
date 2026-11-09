//! Logging und Diagnose (M1-19): rotierende Logdateien, Crash-Reports, Diagnose-Export.
//!
//! Der Export enthaelt Version, System, Updatestand und die letzten Logzeilen. Schluessel und
//! Nachrichteninhalte werden nie geloggt; zusaetzlich filtert `redact` lange Zeichenketten heraus.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};

static GUARD: OnceLock<WorkerGuard> = OnceLock::new();

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
    install_panic_hook(dir);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Campfire gestartet");
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
        "(keine Meldung)".into()
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

/// Ersetzt lange schluesselartige Zeichenketten (Base32/Base64/Hex ab 40 Zeichen) und
/// age-Geheimschluessel durch `[entfernt]`.
pub fn redact(text: &str) -> String {
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
    for f in files.iter().filter(|f| {
        f.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("crash-"))
    }) {
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

/// Schreibt die Diagnose nach Dokumente und gibt den Pfad zurueck.
pub fn export(extra: &str) -> std::io::Result<PathBuf> {
    let home = std::env::var_os("USERPROFILE").unwrap_or_default();
    let path = PathBuf::from(home)
        .join("Documents")
        .join("Campfire-Diagnose.txt");
    std::fs::write(&path, build_report(&log_dir(), extra))?;
    Ok(path)
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
    fn redact_keeps_short_words_and_paths() {
        assert_eq!(
            redact("Fehler bei C:\\a\\b.rs:12"),
            "Fehler bei C:\\a\\b.rs:12"
        );
    }
}

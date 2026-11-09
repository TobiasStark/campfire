//! Auto-Update ueber Velopack (M1-18).
//!
//! Der Check laeuft in einem eigenen Thread und blockiert den Start nie. Ein gefundenes Update wird
//! heruntergeladen und beim naechsten Start eingespielt (Velopack macht das in `VelopackApp::run`).
//! Ohne Installation (z. B. `cargo run`) passiert nichts.

use std::sync::Mutex;
use std::time::Duration;
use velopack::{sources::HttpSource, UpdateCheck, UpdateManager, VelopackApp};

/// Quelle der Releases: Ordner mit `releases.<kanal>.json` (GitHub-Release-Downloads).
/// Per Umgebungsvariable `CAMPFIRE_UPDATE_URL` ueberschreibbar.
const DEFAULT_URL: &str = "https://github.com/TobiasStark/campfire/releases/latest/download";

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Nicht installiert (Entwicklungsstart): kein Update moeglich.
    NotInstalled,
    Checking,
    UpToDate,
    Downloading(String),
    /// Heruntergeladen, wird beim naechsten Start installiert.
    Ready(String),
    Failed(String),
}

static STATUS: Mutex<Status> = Mutex::new(Status::NotInstalled);

pub fn status() -> Status {
    STATUS
        .lock()
        .map(|s| s.clone())
        .unwrap_or(Status::NotInstalled)
}

fn set(s: Status) {
    if let Ok(mut g) = STATUS.lock() {
        *g = s;
    }
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Muss als Erstes in `main` laufen: behandelt Installer-/Update-Aufrufe und wendet
/// ein bereitgestelltes Update an.
pub fn init() {
    VelopackApp::build().run();
}

pub fn spawn_check() {
    let _ = std::thread::Builder::new()
        .name("updater".into())
        .spawn(|| {
            // Erst die Oberflaeche starten lassen.
            std::thread::sleep(Duration::from_secs(10));
            loop {
                check_once();
                std::thread::sleep(Duration::from_secs(6 * 3600));
            }
        });
}

fn check_once() {
    let url = std::env::var("CAMPFIRE_UPDATE_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let Ok(um) = UpdateManager::new(HttpSource::new(url), None, None) else {
        set(Status::NotInstalled);
        return;
    };
    if let Some(v) = um.get_update_pending_restart() {
        set(Status::Ready(v.Version));
        return;
    }
    set(Status::Checking);
    match um.check_for_updates() {
        Ok(UpdateCheck::UpdateAvailable(info)) => {
            let v = info.TargetFullRelease.Version.clone();
            set(Status::Downloading(v.clone()));
            match um.download_updates(&info, None) {
                Ok(()) => set(Status::Ready(v)),
                Err(e) => set(Status::Failed(e.to_string())),
            }
        }
        Ok(_) => set(Status::UpToDate),
        Err(e) => set(Status::Failed(e.to_string())),
    }
}

pub fn describe() -> String {
    let v = version();
    match status() {
        Status::NotInstalled => app_core::tr!(
            "Version {v} (development build, no updates)",
            "Version {v} (Entwicklungsstart, keine Updates)"
        ),
        Status::Checking => app_core::tr!(
            "Version {v}: checking for updates ...",
            "Version {v}: suche Updates ..."
        ),
        Status::UpToDate => app_core::tr!("Version {v}: up to date", "Version {v}: aktuell"),
        Status::Downloading(n) => app_core::tr!(
            "Version {v}: downloading {n} ...",
            "Version {v}: lade {n} herunter ..."
        ),
        Status::Ready(n) => app_core::tr!(
            "Version {v}: {n} will be installed on next start",
            "Version {v}: {n} wird beim nächsten Start installiert"
        ),
        Status::Failed(e) => app_core::tr!(
            "Version {v}: update check failed ({e})",
            "Version {v}: Update-Suche fehlgeschlagen ({e})"
        ),
    }
}

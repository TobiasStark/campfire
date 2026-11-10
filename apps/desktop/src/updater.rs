//! Updates ueber Velopack (M1-18), nur mit Zustimmung (H-2).
//!
//! Die App sucht im Hintergrund nach Updates, laedt und installiert aber nichts von selbst:
//! Ein gefundenes Update wird in den Einstellungen angeboten. Erst ein Klick auf
//! "Herunterladen" laedt es, erst "Neu starten und installieren" spielt es ein. Auch beim Start
//! wird kein bereitliegendes Update automatisch angewendet. Ohne Installation (z. B. `cargo run`)
//! passiert nichts.

use app_proto::lock::Locked;
use std::sync::Mutex;
use std::time::Duration;
use velopack::{sources::HttpSource, UpdateCheck, UpdateInfo, UpdateManager, VelopackApp};

/// Quelle der Releases: Ordner mit `releases.<kanal>.json` (GitHub-Release-Downloads).
const DEFAULT_URL: &str = "https://github.com/TobiasStark/campfire/releases/latest/download";

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Nicht installiert (Entwicklungsstart): kein Update moeglich.
    NotInstalled,
    Checking,
    UpToDate,
    /// Gefunden, wartet auf Zustimmung zum Herunterladen.
    Available(String),
    Downloading(String),
    /// Heruntergeladen, wartet auf Zustimmung zum Installieren.
    Ready(String),
    Failed(String),
}

static STATUS: Mutex<Status> = Mutex::new(Status::NotInstalled);
/// Das zuletzt gefundene Update (fuer "Herunterladen").
static FOUND: Mutex<Option<UpdateInfo>> = Mutex::new(None);

pub fn status() -> Status {
    STATUS.locked().clone()
}

fn set(s: Status) {
    *STATUS.locked() = s;
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Muss als Erstes in `main` laufen: behandelt Installer-Aufrufe. Ein bereitliegendes Update
/// wird **nicht** automatisch angewendet.
pub fn init() {
    VelopackApp::build().set_auto_apply_on_startup(false).run();
}

/// Quelle der Updates. Nur Entwicklungs-Builds lassen sie per `CAMPFIRE_UPDATE_URL` umlenken
/// (N-12); in Releases ist sie fest.
fn source_url() -> String {
    #[cfg(debug_assertions)]
    if let Ok(u) = std::env::var("CAMPFIRE_UPDATE_URL") {
        return u;
    }
    DEFAULT_URL.to_string()
}

fn manager() -> Option<UpdateManager> {
    UpdateManager::new(HttpSource::new(source_url()), None, None).ok()
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
    if matches!(status(), Status::Downloading(_) | Status::Ready(_)) {
        return;
    }
    let Some(um) = manager() else {
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
            *FOUND.locked() = Some(*info);
            set(Status::Available(v));
        }
        Ok(_) => set(Status::UpToDate),
        Err(e) => set(Status::Failed(e.to_string())),
    }
}

/// Nach Zustimmung: das gefundene Update herunterladen (im Hintergrund).
pub fn download() {
    let Some(info) = FOUND.locked().clone() else {
        return;
    };
    let v = info.TargetFullRelease.Version.clone();
    set(Status::Downloading(v.clone()));
    let _ = std::thread::Builder::new()
        .name("update-download".into())
        .spawn(move || {
            let r = manager()
                .ok_or_else(|| "not installed".to_owned())
                .and_then(|um| um.download_updates(&info, None).map_err(|e| e.to_string()));
            set(match r {
                Ok(()) => Status::Ready(v),
                Err(e) => Status::Failed(e),
            });
            crate::signals::send(crate::signals::Signal::UpdateChanged);
        });
}

/// Nach Zustimmung: Update nach dem Beenden einspielen und neu starten. `true`, wenn die App
/// sich jetzt beenden soll.
pub fn install_and_restart() -> bool {
    let Some(um) = manager() else {
        return false;
    };
    let Some(asset) = um.get_update_pending_restart() else {
        return false;
    };
    match um.wait_exit_then_apply_updates(&asset, false, true, Vec::<String>::new()) {
        Ok(()) => true,
        Err(e) => {
            set(Status::Failed(e.to_string()));
            false
        }
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
        Status::Available(n) => app_core::tr!(
            "Version {v}: version {n} is available",
            "Version {v}: Version {n} ist verfügbar"
        ),
        Status::Downloading(n) => app_core::tr!(
            "Version {v}: downloading {n} ...",
            "Version {v}: lade {n} herunter ..."
        ),
        Status::Ready(n) => app_core::tr!(
            "Version {v}: {n} is downloaded and will be installed when you confirm",
            "Version {v}: {n} ist heruntergeladen und wird installiert, sobald du zustimmst"
        ),
        Status::Failed(e) => app_core::tr!(
            "Version {v}: update check failed ({e})",
            "Version {v}: Update-Suche fehlgeschlagen ({e})"
        ),
    }
}

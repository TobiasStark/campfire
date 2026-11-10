//! Lokaler Speicher: SQLite (WAL) auf einem eigenen DB-Thread.
//!
//! Die Connection lebt ausschliesslich auf dem DB-Thread. Aufrufer geben
//! Closures ab (`call` blockierend fuer Worker-Threads, `spawn` fuer
//! Fire-and-forget mit Callback). Der UI-Thread ruft
//! [`forbid_on_current_thread`] auf; danach lehnt `call` dort ab, nur `spawn` geht.

mod schema;

use rusqlite::Connection;
use std::cell::Cell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;

pub use schema::migrations;

pub const APP_DIR_NAME: &str = "Campfire";
/// Ordner der Datenbank unter `%LOCALAPPDATA%`. Nicht `Campfire`: den loescht der Installer
/// beim Deinstallieren samt Inhalt, und die Datenbank enthaelt die Identitaet.
pub const DATA_DIR_NAME: &str = "CampfireData";
const DB_FILE: &str = "app.db";

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Migration: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("DB thread has ended")]
    Closed,
    #[error("Blocking DB access from this thread (UI) is forbidden; use spawn")]
    ForbiddenThread,
    #[error("Environment variable {0} is missing")]
    MissingEnv(&'static str),
}

pub type Result<T> = std::result::Result<T, StorageError>;

thread_local! {
    static FORBIDDEN: Cell<bool> = const { Cell::new(false) };
}

/// Markiert den aktuellen Thread (UI-Thread) als tabu fuer blockierende DB-Aufrufe.
pub fn forbid_on_current_thread() {
    FORBIDDEN.with(|f| f.set(true));
}

/// Speicherorte.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `%LOCALAPPDATA%\CampfireData` – Datenbank. Nicht im Roaming-Profil (N-8): sonst wandert
    /// sie mit servergespeicherten Profilen auf fremde Rechner.
    pub data_dir: PathBuf,
    /// `%LOCALAPPDATA%\Campfire\blobs`.
    pub blobs_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        let local =
            std::env::var_os("LOCALAPPDATA").ok_or(StorageError::MissingEnv("LOCALAPPDATA"))?;
        Ok(Self {
            data_dir: Path::new(&local).join(DATA_DIR_NAME),
            blobs_dir: Path::new(&local).join(APP_DIR_NAME).join("blobs"),
        })
    }

    /// Bis 0.6 lag die Datenbank in `%APPDATA%\Campfire` (Roaming). Verschiebt sie einmalig an
    /// den neuen Ort (N-8); ist dort schon eine, bleibt alles, wie es ist.
    pub fn migrate_from_roaming(&self) -> Result<()> {
        match std::env::var_os("APPDATA") {
            Some(roaming) => move_db(&Path::new(&roaming).join(APP_DIR_NAME), &self.data_dir),
            None => Ok(()),
        }
    }

    /// Alles unter einem Wurzelverzeichnis (Tests, portable Nutzung).
    pub fn in_dir(root: &Path) -> Self {
        Self {
            data_dir: root.join("data"),
            blobs_dir: root.join("blobs"),
        }
    }

    pub fn db_file(&self) -> PathBuf {
        self.data_dir.join(DB_FILE)
    }

    pub fn create_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(&self.blobs_dir)?;
        Ok(())
    }
}

/// Verschiebt `app.db` samt WAL von `from` nach `to`. Erst kopieren, die Datenbank zuletzt
/// unter einem Zwischennamen; erst wenn alles angekommen ist, wird sie umbenannt und das
/// Original geloescht. Bricht es ab, wird beim naechsten Start neu versucht.
fn move_db(from: &Path, to: &Path) -> Result<()> {
    let (old, new) = (from.join(DB_FILE), to.join(DB_FILE));
    if !old.is_file() || new.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    for suffix in ["-wal", "-shm"] {
        let f = from.join(format!("{DB_FILE}{suffix}"));
        if f.is_file() {
            std::fs::copy(&f, to.join(format!("{DB_FILE}{suffix}")))?;
        }
    }
    let part = to.join(format!("{DB_FILE}.part"));
    std::fs::copy(&old, &part)?;
    std::fs::rename(&part, &new)?;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(from.join(format!("{DB_FILE}{suffix}")));
    }
    // Nur leere Ordner; anderes bleibt unangetastet.
    let _ = std::fs::remove_dir(from);
    Ok(())
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// Zwei Warteschlangen: eigene Aktionen (UI) vor Arbeit, die das Netz ausloest. So kann ein
/// Peer, der viel Arbeit erzeugt, die eigene Bedienung nicht aufhalten.
#[derive(Default)]
struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

#[derive(Default)]
struct QueueState {
    high: VecDeque<Job>,
    low: VecDeque<Job>,
    closed: bool,
}

impl Queue {
    fn push(&self, job: Job, low: bool) -> Result<()> {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if s.closed {
            return Err(StorageError::Closed);
        }
        if low {
            s.low.push_back(job);
        } else {
            s.high.push_back(job);
        }
        drop(s);
        self.ready.notify_one();
        Ok(())
    }

    /// Naechster Job, eigene zuerst; `None`, wenn geschlossen und leer.
    fn pop(&self) -> Option<Job> {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(j) = s.high.pop_front().or_else(|| s.low.pop_front()) {
                return Some(j);
            }
            if s.closed {
                return None;
            }
            s = self.ready.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed = true;
        self.ready.notify_all();
    }
}

struct Inner {
    queue: Arc<Queue>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Was schon wartet, laeuft noch; danach endet der Thread.
        self.queue.close();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Handle auf den DB-Thread; billig klonbar. Der Thread endet mit dem letzten Handle.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
    /// Jobs dieses Handles kommen hinter die eigenen (siehe [`Db::background`]).
    low: bool,
}

/// Oeffnet die Connection mit den Standard-Pragmas und wendet Migrationen an.
pub fn open_connection(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    // Exklusiv: solange Campfire laeuft, kann kein anderes Programm die Datenbank sperren
    // (oder nebenher lesen). Muss vor dem WAL-Modus stehen, dann braucht WAL keinen
    // gemeinsamen Speicher.
    conn.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    migrations().to_latest(&mut conn)?;
    // Sperre sofort nehmen, nicht erst beim ersten Schreiben.
    conn.execute_batch("BEGIN EXCLUSIVE; COMMIT;")?;
    Ok(conn)
}

impl Db {
    /// Legt Verzeichnisse und DB beim ersten Start an und startet den DB-Thread.
    pub fn open(paths: &Paths) -> Result<Self> {
        paths.create_dirs()?;
        paths.migrate_from_roaming()?;
        Self::open_file(&paths.db_file())
    }

    pub fn open_file(path: &Path) -> Result<Self> {
        // Fehler beim Oeffnen/Migrieren kommen synchron zurueck.
        let conn = open_connection(path)?;
        let queue = Arc::new(Queue::default());
        let q = queue.clone();
        let thread = std::thread::Builder::new()
            .name("db".into())
            .spawn(move || {
                let mut conn = conn;
                while let Some(job) = q.pop() {
                    job(&mut conn);
                }
            })?;
        Ok(Self {
            inner: Arc::new(Inner {
                queue,
                thread: Some(thread),
            }),
            low: false,
        })
    }

    /// Handle fuer Arbeit, die von anderen ausgeloest wird (Sync, Anfragen, Dateien): seine
    /// Jobs laufen erst, wenn keine eigenen warten.
    pub fn background(&self) -> Db {
        Db {
            inner: self.inner.clone(),
            low: true,
        }
    }

    fn send(&self, job: Job) -> Result<()> {
        self.inner.queue.push(job, self.low)
    }

    /// Fuehrt `f` auf dem DB-Thread aus und wartet auf das Ergebnis.
    /// Verboten auf Threads, die `forbid_on_current_thread` aufgerufen haben.
    pub fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<R> + Send + 'static,
    ) -> Result<R> {
        if FORBIDDEN.with(Cell::get) {
            return Err(StorageError::ForbiddenThread);
        }
        let (rtx, rrx) = mpsc::sync_channel(1);
        self.send(Box::new(move |c| {
            let _ = rtx.send(f(c));
        }))?;
        Ok(rrx.recv().map_err(|_| StorageError::Closed)??)
    }

    /// Fuehrt `f` aus und ruft danach `done` auf dem DB-Thread auf (nicht blockierend).
    pub fn spawn<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<R> + Send + 'static,
        done: impl FnOnce(Result<R>) + Send + 'static,
    ) -> Result<()> {
        self.send(Box::new(move |c| done(f(c).map_err(Into::into))))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod move_tests {
    use super::*;

    #[test]
    fn database_moves_once_with_its_wal() {
        let t = tempfile::tempdir().unwrap();
        let (live, old, new) = (
            t.path().join("live"),
            t.path().join("roaming"),
            t.path().join("local"),
        );
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&old).unwrap();
        {
            // Kopie bei offener Verbindung: der Eintrag steht nur im WAL, wie nach einem Absturz.
            let c = open_connection(&live.join(DB_FILE)).unwrap();
            c.execute("INSERT INTO settings (key, value) VALUES ('k', 'v')", [])
                .unwrap();
            // Im exklusiven Modus gibt es keine `-shm` (0.6 hatte eine).
            for suffix in ["", "-wal", "-shm"] {
                let f = format!("{DB_FILE}{suffix}");
                if live.join(&f).is_file() {
                    std::fs::copy(live.join(&f), old.join(&f)).unwrap();
                }
            }
        }
        move_db(&old, &new).unwrap();
        assert!(!old.join(DB_FILE).exists());
        let c = open_connection(&new.join(DB_FILE)).unwrap();
        let v: String = c
            .query_row("SELECT value FROM settings WHERE key = 'k'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(v, "v");
        // Liegt am neuen Ort schon eine, wird nichts ueberschrieben.
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join(DB_FILE), b"alt").unwrap();
        move_db(&old, &new).unwrap();
        assert_eq!(std::fs::read(old.join(DB_FILE)).unwrap(), b"alt");
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    /// Solange Campfire die Datenbank offen hat, kann kein anderes Programm sie sperren.
    #[test]
    fn other_programs_cannot_lock_the_open_database() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join(DB_FILE);
        let db = Db::open_file(&path).unwrap();
        let other = Connection::open(&path).unwrap();
        other
            .busy_timeout(std::time::Duration::from_millis(100))
            .unwrap();
        assert!(other.execute_batch("BEGIN EXCLUSIVE; COMMIT;").is_err());
        // Campfire selbst arbeitet weiter.
        db.call(|c| c.execute("INSERT INTO settings (key, value) VALUES ('k', 'v')", []))
            .unwrap();
    }

    /// Eigene Jobs ueberholen wartende Hintergrund-Jobs.
    #[test]
    fn own_jobs_run_before_background_jobs() {
        let t = tempfile::tempdir().unwrap();
        let db = Db::open_file(&t.path().join(DB_FILE)).unwrap();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let order = Arc::new(Mutex::new(Vec::new()));
        // Blockiert den DB-Thread, bis alle Jobs in der Warteschlange stehen.
        db.spawn(move |_| Ok(gate_rx.recv().ok()), |_| {}).unwrap();
        let bg = db.background();
        for i in 0..3 {
            let o = order.clone();
            bg.spawn(
                move |_| Ok(()),
                move |_| o.lock().unwrap().push(format!("bg{i}")),
            )
            .unwrap();
        }
        let o = order.clone();
        db.spawn(
            move |_| Ok(()),
            move |_| o.lock().unwrap().push("own".into()),
        )
        .unwrap();
        gate_tx.send(()).unwrap();
        db.call(|_| Ok(())).unwrap();
        bg.call(|_| Ok(())).unwrap();
        assert_eq!(order.lock().unwrap()[0], "own");
    }
}

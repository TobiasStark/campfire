//! Lokaler Speicher: SQLite (WAL) auf einem eigenen DB-Thread.
//!
//! Die Connection lebt ausschliesslich auf dem DB-Thread. Aufrufer geben
//! Closures ab (`call` blockierend fuer Worker-Threads, `spawn` fuer
//! Fire-and-forget mit Callback). Der UI-Thread ruft
//! [`forbid_on_current_thread`] auf; danach lehnt `call` dort ab, nur `spawn` geht.

mod schema;

use rusqlite::Connection;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

pub use schema::migrations;

pub const APP_DIR_NAME: &str = "Campfire";

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Migration: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("DB-Thread ist beendet")]
    Closed,
    #[error("Blockierender DB-Zugriff aus diesem Thread (UI) ist verboten; spawn nutzen")]
    ForbiddenThread,
    #[error("Umgebungsvariable {0} fehlt")]
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
    /// `%APPDATA%\Campfire` – Datenbank.
    pub data_dir: PathBuf,
    /// `%LOCALAPPDATA%\Campfire\blobs`.
    pub blobs_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        let roaming = std::env::var_os("APPDATA").ok_or(StorageError::MissingEnv("APPDATA"))?;
        let local =
            std::env::var_os("LOCALAPPDATA").ok_or(StorageError::MissingEnv("LOCALAPPDATA"))?;
        Ok(Self {
            data_dir: Path::new(&roaming).join(APP_DIR_NAME),
            blobs_dir: Path::new(&local).join(APP_DIR_NAME).join("blobs"),
        })
    }

    /// Alles unter einem Wurzelverzeichnis (Tests, portable Nutzung).
    pub fn in_dir(root: &Path) -> Self {
        Self {
            data_dir: root.join("data"),
            blobs_dir: root.join("blobs"),
        }
    }

    pub fn db_file(&self) -> PathBuf {
        self.data_dir.join("app.db")
    }

    pub fn create_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(&self.blobs_dir)?;
        Ok(())
    }
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

struct Inner {
    tx: Option<Sender<Job>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Handle auf den DB-Thread; billig klonbar. Der Thread endet mit dem letzten Handle.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

/// Oeffnet die Connection mit den Standard-Pragmas und wendet Migrationen an.
pub fn open_connection(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    migrations().to_latest(&mut conn)?;
    Ok(conn)
}

impl Db {
    /// Legt Verzeichnisse und DB beim ersten Start an und startet den DB-Thread.
    pub fn open(paths: &Paths) -> Result<Self> {
        paths.create_dirs()?;
        Self::open_file(&paths.db_file())
    }

    pub fn open_file(path: &Path) -> Result<Self> {
        // Fehler beim Oeffnen/Migrieren kommen synchron zurueck.
        let conn = open_connection(path)?;
        let (tx, rx) = mpsc::channel::<Job>();
        let thread = std::thread::Builder::new()
            .name("db".into())
            .spawn(move || {
                let mut conn = conn;
                while let Ok(job) = rx.recv() {
                    job(&mut conn);
                }
            })?;
        Ok(Self {
            inner: Arc::new(Inner {
                tx: Some(tx),
                thread: Some(thread),
            }),
        })
    }

    fn send(&self, job: Job) -> Result<()> {
        self.inner
            .tx
            .as_ref()
            .ok_or(StorageError::Closed)?
            .send(job)
            .map_err(|_| StorageError::Closed)
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

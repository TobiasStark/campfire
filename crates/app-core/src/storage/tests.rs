use super::*;
use std::process::{Command, Stdio};

fn table_names(conn: &Connection) -> Vec<String> {
    let mut s = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap();
    s.query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn user_version(c: &Connection) -> i64 {
    c.query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn first_start_creates_db_and_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(tmp.path());
    assert!(!paths.db_file().exists());
    let db = Db::open(&paths).unwrap();
    assert!(paths.db_file().exists() && paths.blobs_dir.is_dir());
    let (names, mode): (Vec<String>, String) = db
        .call(|c| {
            let m: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            Ok((table_names(c), m))
        })
        .unwrap();
    assert_eq!(mode, "wal");
    for t in [
        "identity",
        "friends",
        "settings",
        "events",
        "author_heads",
        "spaces",
        "members",
        "channels",
        "messages",
        "reactions",
        "attachments",
        "read_state",
        "blobs_meta",
    ] {
        assert!(names.iter().any(|n| n == t), "Tabelle {t} fehlt");
    }
}

#[test]
fn migrations_are_valid_and_idempotent() {
    migrations().validate().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("a.db");
    {
        let db = Db::open_file(&path).unwrap();
        db.call(|c| c.execute("INSERT INTO settings VALUES ('k', 'v')", []))
            .unwrap();
    }
    // Mehrfaches Oeffnen aendert weder Schema noch Daten.
    for _ in 0..3 {
        let c = open_connection(&path).unwrap();
        assert_eq!(user_version(&c), 14);
        let n: i64 = c
            .query_row("SELECT count(*) FROM settings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}

#[test]
fn ui_thread_cannot_block_on_db() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open_file(&tmp.path().join("a.db")).unwrap();
    let db2 = db.clone();
    std::thread::spawn(move || {
        forbid_on_current_thread();
        assert!(matches!(
            db2.call(|_| Ok(())),
            Err(StorageError::ForbiddenThread)
        ));
        // spawn bleibt erlaubt.
        let (tx, rx) = mpsc::channel();
        db2.spawn(
            |c| c.query_row("SELECT 41 + 1", [], |r| r.get::<_, i64>(0)),
            move |r| tx.send(r.unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(rx.recv().unwrap(), 42);
    })
    .join()
    .unwrap();
}

/// Kindprozess: schreibt Batches (je 10 Zeilen, eine Transaktion) bis er gekillt wird.
#[test]
#[ignore = "wird nur vom Absturztest als Kindprozess gestartet"]
fn crash_child() {
    let Some(path) = std::env::var_os("CRASH_DB") else {
        return;
    };
    let db = Db::open_file(Path::new(&path)).unwrap();
    db.call(|c| {
        c.execute_batch("CREATE TABLE IF NOT EXISTS t (batch INTEGER, i INTEGER, pad BLOB)")
    })
    .unwrap();
    println!("ready");
    let mut batch: i64 = db
        .call(|c| {
            c.query_row("SELECT coalesce(max(batch) + 1, 0) FROM t", [], |r| {
                r.get(0)
            })
        })
        .unwrap();
    loop {
        db.call(move |c| {
            let tx = c.transaction()?;
            for i in 0..10 {
                tx.execute("INSERT INTO t VALUES (?1, ?2, zeroblob(4096))", (batch, i))?;
            }
            tx.commit()
        })
        .unwrap();
        batch += 1;
    }
}

#[test]
fn killed_process_does_not_corrupt_db() {
    use std::io::{BufRead, BufReader};
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("crash.db");
    let exe = std::env::current_exe().unwrap();
    let mut total_batches = 0;
    for round in 0..5u64 {
        let mut child = Command::new(&exe)
            .args([
                "--exact",
                "storage::tests::crash_child",
                "--include-ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("CRASH_DB", &path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        loop {
            if lines.next().unwrap().unwrap().contains("ready") {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(150 + round * 40));
        child.kill().unwrap(); // harter Abbruch mitten im Schreiben
        child.wait().unwrap();

        let c = open_connection(&path).unwrap();
        let ok: String = c
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok");
        // Atomaritaet: jeder Batch ist vollstaendig oder gar nicht da.
        let bad: i64 = c
            .query_row(
                "SELECT count(*) FROM (SELECT batch FROM t GROUP BY batch HAVING count(*) != 10)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bad, 0, "halber Batch nach Abbruch in Runde {round}");
        total_batches = c
            .query_row("SELECT count(DISTINCT batch) FROM t", [], |r| r.get(0))
            .unwrap();
    }
    assert!(total_batches > 0, "Kindprozess hat nichts geschrieben");
}

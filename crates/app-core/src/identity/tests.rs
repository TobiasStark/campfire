use super::*;
use crate::storage::open_connection;

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn mem_db() -> (tempfile::TempDir, Connection) {
    let tmp = tempfile::tempdir().unwrap();
    let conn = open_connection(&tmp.path().join("t.db")).unwrap();
    (tmp, conn)
}

#[test]
fn generated_identity_has_valid_cert() {
    let id = Identity::generate("  Tobi  ").unwrap();
    assert_eq!(id.display_name, "Tobi");
    assert_eq!(id.cert().user, id.user_pubkey());
    assert_eq!(id.cert().device, id.device_pubkey());
    assert_ne!(id.user_pubkey(), id.device_pubkey());
    assert!(verify_cert(id.cert()));

    let mut bad = id.cert().clone();
    bad.created_at += 1;
    assert!(!verify_cert(&bad));
    let mut bad = id.cert().clone();
    bad.device = [7; 32];
    assert!(!verify_cert(&bad));
    let other = Identity::generate("X").unwrap();
    let mut bad = id.cert().clone();
    bad.user = other.user_pubkey();
    assert!(!verify_cert(&bad));
}

#[test]
fn name_validation() {
    assert!(matches!(
        Identity::generate(""),
        Err(IdentityError::BadName)
    ));
    assert!(matches!(
        Identity::generate("   "),
        Err(IdentityError::BadName)
    ));
    assert!(matches!(
        Identity::generate("a\nb"),
        Err(IdentityError::BadName)
    ));
    assert!(matches!(
        Identity::generate(&"x".repeat(MAX_NAME_LEN + 1)),
        Err(IdentityError::BadName)
    ));
    assert!(Identity::generate("Jörg \u{1F525}").is_ok());
}

#[cfg(windows)]
#[test]
fn dpapi_save_load_roundtrip_and_secrets_not_plain_in_db() {
    let (_tmp, conn) = mem_db();
    assert!(Identity::load(&conn, &Dpapi).unwrap().is_none());
    let id = Identity::generate("Tobi").unwrap();
    id.save(&conn, &Dpapi).unwrap();
    // Zweites Speichern ueberschreibt.
    id.save(&conn, &Dpapi).unwrap();

    let loaded = Identity::load(&conn, &Dpapi).unwrap().unwrap();
    assert_eq!(loaded.user_pubkey(), id.user_pubkey());
    assert_eq!(loaded.device_pubkey(), id.device_pubkey());
    assert_eq!(loaded.cert(), id.cert());
    assert_eq!(loaded.display_name, "Tobi");

    let (u, d): (Vec<u8>, Vec<u8>) = conn
        .query_row("SELECT user_secret, device_secret FROM identity", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert!(!contains(&u, &id.user.to_bytes()));
    assert!(!contains(&d, &id.device_secret_bytes()[..]));
    // Manipulierter Blob wird erkannt statt still akzeptiert.
    conn.execute("UPDATE identity SET user_secret = zeroblob(10)", [])
        .unwrap();
    assert!(Identity::load(&conn, &Dpapi).is_err());
}

#[test]
fn backup_restores_same_user_on_fresh_system() {
    let id = Identity::generate("Tobi").unwrap();
    let file = id.export_backup("korrekt-pferd-batterie").unwrap();

    // Frisches System: leere DB, kein Zugriff auf das Original.
    let (_tmp, conn) = mem_db();
    let restored = Identity::import_backup(&file, "korrekt-pferd-batterie").unwrap();
    assert_eq!(restored.user_pubkey(), id.user_pubkey());
    assert_eq!(restored.display_name, "Tobi");
    assert_ne!(restored.device_pubkey(), id.device_pubkey());
    assert!(verify_cert(restored.cert()));
    assert_eq!(restored.cert().user, id.user_pubkey());

    #[cfg(windows)]
    {
        restored.save(&conn, &Dpapi).unwrap();
        let again = Identity::load(&conn, &Dpapi).unwrap().unwrap();
        assert_eq!(again.user_pubkey(), id.user_pubkey());
    }
    let _ = conn;
}

#[test]
fn backup_rejects_wrong_passphrase_garbage_and_weak() {
    let id = Identity::generate("Tobi").unwrap();
    assert!(matches!(
        id.export_backup("kurz"),
        Err(IdentityError::WeakPassphrase)
    ));
    let file = id.export_backup("korrekt-pferd-batterie").unwrap();
    assert!(matches!(
        Identity::import_backup(&file, "falsch-falsch-falsch"),
        Err(IdentityError::BadBackup)
    ));
    assert!(matches!(
        Identity::import_backup(b"kein backup", "korrekt-pferd-batterie"),
        Err(IdentityError::BadBackup)
    ));
    let mut cut = file.clone();
    cut.truncate(cut.len() / 2);
    assert!(Identity::import_backup(&cut, "korrekt-pferd-batterie").is_err());
    // Die Datei enthaelt den Schluessel nicht im Klartext.
    assert!(!contains(&file, &id.user.to_bytes()));
}

#[test]
fn secrets_never_appear_in_debug_or_error_output() {
    let id = Identity::generate("Tobi").unwrap();
    let user = id.user.to_bytes();
    let device = id.device_secret_bytes();
    let dbg = format!("{id:?} {:?}", id.cert());
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    for secret in [&user[..], &device[..]] {
        assert!(!dbg.contains(&hex(secret)));
        assert!(!dbg.contains(&format!("{:?}", secret)));
        assert!(!dbg
            .to_lowercase()
            .contains(&data_encoding::HEXLOWER.encode(secret)));
    }
    assert!(dbg.contains("redacted"));
    let err = format!(
        "{}",
        Identity::import_backup(b"x", "passphrase123").unwrap_err()
    );
    assert!(!err.contains("passphrase123"));
}

#[test]
fn backup_flag_roundtrip() {
    let (_tmp, conn) = mem_db();
    assert!(!backup_done(&conn).unwrap());
    mark_backup_done(&conn).unwrap();
    mark_backup_done(&conn).unwrap();
    assert!(backup_done(&conn).unwrap());
}

#[test]
fn backup_files_never_replace_an_existing_backup() {
    let dir = tempfile::tempdir().unwrap();
    let first = write_backup_file(dir.path(), b"altes konto").unwrap();
    assert_eq!(first, dir.path().join(BACKUP_FILE_NAME));
    let second = write_backup_file(dir.path(), b"neues konto").unwrap();
    assert_eq!(second, dir.path().join("Campfire-Backup (2).age"));
    assert_eq!(std::fs::read(&first).unwrap(), b"altes konto");
    assert_eq!(std::fs::read(&second).unwrap(), b"neues konto");
}

#[test]
fn weak_patterns_are_refused_and_generated_passwords_are_strong() {
    for (i, weak) in [
        "aaaaaaaaaaaaaaaa",
        "abcdefghijklmnop",
        "Password1234!",
        "geheim-geheim",
        "123456789012",
        "abcabcabcabcabc",
    ]
    .into_iter()
    .enumerate()
    {
        assert!(!passphrase_ok(weak), "weak passphrase #{i} was accepted");
    }
    for (i, strong) in [
        "korrekt-pferd-batterie",
        "Nebel-Fuchs-7-Kahn",
        "mein hund frisst gerne blaue socken",
    ]
    .into_iter()
    .enumerate()
    {
        assert!(passphrase_ok(strong), "strong passphrase #{i} was refused");
    }
    for _ in 0..50 {
        let g = generate_passphrase();
        assert_eq!(g.len(), 29);
        assert!(passphrase_ok(&g), "generated passphrase was refused");
    }
    assert_ne!(*generate_passphrase(), *generate_passphrase());
}

#[test]
fn passphrase_is_taken_as_typed_and_old_trimmed_backups_still_open() {
    let id = Identity::generate("Tobi").unwrap();
    assert!(matches!(
        id.export_backup("elf zeichen"),
        Err(IdentityError::WeakPassphrase)
    ));
    // Leerzeichen am Rand gehoeren zum Passwort.
    let file = id.export_backup(" Nebel-Fuchs-7-Kahn ").unwrap();
    assert!(Identity::import_backup(&file, " Nebel-Fuchs-7-Kahn ").is_ok());
    assert!(matches!(
        Identity::import_backup(&file, "Nebel-Fuchs-7-Kahn"),
        Err(IdentityError::BadBackup)
    ));
    // Bis 0.5.0 wurde beim Anlegen gekuerzt: "X " ergab ein Backup mit "X".
    let old = id.export_backup("Nebel-Fuchs-7-Kahn").unwrap();
    let back = Identity::import_backup(&old, "Nebel-Fuchs-7-Kahn ").unwrap();
    assert_eq!(back.user_pubkey(), id.user_pubkey());
}

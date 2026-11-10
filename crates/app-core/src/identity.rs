//! Identitaet: User-Key, Device-Key, Device-Zertifikat, Schluessel-Backup.
//!
//! Geheimnisse liegen in der DB nur DPAPI-geschuetzt. `Identity` hat ein
//! geschwaerztes `Debug` und kein `Display`, damit Schluessel nie in Logs landen.

use app_proto::control::DeviceCert;
use app_proto::{Hash, PublicKey, Signature};
use ed25519_dalek::{Signer, SigningKey};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const ENTROPY: &[u8] = b"campfire/identity/1";
const BACKUP_VERSION: u8 = 1;
const SETTING_BACKUP_DONE: &str = "identity.backup_done";
/// Mindestlaenge der Backup-Passphrase. Das Backup liegt oft in einem synchronisierten Ordner
/// (OneDrive-"Dokumente"); scrypt bremst Rateversuche, schuetzt aber kein `Sommer24`.
pub const MIN_PASSPHRASE_LEN: usize = 12;
/// Mindeststaerke der Backup-Passphrase (geschaetzte Bits, siehe [`passphrase_bits`]) (N-9).
pub const MIN_PASSPHRASE_BITS: f64 = 60.0;

/// Haeufige Bausteine schwacher Passwoerter; sie zaehlen bei der Schaetzung nicht mit.
const COMMON: &[&str] = &[
    "password",
    "passwort",
    "qwertz",
    "qwerty",
    "asdf",
    "123456",
    "654321",
    "abcdef",
    "letmein",
    "campfire",
    "hallo",
    "hello",
    "admin",
    "welcome",
    "willkommen",
    "sommer",
    "summer",
    "winter",
    "geheim",
    "secret",
    "iloveyou",
    "ichliebedich",
    "dragon",
    "monkey",
    "football",
    "fussball",
];

/// Grobe, konservative Schaetzung der Passwortstaerke in Bit: Zeichenvorrat hoch Anzahl
/// verschiedener, nicht trivialer Zeichenpaare. Wiederholungen ("abcabc"), Folgen ("1234",
/// "aaaa") und haeufige Woerter zaehlen nicht. Zufaellige Passphrasen aus mehreren Woertern
/// oder generierte Passwoerter liegen weit darueber.
pub fn passphrase_bits(p: &str) -> f64 {
    let mut s = p.to_lowercase();
    for w in COMMON {
        s = s.replace(w, " ");
    }
    // Zeichenvorrat aus dem Original (Gross/Klein zaehlt).
    let mut pool = 0u32;
    if p.chars().any(|c| c.is_ascii_lowercase()) {
        pool += 26;
    }
    if p.chars().any(|c| c.is_ascii_uppercase()) {
        pool += 26;
    }
    if p.chars().any(|c| c.is_ascii_digit()) {
        pool += 10;
    }
    if p.chars().any(|c| c.is_ascii_punctuation() || c == ' ') {
        pool += 33;
    }
    if !p.is_ascii() {
        pool += 100;
    }
    let chars: Vec<char> = s.chars().collect();
    let mut pairs = std::collections::HashSet::new();
    for w in chars.windows(2) {
        let (a, b) = (w[0] as i64, w[1] as i64);
        if a != b && (a - b).abs() != 1 {
            pairs.insert((w[0], w[1]));
        }
    }
    let effective = pairs.len() + usize::from(!chars.is_empty());
    effective as f64 * f64::from(pool.max(1)).log2()
}

/// Lang und stark genug fuer ein Backup.
pub fn passphrase_ok(p: &str) -> bool {
    p.chars().count() >= MIN_PASSPHRASE_LEN && passphrase_bits(p) >= MIN_PASSPHRASE_BITS
}

/// Zufaelliges Passwort zum Aufschreiben: 5 Gruppen zu 5 Zeichen ohne verwechselbare Zeichen
/// (etwa 124 Bit), z. B. `k7m2q-x9p4t-...`.
pub fn generate_passphrase() -> Zeroizing<String> {
    const ABC: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut out = Zeroizing::new(String::with_capacity(29));
    let mut buf = Zeroizing::new([0u8; 25]);
    // Ohne Zufall kein Passwort: lieber abbrechen als ein vorhersagbares liefern.
    getrandom::fill(buf.as_mut()).expect("Zufallsquelle nicht verfuegbar");
    for (i, b) in buf.iter().enumerate() {
        if i > 0 && i % 5 == 0 {
            out.push('-');
        }
        // 256 % 31 != 0: winzige Schieflage, bei 124 Bit bedeutungslos.
        out.push(ABC[usize::from(*b) % ABC.len()] as char);
    }
    out
}
/// Standardname der Backup-Datei.
pub const BACKUP_FILE_NAME: &str = "Campfire-Backup.age";
pub const MAX_NAME_LEN: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error(
        "{}",
        app_proto::i18n::tr(
            "Display name is empty or too long",
            "Anzeigename ist leer oder zu lang"
        )
    )]
    BadName,
    #[error("{}", app_proto::tr!("Password is too short or too easy to guess (at least {} characters, no simple patterns)", "Passwort ist zu kurz oder zu leicht zu erraten (mindestens {} Zeichen, keine einfachen Muster)", MIN_PASSPHRASE_LEN))]
    WeakPassphrase,
    #[error(
        "{}",
        app_proto::i18n::tr(
            "Backup could not be decrypted (wrong passphrase or damaged file)",
            "Backup konnte nicht entschlüsselt werden (falsche Passphrase oder beschädigte Datei)"
        )
    )]
    BadBackup,
    #[error(
        "{}",
        app_proto::i18n::tr("Backup has an unknown version", "Backup hat eine unbekannte Version")
    )]
    UnsupportedBackup,
    #[error("Key protection (DPAPI): {0}")]
    Protect(String),
    #[error(
        "{}",
        app_proto::i18n::tr(
            "Stored identity is damaged",
            "Gespeicherte Identitaet ist beschaedigt"
        )
    )]
    Corrupt,
    #[error("Random source: {0}")]
    Random(String),
    #[error(
        "{}",
        app_proto::i18n::tr(
            "Recovery key is invalid or does not belong to this account",
            "Wiederherstellungsschlüssel ist ungültig oder gehört nicht zu diesem Konto"
        )
    )]
    WrongRecoveryKey,
    #[error(
        "{}",
        app_proto::i18n::tr(
            "A recovery key already exists",
            "Es gibt schon einen Wiederherstellungsschlüssel"
        )
    )]
    RecoveryExists,
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, IdentityError>;

/// Schuetzt Geheimnisse im Ruhezustand.
pub trait SecretProtector {
    fn protect(&self, plain: &[u8]) -> Result<Vec<u8>>;
    fn unprotect(&self, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>>;
}

/// Windows-DPAPI, gebunden an den Windows-Benutzer.
#[cfg(windows)]
pub struct Dpapi;

#[cfg(windows)]
impl Dpapi {
    fn blob(data: &[u8]) -> windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
        windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    fn take_out(
        out: windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB,
    ) -> Zeroizing<Vec<u8>> {
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        // SAFETY: DPAPI liefert einen mit LocalAlloc angelegten Puffer der Laenge cbData.
        unsafe {
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            // Klartext-Ausgabe vor dem Freigeben loeschen (bei unprotect relevant).
            std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize).fill(0);
            let _ = LocalFree(Some(HLOCAL(out.pbData as *mut _)));
            Zeroizing::new(v)
        }
    }
}

#[cfg(windows)]
impl SecretProtector for Dpapi {
    fn protect(&self, plain: &[u8]) -> Result<Vec<u8>> {
        use windows::core::w;
        use windows::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
        let input = Self::blob(plain);
        let entropy = Self::blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: Zeiger zeigen auf lebende Puffer; out wird von DPAPI befuellt.
        unsafe {
            CryptProtectData(
                &input,
                w!("campfire"),
                Some(&entropy),
                None,
                None,
                0,
                &mut out,
            )
            .map_err(|e| IdentityError::Protect(e.to_string()))?;
        }
        Ok(Self::take_out(out).to_vec())
    }

    fn unprotect(&self, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
        let input = Self::blob(sealed);
        let entropy = Self::blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: wie oben.
        unsafe {
            CryptUnprotectData(&input, None, Some(&entropy), None, None, 0, &mut out)
                .map_err(|e| IdentityError::Protect(e.to_string()))?;
        }
        Ok(Self::take_out(out))
    }
}

fn random_key() -> Result<SigningKey> {
    let mut seed = Zeroizing::new([0u8; 32]);
    getrandom::fill(&mut *seed).map_err(|e| IdentityError::Random(e.to_string()))?;
    Ok(SigningKey::from_bytes(&seed))
}

fn validate_name(name: &str) -> Result<String> {
    let n = app_proto::text::clean_name(name, usize::MAX);
    if n.is_empty() || n.len() > MAX_NAME_LEN || name.chars().any(char::is_control) {
        return Err(IdentityError::BadName);
    }
    Ok(n)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub use app_net::verify_cert;

pub struct Identity {
    user: SigningKey,
    device: SigningKey,
    cert: DeviceCert,
    pub display_name: String,
    pub avatar_hash: Option<Hash>,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("user_pubkey", &"<public>")
            .field("secrets", &"<redacted>")
            .field("display_name", &self.display_name)
            .finish()
    }
}

impl Identity {
    /// Neue Identitaet mit frischem User- und Device-Key.
    pub fn generate(display_name: &str) -> Result<Self> {
        let name = validate_name(display_name)?;
        Self::with_user_key(random_key()?, name, None)
    }

    fn with_user_key(
        user: SigningKey,
        display_name: String,
        avatar_hash: Option<Hash>,
    ) -> Result<Self> {
        let device = random_key()?;
        let cert = Self::issue_cert(&user, &device);
        Ok(Self {
            user,
            device,
            cert,
            display_name,
            avatar_hash,
        })
    }

    fn issue_cert(user: &SigningKey, device: &SigningKey) -> DeviceCert {
        let user_pk = user.verifying_key().to_bytes();
        let device_pk = device.verifying_key().to_bytes();
        let created_at = now_secs();
        let sig = user.sign(&DeviceCert::signing_bytes(&user_pk, &device_pk, created_at));
        DeviceCert {
            user: user_pk,
            device: device_pk,
            created_at,
            signature: Signature(sig.to_bytes()),
        }
    }

    pub fn user_pubkey(&self) -> PublicKey {
        self.user.verifying_key().to_bytes()
    }

    /// Eigener Freundescode fuer die Generation `epoch` (siehe `invite_secret`); `relay_url`
    /// ist das Heim-Relay, falls bekannt.
    pub fn friend_code(
        &self,
        relay_url: Option<String>,
        epoch: u64,
    ) -> app_proto::friend_code::FriendCode {
        app_proto::friend_code::FriendCode {
            user: self.user_pubkey(),
            node_id: self.device_pubkey(),
            secret: crate::friends::invite_secret(&self.invite_key(), epoch),
            relay_url,
        }
    }

    /// Schluessel fuer die Einladungsgeheimnisse (aus dem User-Key abgeleitet, damit ein
    /// wiederhergestelltes Konto dieselben Codes hat).
    pub fn invite_key(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(blake3::derive_key(
            "campfire 2026-11 friend code invite secret",
            &self.user.to_bytes(),
        ))
    }

    /// Entspricht der iroh-NodeId.
    pub fn device_pubkey(&self) -> PublicKey {
        self.device.verifying_key().to_bytes()
    }

    pub fn cert(&self) -> &DeviceCert {
        &self.cert
    }

    /// Geheimer Device-Schluessel (32 Bytes) fuer den iroh-Endpoint.
    pub fn device_secret_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.device.to_bytes())
    }

    /// Signiert mit dem User-Key (Events, M1-08).
    pub fn sign_user(&self, msg: &[u8]) -> Signature {
        Signature(self.user.sign(msg).to_bytes())
    }

    // ---- Speicherung ----

    pub fn save(&self, conn: &Connection, protector: &dyn SecretProtector) -> Result<()> {
        let user = protector.protect(&self.user.to_bytes())?;
        let device = protector.protect(&self.device.to_bytes())?;
        let cert = app_proto::encode(&self.cert).map_err(|_| IdentityError::Corrupt)?;
        conn.execute(
            "INSERT INTO identity (id, user_secret, device_secret, device_cert, display_name, avatar_hash)
             VALUES (1, ?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET user_secret = ?1, device_secret = ?2,
                 device_cert = ?3, display_name = ?4, avatar_hash = ?5",
            (user, device, cert, &self.display_name, self.avatar_hash.as_ref().map(|h| h.to_vec())),
        )?;
        Ok(())
    }

    pub fn load(conn: &Connection, protector: &dyn SecretProtector) -> Result<Option<Self>> {
        type Row = (Vec<u8>, Vec<u8>, Vec<u8>, String, Option<Vec<u8>>);
        let row: Option<Row> = conn
            .query_row(
                "SELECT user_secret, device_secret, device_cert, display_name, avatar_hash FROM identity WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((user, device, cert, display_name, avatar)) = row else {
            return Ok(None);
        };
        let key = |sealed: &[u8]| -> Result<SigningKey> {
            let plain = protector.unprotect(sealed)?;
            let arr: [u8; 32] = plain
                .as_slice()
                .try_into()
                .map_err(|_| IdentityError::Corrupt)?;
            let k = SigningKey::from_bytes(&arr);
            Ok(k)
        };
        let user = key(&user)?;
        let device = key(&device)?;
        let cert: DeviceCert = app_proto::decode(&cert).map_err(|_| IdentityError::Corrupt)?;
        let avatar_hash = avatar
            .map(|a| Hash::try_from(a.as_slice()).map_err(|_| IdentityError::Corrupt))
            .transpose()?;
        if cert.user != user.verifying_key().to_bytes()
            || cert.device != device.verifying_key().to_bytes()
            || !verify_cert(&cert)
        {
            return Err(IdentityError::Corrupt);
        }
        Ok(Some(Self {
            user,
            device,
            cert,
            display_name,
            avatar_hash,
        }))
    }

    // ---- Backup ----

    /// Exportiert User-Key und Profil als `age`-Datei (scrypt-Passphrase).
    /// Der Device-Key gehoert zum Geraet und ist bewusst nicht enthalten.
    pub fn export_backup(&self, passphrase: &str) -> Result<Vec<u8>> {
        use age::secrecy::SecretString;
        if !passphrase_ok(passphrase) {
            return Err(IdentityError::WeakPassphrase);
        }
        let payload = Zeroizing::new(
            postcard::to_stdvec(&BackupPayload {
                version: BACKUP_VERSION,
                user_secret: self.user.to_bytes(),
                display_name: self.display_name.clone(),
                avatar_hash: self.avatar_hash,
            })
            .map_err(|_| IdentityError::Corrupt)?,
        );
        let recipient = age::scrypt::Recipient::new(SecretString::from(passphrase.to_owned()));
        age::encrypt(&recipient, &payload).map_err(|_| IdentityError::BadBackup)
    }

    /// Stellt die Identitaet aus einem Backup wieder her: gleicher User-Key,
    /// neuer Device-Key mit frischem Zertifikat.
    ///
    /// Das Passwort gilt so, wie es eingegeben wurde. Versionen bis 0.5.0 haben es beim Anlegen
    /// an den Raendern beschnitten (`"geheim "` -> `"geheim"`); passt es ungekuerzt nicht, wird
    /// deshalb auch die gekuerzte Form versucht.
    pub fn import_backup(data: &[u8], passphrase: &str) -> Result<Self> {
        match Self::import_backup_exact(data, passphrase) {
            Err(IdentityError::BadBackup) if passphrase.trim() != passphrase => {
                Self::import_backup_exact(data, passphrase.trim())
            }
            r => r,
        }
    }

    fn import_backup_exact(data: &[u8], passphrase: &str) -> Result<Self> {
        use age::secrecy::SecretString;
        let identity = age::scrypt::Identity::new(SecretString::from(passphrase.to_owned()));
        let plain =
            Zeroizing::new(age::decrypt(&identity, data).map_err(|_| IdentityError::BadBackup)?);
        let p: BackupPayload =
            postcard::from_bytes(&plain).map_err(|_| IdentityError::BadBackup)?;
        let secret = Zeroizing::new(p.user_secret);
        if p.version != BACKUP_VERSION {
            return Err(IdentityError::UnsupportedBackup);
        }
        let name = validate_name(&p.display_name).map_err(|_| IdentityError::BadBackup)?;
        Self::with_user_key(SigningKey::from_bytes(&secret), name, p.avatar_hash)
    }
}

/// Schreibt ein Backup nach `dir` und ersetzt dabei **nie** eine vorhandene Datei: Liegt dort
/// schon `Campfire-Backup.age` (z. B. das einzige Backup eines anderen Kontos, per OneDrive
/// wiederhergestellt), heisst das neue `Campfire-Backup (2).age` usw. Gibt den Pfad zurueck.
pub fn write_backup_file(
    dir: &std::path::Path,
    data: &[u8],
) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let stem = BACKUP_FILE_NAME.trim_end_matches(".age");
    for n in 1..1000u32 {
        let path = if n == 1 {
            dir.join(BACKUP_FILE_NAME)
        } else {
            dir.join(format!("{stem} ({n}).age"))
        };
        let mut f = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        if let Err(e) = f.write_all(data).and_then(|()| f.sync_all()) {
            drop(f);
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
        return Ok(path);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free file name for the backup",
    ))
}

#[derive(Serialize, Deserialize)]
struct BackupPayload {
    version: u8,
    user_secret: [u8; 32],
    display_name: String,
    avatar_hash: Option<Hash>,
}

impl Drop for BackupPayload {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.user_secret);
    }
}

/// Merkt, dass der Nutzer ein Backup erstellt hat (Ersteinrichtung zeigt sonst die Warnung).
pub fn mark_backup_done(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, 'true') ON CONFLICT(key) DO UPDATE SET value = 'true'",
        [SETTING_BACKUP_DONE],
    )?;
    Ok(())
}

pub fn backup_done(conn: &Connection) -> Result<bool> {
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [SETTING_BACKUP_DONE],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.as_deref() == Some("true"))
}

// ---- Wiederherstellung und Schluesselwechsel (M-7) ----

const SETTING_RECOVERY: &str = "identity.recovery_commit";
const SETTING_ROTATION: &str = "identity.rotation";

/// Neuer Wiederherstellungsschluessel. Nur `commitment` wird gespeichert; `text` sieht der
/// Nutzer genau einmal.
#[derive(Clone)]
pub struct RecoveryKey {
    pub text: Zeroizing<String>,
    pub commitment: Hash,
}

impl std::fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecoveryKey(<redacted>)")
    }
}

impl RecoveryKey {
    pub fn generate() -> Result<Self> {
        let k = random_key()?;
        Ok(Self {
            text: Zeroizing::new(app_proto::recovery::encode_secret(&k.to_bytes())),
            commitment: app_proto::recovery::commitment(&k.verifying_key().to_bytes()),
        })
    }
}

fn get_hex<const N: usize>(conn: &Connection, key: &str) -> Result<Option<[u8; N]>> {
    let v: Option<String> = crate::settings::get(conn, key)?;
    Ok(
        v.and_then(|h| data_encoding::HEXLOWER.decode(h.as_bytes()).ok())
            .and_then(|b| b.try_into().ok()),
    )
}

/// Hash des eigenen Wiederherstellungsschluessels (geht im Handshake an Freunde).
pub fn recovery_commitment(conn: &Connection) -> Result<Option<Hash>> {
    get_hex(conn, SETTING_RECOVERY)
}

/// Speichert den Hash eines neuen Wiederherstellungsschluessels. Nur einmal moeglich: Freunde
/// halten ohnehin nur den ersten fest, ein zweiter wuerde sie nie erreichen.
pub fn set_recovery_commitment(conn: &Connection, c: &Hash) -> Result<()> {
    if recovery_commitment(conn)?.is_some() {
        return Err(IdentityError::RecoveryExists);
    }
    crate::settings::set(conn, SETTING_RECOVERY, &data_encoding::HEXLOWER.encode(c))?;
    Ok(())
}

/// Der eigene letzte Schluesselwechsel; geht mit jeder Freundschaftsanfrage mit, damit auch
/// Freunde, die die Meldung verpasst haben, ihn uebernehmen.
pub fn own_rotation(conn: &Connection) -> Result<Option<app_proto::recovery::Rotation>> {
    let v: Option<String> = crate::settings::get(conn, SETTING_ROTATION)?;
    Ok(
        v.and_then(|h| data_encoding::HEXLOWER.decode(h.as_bytes()).ok())
            .and_then(|b| app_proto::decode(&b).ok()),
    )
}

/// Ersetzt den User-Key nach einem Geraeteverlust (M-7): neuer User- und Device-Key, mit dem
/// Wiederherstellungsschluessel unterschriebene [`Rotation`](app_proto::recovery::Rotation),
/// Meldung an alle Freunde ueber die Outbox. Gibt den naechsten Wiederherstellungsschluessel
/// zurueck (der alte ist damit verbraucht). Danach muss die App neu starten.
///
/// Spaces des alten Schluessels verschwinden beim Neustart (nicht mehr Mitglied); Backups des
/// alten Schluessels sind wertlos, weil Freunde ihn sperren.
pub fn rotate(
    conn: &mut Connection,
    protector: &dyn SecretProtector,
    current: &Identity,
    recovery_text: &str,
) -> Result<RecoveryKey> {
    use app_proto::recovery::{commitment, decode_secret, Rotation};
    let secret =
        Zeroizing::new(decode_secret(recovery_text).ok_or(IdentityError::WrongRecoveryKey)?);
    let r1 = SigningKey::from_bytes(&secret);
    let r1_pub = r1.verifying_key().to_bytes();
    // Fehlt der Hash (z. B. nach einem Backup-Import), pruefen allein die Freunde.
    if let Some(c) = recovery_commitment(conn)? {
        if c != commitment(&r1_pub) {
            return Err(IdentityError::WrongRecoveryKey);
        }
    }
    let next = Identity::with_user_key(
        random_key()?,
        current.display_name.clone(),
        current.avatar_hash,
    )?;
    let r2 = RecoveryKey::generate()?;
    let (old, new) = (current.user_pubkey(), next.user_pubkey());
    let sig = r1.sign(&Rotation::signing_bytes(&old, &new, &r2.commitment));
    let rot = Rotation {
        old,
        new,
        recovery: r1_pub,
        next: r2.commitment,
        signature: Signature(sig.to_bytes()),
    };
    let rot = app_proto::encode(&rot).map_err(|_| IdentityError::Corrupt)?;
    let tx = conn.transaction()?;
    next.save(&tx, protector)?;
    let hex = data_encoding::HEXLOWER;
    crate::settings::set(&tx, SETTING_RECOVERY, &hex.encode(&r2.commitment))?;
    crate::settings::set(&tx, SETTING_ROTATION, &hex.encode(&rot))?;
    tx.execute("DELETE FROM settings WHERE key = ?1", [SETTING_BACKUP_DONE])?;
    let now = now_secs() as i64;
    use crate::friends::store;
    for f in store::list(&tx)? {
        if f.status != crate::friends::FriendStatus::Accepted {
            continue;
        }
        if let Some(node) = f.rendezvous() {
            store::outbox_put(
                &tx,
                &f.user,
                store::OutboxKind::Rotate,
                &node,
                f.hint.relay_url.as_deref(),
                now,
            )?;
        }
    }
    tx.commit()?;
    Ok(r2)
}

#[cfg(test)]
mod tests;

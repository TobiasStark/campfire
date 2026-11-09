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
    #[error("{}", app_proto::tr!("Passphrase is too short (at least {} characters)", "Passphrase ist zu kurz (mindestens {} Zeichen)", MIN_PASSPHRASE_LEN))]
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
    let n = name.trim();
    if n.is_empty() || n.len() > MAX_NAME_LEN || n.chars().any(char::is_control) {
        return Err(IdentityError::BadName);
    }
    Ok(n.to_owned())
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

    /// Eigener Freundescode; `relay_url` ist das Heim-Relay, falls bekannt.
    pub fn friend_code(&self, relay_url: Option<String>) -> app_proto::friend_code::FriendCode {
        app_proto::friend_code::FriendCode {
            user: self.user_pubkey(),
            node_id: self.device_pubkey(),
            relay_url,
        }
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
        if passphrase.chars().count() < MIN_PASSPHRASE_LEN {
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

#[cfg(test)]
mod tests;

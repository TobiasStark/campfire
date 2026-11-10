//! Einfache Einstellungen (Tabelle `settings`, Werte als JSON).

use rusqlite::{Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Serialize};

pub const THEME: &str = "ui.theme";
/// UI language ([`crate::i18n::Language`]); English when unset.
pub const LANGUAGE: &str = "ui.language";
pub const RELAY_MODE: &str = "net.relay_mode";
pub const AUTOSTART: &str = "app.autostart";
pub const AUDIO_INPUT: &str = "audio.input";
pub const HOTKEYS: &str = "audio.hotkeys";
pub const PEER_PREFS: &str = "audio.peer_prefs";
pub const OVERLAY: &str = "ui.overlay";
pub const AUDIO_DEVICES: &str = "audio.devices";
pub const AUDIO_BITRATE: &str = "audio.bitrate";
pub const SOUNDS: &str = "audio.sounds";
pub const VIDEO: &str = "video.stream";
/// Eigene Farben ([`ColorOverrides`]).
pub const COLORS: &str = "ui.colors";
/// Was automatisch geladen werden darf ([`AutoDownload`]). Ersetzt das fruehere
/// `files.auto_mb` (bis 100 MB alles automatisch), das bewusst nicht uebernommen wird.
pub const FILES_AUTO: &str = "files.auto_download";
/// Groesse des Datei-Speichers in MB, danach werden die aeltesten Dateien entfernt; 0 = unbegrenzt.
pub const FILES_CACHE_MB: &str = "files.cache_mb";
/// Standard fuer [`FILES_CACHE_MB`] (10 GB).
pub const FILES_CACHE_MB_DEFAULT: u64 = 10_240;
/// Speichergrenze fuer Nachrichten in MB (Standard und Minimum in
/// [`crate::eventlog::prune`]); darueber wird das Aelteste aufgeraeumt.
pub const MESSAGES_MB: &str = "messages.limit_mb";
/// Angepinnte (und selbst geteilte) Dateien: werden nie entfernt.
pub const FILES_PINNED: &str = "files.pinned";

/// Automatische Downloads. Standard: **nichts** wird automatisch geladen, auch keine
/// Vorschaubilder; jede Datei laedt erst ein Klick. Der Nutzer kann je Art freigeben, was
/// automatisch geladen werden darf (bis `max_mb`). Programme, Skripte und unbekannte Typen
/// ([`FileKind::Other`](crate::files::FileKind::Other)) laedt die App nie von selbst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AutoDownload {
    /// Vorschaubilder (hoechstens 256 KB je Bild).
    pub previews: bool,
    pub images: bool,
    pub video: bool,
    pub audio: bool,
    /// Text, PDF, Office ohne Makros.
    pub documents: bool,
    pub archives: bool,
    /// Groesste Datei, die automatisch geladen wird.
    pub max_mb: u64,
}

impl Default for AutoDownload {
    fn default() -> Self {
        Self {
            previews: false,
            images: false,
            video: false,
            audio: false,
            documents: false,
            archives: false,
            max_mb: 25,
        }
    }
}

impl AutoDownload {
    /// Ist ueberhaupt eine Dateiart freigegeben?
    pub fn any_files(&self) -> bool {
        self.images || self.video || self.audio || self.documents || self.archives
    }

    /// Darf diese Datei (Name laut Absender, Groesse in Bytes) automatisch geladen werden?
    pub fn allows(&self, name: &str, size: u64) -> bool {
        use crate::files::FileKind;
        let on = match crate::files::file_kind(name) {
            FileKind::Image => self.images,
            FileKind::Video => self.video,
            FileKind::Audio => self.audio,
            FileKind::Document => self.documents,
            FileKind::Archive => self.archives,
            FileKind::Other => false,
        };
        on && size <= self.max_mb.saturating_mul(1024 * 1024)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, Default)]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
}

/// Vom Nutzer geaenderte Farben je Variante: Farbname -> Hex (`#RRGGBB` oder `#RRGGBBAA`).
/// Was fehlt, behaelt die Standardfarbe.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ColorOverrides {
    pub dark: std::collections::BTreeMap<String, String>,
    pub light: std::collections::BTreeMap<String, String>,
}

impl ColorOverrides {
    pub fn map(&self, t: ThemeChoice) -> &std::collections::BTreeMap<String, String> {
        match t {
            ThemeChoice::Dark => &self.dark,
            ThemeChoice::Light => &self.light,
        }
    }

    pub fn map_mut(&mut self, t: ThemeChoice) -> &mut std::collections::BTreeMap<String, String> {
        match t {
            ThemeChoice::Dark => &mut self.dark,
            ThemeChoice::Light => &mut self.light,
        }
    }
}

pub fn get<T: DeserializeOwned>(conn: &Connection, key: &str) -> rusqlite::Result<Option<T>> {
    let v: Option<String> = conn
        .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(v.and_then(|s| serde_json::from_str(&s).ok()))
}

pub fn set<T: Serialize>(conn: &Connection, key: &str, value: &T) -> rusqlite::Result<()> {
    let json = serde_json::to_string(value).unwrap_or_else(|_| "null".into());
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = ?2",
        (key, json),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::open_connection;

    #[test]
    fn roundtrip_and_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let c = open_connection(&tmp.path().join("s.db")).unwrap();
        assert_eq!(get::<ThemeChoice>(&c, THEME).unwrap(), None);
        set(&c, THEME, &ThemeChoice::Light).unwrap();
        set(&c, THEME, &ThemeChoice::Light).unwrap();
        assert_eq!(
            get::<ThemeChoice>(&c, THEME).unwrap(),
            Some(ThemeChoice::Light)
        );
        set(&c, AUTOSTART, &true).unwrap();
        assert_eq!(get::<bool>(&c, AUTOSTART).unwrap(), Some(true));
        // Kaputter Wert wird wie "nicht gesetzt" behandelt.
        c.execute(
            "UPDATE settings SET value = 'kaputt' WHERE key = ?1",
            [THEME],
        )
        .unwrap();
        assert_eq!(get::<ThemeChoice>(&c, THEME).unwrap(), None);
    }
}

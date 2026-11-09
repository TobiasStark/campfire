//! Einfache Einstellungen (Tabelle `settings`, Werte als JSON).

use rusqlite::{Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Serialize};

pub const THEME: &str = "ui.theme";
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
/// Dateien bis zu dieser Groesse (MB) werden automatisch geladen; 0 = nie.
pub const FILES_AUTO_MB: &str = "files.auto_mb";
/// Standard fuer [`FILES_AUTO_MB`].
pub const FILES_AUTO_MB_DEFAULT: u64 = 100;
/// Groesse des Datei-Speichers in MB, danach werden die aeltesten Dateien entfernt; 0 = unbegrenzt.
pub const FILES_CACHE_MB: &str = "files.cache_mb";
/// Standard fuer [`FILES_CACHE_MB`] (10 GB).
pub const FILES_CACHE_MB_DEFAULT: u64 = 10_240;
/// Angepinnte (und selbst geteilte) Dateien: werden nie entfernt.
pub const FILES_PINNED: &str = "files.pinned";

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

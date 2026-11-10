//! Benachrichtigungen (M2-06): Einstellungen je Space/Channel, "Bitte nicht stoeren" und die
//! Frage, welche neu eingegangenen Nachrichten einen Toast verdienen.

use crate::eventlog::unread;
use app_proto::event::{ChannelId, SpaceId};
use app_proto::PublicKey;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SETTINGS_KEY: &str = "notify.settings";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Mode {
    All,
    /// Standard: nur wenn jemand `@Name` schreibt.
    #[default]
    Mentions,
    Nothing,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::All => crate::i18n::tr("All messages", "Alle Nachrichten"),
            Mode::Mentions => crate::i18n::tr("Only mentions", "Nur Erwähnungen"),
            Mode::Nothing => crate::i18n::tr("Nothing", "Nichts"),
        }
    }

    /// Naechste Stufe beim Durchschalten.
    pub fn next(self) -> Mode {
        match self {
            Mode::All => Mode::Mentions,
            Mode::Mentions => Mode::Nothing,
            Mode::Nothing => Mode::All,
        }
    }
}

fn hex(id: &[u8]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifySettings {
    /// "Bitte nicht stoeren": unterdrueckt alle Toasts.
    pub dnd: bool,
    spaces: BTreeMap<String, Mode>,
    channels: BTreeMap<String, Mode>,
}

impl NotifySettings {
    /// Wirksamer Modus: Channel vor Space vor Standard.
    pub fn mode(&self, space: &SpaceId, channel: &ChannelId) -> Mode {
        self.mode_for(space, channel, false)
    }

    /// Wie `mode`; Direktnachrichten melden standardmaessig alles.
    pub fn mode_for(&self, space: &SpaceId, channel: &ChannelId, dm: bool) -> Mode {
        self.channels
            .get(&hex(channel))
            .or_else(|| self.spaces.get(&hex(space)))
            .copied()
            .unwrap_or(if dm { Mode::All } else { Mode::Mentions })
    }

    pub fn space_mode(&self, space: &SpaceId) -> Mode {
        self.spaces.get(&hex(space)).copied().unwrap_or_default()
    }

    pub fn set_channel(&mut self, channel: &ChannelId, mode: Mode) {
        self.channels.insert(hex(channel), mode);
    }

    /// Setzt den Space-Modus und verwirft Channel-Ausnahmen nicht (sie gelten weiter).
    pub fn set_space(&mut self, space: &SpaceId, mode: Mode) {
        self.spaces.insert(hex(space), mode);
    }

    pub fn allows(&self, space: &SpaceId, channel: &ChannelId, mention: bool, dm: bool) -> bool {
        if self.dnd {
            return false;
        }
        match self.mode_for(space, channel, dm) {
            Mode::All => true,
            Mode::Mentions => mention,
            Mode::Nothing => false,
        }
    }
}

pub fn load(conn: &Connection) -> NotifySettings {
    crate::settings::get(conn, SETTINGS_KEY)
        .ok()
        .flatten()
        .unwrap_or_default()
}

pub fn save(conn: &Connection, s: &NotifySettings) -> rusqlite::Result<()> {
    crate::settings::set(conn, SETTINGS_KEY, s)
}

/// Eine neu eingegangene, ungelesene Nachricht fremder Autoren.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incoming {
    pub space: SpaceId,
    pub space_name: String,
    pub channel: ChannelId,
    pub channel_name: String,
    pub author_name: String,
    pub body: String,
    pub mention: bool,
    /// Direktnachricht (Space mit dem reservierten Namen).
    pub dm: bool,
}

/// Aktueller Stand der Eingangsreihenfolge (Wasserzeichen fuer `incoming`).
pub fn watermark(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COALESCE(max(rowid), 0) FROM events", [], |r| {
        r.get(0)
    })
}

/// Ungelesene fremde Nachrichten, die nach dem Wasserzeichen im Log gelandet sind. Das
/// Wasserzeichen ist die Eingangsreihenfolge, nicht die HLC-Zeit: Nachrichten, die ein Freund
/// offline geschrieben hat, melden sich so auch dann, wenn sie aelter wirken.
/// Gibt die Nachrichten (aelteste zuerst) und das neue Wasserzeichen zurueck.
pub fn incoming(
    conn: &Connection,
    me: &PublicKey,
    my_name: &str,
    since: i64,
) -> rusqlite::Result<(Vec<Incoming>, i64)> {
    let mark = watermark(conn)?;
    if mark <= since {
        return Ok((Vec::new(), since.max(mark)));
    }
    let mut st = conn.prepare_cached(
        "SELECT m.space_id, m.channel_id, m.body, s.name, c.name, COALESCE(f.name, 'Unbekannt'),
                (SELECT count(*) FROM members n WHERE n.space_id = m.space_id)
         FROM events e
         JOIN messages m ON m.event_id = e.event_id
         JOIN spaces s ON s.space_id = m.space_id
         JOIN members mine ON mine.space_id = m.space_id AND mine.user_pubkey = ?3
         JOIN shown_spaces v ON v.space_id = m.space_id
         JOIN channels c ON c.space_id = m.space_id AND c.channel_id = m.channel_id
         LEFT JOIN friends f ON f.user_pubkey = m.author
         LEFT JOIN read_state r ON r.channel_id = m.channel_id
         WHERE e.rowid > ?1 AND e.rowid <= ?2 AND m.author != ?3 AND m.deleted = 0
           AND m.author NOT IN (SELECT user_pubkey FROM friends WHERE status = 'blocked')
           AND m.hlc_wall > COALESCE(r.last_read_hlc, 0)
         ORDER BY e.rowid",
    )?;
    let out = st
        .query_map(params![since, mark, me], |r| {
            let body: String = r.get(2)?;
            Ok(Incoming {
                space: r.get(0)?,
                channel: r.get(1)?,
                mention: unread::mentions(&body, my_name),
                body,
                // Als DM gilt nur ein Space mit dem reservierten Namen und genau zwei
                // Mitgliedern (wie `dm_index`); sonst koennte ein Freund einen unsichtbaren
                // "DM"-Space mit Standard "alle Nachrichten" anlegen (N-11).
                dm: crate::eventlog::dm::is_dm(&r.get::<_, String>(3)?) && r.get::<_, i64>(6)? == 2,
                space_name: r.get(3)?,
                channel_name: r.get(4)?,
                author_name: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok((out, mark))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::open_connection;

    const SP: SpaceId = [1; 16];
    const CH: ChannelId = [2; 16];
    const ME: PublicKey = [9; 32];
    const OTHER: PublicKey = [5; 32];

    #[test]
    fn channel_beats_space_beats_default_and_dnd_wins() {
        let mut s = NotifySettings::default();
        assert_eq!(s.mode(&SP, &CH), Mode::Mentions);
        assert!(s.allows(&SP, &CH, true, false) && !s.allows(&SP, &CH, false, false));
        // Direktnachrichten melden standardmaessig alles, solange nichts eingestellt ist.
        assert!(s.allows(&SP, &CH, false, true));
        s.set_space(&SP, Mode::All);
        assert!(s.allows(&SP, &CH, false, false));
        s.set_channel(&CH, Mode::Nothing);
        assert!(!s.allows(&SP, &CH, true, false) && !s.allows(&SP, &CH, true, true));
        assert!(s.allows(&SP, &[3; 16], false, false));
        s.set_channel(&CH, Mode::All);
        s.dnd = true;
        assert!(!s.allows(&SP, &CH, true, false));
        assert_eq!(Mode::All.next().next().next(), Mode::All);
    }

    #[test]
    fn settings_survive_reopening() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("n.db");
        {
            let c = open_connection(&path).unwrap();
            let mut s = NotifySettings::default();
            s.set_channel(&CH, Mode::Nothing);
            s.set_space(&SP, Mode::All);
            s.dnd = true;
            save(&c, &s).unwrap();
        }
        let s = load(&open_connection(&path).unwrap());
        assert!(s.dnd);
        assert_eq!(s.mode(&SP, &CH), Mode::Nothing);
        assert_eq!(s.space_mode(&SP), Mode::All);
    }

    fn add(c: &Connection, n: u8, author: PublicKey, wall: i64, body: &str) {
        c.execute(
            "INSERT INTO events (event_id, space_id, author, seq, hlc_wall, hlc_counter, kind, payload, signature, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 'message', x'', x'', 0)",
            params![[n; 32], SP, author, n as i64, wall],
        )
        .unwrap();
        c.execute(
            "INSERT INTO messages (space_id, channel_id, event_id, author, hlc_wall, hlc_counter, body)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
            params![SP, CH, [n; 32], author, wall, body],
        )
        .unwrap();
    }

    #[test]
    fn incoming_reports_only_new_foreign_unread_messages_in_arrival_order() {
        let t = tempfile::tempdir().unwrap();
        let c = open_connection(&t.path().join("i.db")).unwrap();
        c.execute(
            "INSERT INTO spaces (space_id, name, joined_at) VALUES (?1, 'Freunde', 0)",
            [SP],
        )
        .unwrap();
        c.execute(
            "INSERT INTO channels (space_id, channel_id, kind, name) VALUES (?1, ?2, 'text', 'allgemein')",
            params![SP, CH],
        )
        .unwrap();
        c.execute(
            "INSERT INTO friends (user_pubkey, name, status) VALUES (?1, 'Anna', 'accepted')",
            [OTHER],
        )
        .unwrap();
        add(&c, 1, OTHER, 100, "alt");
        let (none, mark) = incoming(&c, &ME, "tobi", watermark(&c).unwrap()).unwrap();
        assert!(none.is_empty());
        add(&c, 2, ME, 300, "eigene");
        // Spaeter eingetroffen, aber mit aelterer HLC-Zeit (offline geschrieben).
        add(&c, 3, OTHER, 50, "hey @Tobi");
        add(&c, 4, OTHER, 400, "gelesen");
        crate::eventlog::unread::mark_read(&c, &CH, 400).unwrap();
        add(&c, 5, OTHER, 500, "hallo");
        // Solange ich nicht Mitglied des Spaces bin (fremd zugeschickt), meldet sich nichts.
        assert!(incoming(&c, &ME, "tobi", mark).unwrap().0.is_empty());
        c.execute(
            "INSERT INTO members (space_id, user_pubkey, added_by) VALUES (?1, ?2, ?2)",
            params![SP, ME],
        )
        .unwrap();
        // Mitglied, aber noch nicht angenommen (M-2): ebenfalls still.
        assert!(incoming(&c, &ME, "tobi", mark).unwrap().0.is_empty());
        crate::eventlog::consent::accept(&c, &SP).unwrap();
        let (list, mark2) = incoming(&c, &ME, "tobi", mark).unwrap();
        assert!(mark2 > mark);
        // Nachricht 3 (HLC 50) ist durch den Lesestand 400 bereits "gelesen" -> kein Toast.
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].body, "hallo");
        assert_eq!(
            (list[0].author_name.as_str(), list[0].channel_name.as_str()),
            ("Anna", "allgemein")
        );
        assert!(!list[0].mention);
        assert!(!list[0].dm);
        assert!(incoming(&c, &ME, "tobi", mark2).unwrap().0.is_empty());

        // Reservierter DM-Name: DM nur mit genau zwei Mitgliedern (N-11).
        c.execute(
            "UPDATE spaces SET name = ?2 WHERE space_id = ?1",
            params![SP, crate::eventlog::dm::DM_NAME],
        )
        .unwrap();
        c.execute(
            "INSERT INTO members (space_id, user_pubkey, added_by) VALUES (?1, ?2, ?2)",
            params![SP, OTHER],
        )
        .unwrap();
        add(&c, 6, OTHER, 600, "dm");
        let (list, mark3) = incoming(&c, &ME, "tobi", mark2).unwrap();
        assert!(list[0].dm);
        c.execute(
            "INSERT INTO members (space_id, user_pubkey, added_by) VALUES (?1, ?2, ?2)",
            params![SP, [6u8; 32]],
        )
        .unwrap();
        add(&c, 7, OTHER, 700, "fake dm");
        let (list, mark4) = incoming(&c, &ME, "tobi", mark3).unwrap();
        assert!(!list[0].dm, "drei Mitglieder sind keine DM");
        // Blockierte melden sich nicht mehr (M-2).
        c.execute(
            "UPDATE friends SET status = 'blocked' WHERE user_pubkey = ?1",
            params![OTHER],
        )
        .unwrap();
        add(&c, 8, OTHER, 800, "immer noch da");
        assert!(incoming(&c, &ME, "tobi", mark4).unwrap().0.is_empty());
    }
}

//! Space-Einladungen erst nach Zustimmung (M-2).
//!
//! Ein `MemberAdd` macht mich sofort zum Mitglied: das Log braucht das, um Events anderer
//! Mitglieder zu pruefen und weiterzureichen. Angezeigt, gemeldet und automatisch geladen wird
//! ein Space aber erst, wenn ich zugestimmt habe (`accepted_spaces`). Eigene Spaces und
//! Direktnachrichten mit Freunden gelten als angenommen. Wer nicht mehr Mitglied ist, verliert
//! die Zustimmung, eine erneute Einladung fragt also wieder. Spaces, deren Gruender ich blockiert
//! habe, bleiben ausgeblendet (View `shown_spaces`), Nachrichten blockierter Nutzer ebenso.

use super::{dm, head, list_spaces, project, SpaceInfo};
use app_proto::event::SpaceId;
use app_proto::PublicKey;
use rusqlite::{params, Connection};
use std::collections::HashSet;

fn users_with_status(conn: &Connection, status: &str) -> rusqlite::Result<HashSet<PublicKey>> {
    conn.prepare_cached("SELECT user_pubkey FROM friends WHERE status = ?1")?
        .query_map([status], |r| r.get(0))?
        .collect()
}

/// Nutzer, die ich blockiert habe.
pub fn blocked_users(conn: &Connection) -> rusqlite::Result<HashSet<PublicKey>> {
    users_with_status(conn, "blocked")
}

/// Gleicht die Zustimmungen mit der Projektion ab: eigene Spaces, Spaces mit eigenen Events
/// (etwa nach einer Wiederherstellung) und DMs mit Freunden annehmen, Spaces ohne meine
/// Mitgliedschaft (und DMs, die keine mehr sind) vergessen und ihre Daten loeschen.
pub fn sync(conn: &Connection, me: &PublicKey) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO accepted_spaces (space_id)
         SELECT space_id FROM members WHERE user_pubkey = ?1 AND role = 'admin'",
        [me],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO accepted_spaces (space_id)
         SELECT DISTINCT e.space_id FROM events e
           JOIN members m ON m.space_id = e.space_id AND m.user_pubkey = ?1
          WHERE e.author = ?1 AND e.kind != 'MemberRemove'",
        [me],
    )?;
    purge_left(conn, me)?;
    conn.execute(
        "DELETE FROM accepted_spaces
         WHERE space_id NOT IN (SELECT space_id FROM members WHERE user_pubkey = ?1)",
        [me],
    )?;
    let friends = users_with_status(conn, "accepted")?;
    let dms: HashSet<SpaceId> = dm::dm_index(conn, me)?
        .into_iter()
        .filter(|(peer, _)| friends.contains(peer))
        .map(|(_, (space, _))| space)
        .collect();
    let auto: Vec<SpaceId> = conn
        .prepare_cached("SELECT space_id FROM accepted_spaces WHERE via_dm = 1")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for s in auto.iter().filter(|s| !dms.contains(*s)) {
        conn.execute("DELETE FROM accepted_spaces WHERE space_id = ?1", [s])?;
    }
    for s in &dms {
        conn.execute(
            "INSERT OR IGNORE INTO accepted_spaces (space_id, via_dm) VALUES (?1, 1)",
            [s],
        )?;
    }
    Ok(())
}

/// Loescht Spaces, in denen ich nicht (mehr) Mitglied bin, samt Event-Log (M-3): ausgetreten,
/// entfernt oder nie beigetreten. Nach einem eigenen Austritt erst, wenn ein anderes Mitglied
/// ihn bestaetigt hat, sonst erfuehre niemand davon. Spaces, deren Einladung gerade
/// nachgeladen wird, bleiben.
pub fn purge_left(conn: &Connection, me: &PublicKey) -> rusqlite::Result<Vec<SpaceId>> {
    let candidates: Vec<SpaceId> = conn
        .prepare_cached(
            "SELECT DISTINCT space_id FROM author_heads
              WHERE space_id NOT IN (SELECT space_id FROM members WHERE user_pubkey = ?1)
                AND space_id NOT IN (
                  SELECT t.space_id FROM space_tickets t
                   WHERE NOT EXISTS (SELECT 1 FROM events e WHERE e.event_id = t.add_event))",
        )?
        .query_map([me], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for space in candidates {
        let mine = head(conn, &space, me)?;
        if mine > 0 {
            let left: bool = conn
                .prepare_cached(
                    "SELECT kind = 'MemberRemove' AND subject = ?2 FROM events
                      WHERE space_id = ?1 AND author = ?2 AND seq = ?3",
                )?
                .query_row(params![space, me, mine as i64], |r| r.get(0))
                .unwrap_or(false);
            let delivered = conn
                .prepare_cached(
                    "SELECT 1 FROM peer_acks
                      WHERE space_id = ?1 AND author = ?2 AND seq >= ?3 LIMIT 1",
                )?
                .exists(params![space, me, mine as i64])?;
            if left && !delivered {
                continue;
            }
        }
        purge_space(conn, &space)?;
        out.push(space);
    }
    Ok(out)
}

/// Loescht alle Daten eines Spaces (Log, Projektion, Zustimmung, Dateifreigaben).
fn purge_space(conn: &Connection, space: &SpaceId) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM read_state WHERE channel_id IN (SELECT channel_id FROM channels WHERE space_id = ?1)",
        [space],
    )?;
    project::reset(conn, space)?;
    for sql in [
        "DELETE FROM events WHERE space_id = ?1",
        "DELETE FROM author_heads WHERE space_id = ?1",
        "DELETE FROM peer_acks WHERE space_id = ?1",
        "DELETE FROM spaces WHERE space_id = ?1",
        "DELETE FROM accepted_spaces WHERE space_id = ?1",
        "DELETE FROM space_tickets WHERE space_id = ?1",
        "DELETE FROM blob_origin WHERE space_id = ?1",
        "DELETE FROM equivocations WHERE space_id = ?1",
    ] {
        conn.execute(sql, [space])?;
    }
    Ok(())
}

/// Einladung annehmen.
pub fn accept(conn: &Connection, space: &SpaceId) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO accepted_spaces (space_id, via_dm) VALUES (?1, 0)
         ON CONFLICT (space_id) DO UPDATE SET via_dm = 0",
        params![space],
    )?;
    Ok(())
}

/// Spaces fuer die Oberflaeche.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SpaceLists {
    /// Angenommen und Gruender nicht blockiert.
    pub shown: Vec<SpaceInfo>,
    /// Offene Einladungen von Freunden. Einladungen von Fremden (etwa ueber einen
    /// gemeinsamen Freund) und Spaces mit dem reservierten DM-Namen erscheinen gar nicht.
    pub invites: Vec<SpaceInfo>,
}

/// Gleicht ab (`sync`) und teilt meine Spaces in angezeigte und offene Einladungen.
pub fn space_lists(conn: &Connection, me: &PublicKey) -> rusqlite::Result<SpaceLists> {
    sync(conn, me)?;
    let shown: HashSet<SpaceId> = conn
        .prepare_cached("SELECT space_id FROM shown_spaces")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let accepted: HashSet<SpaceId> = conn
        .prepare_cached("SELECT space_id FROM accepted_spaces")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let friends = users_with_status(conn, "accepted")?;
    let mut out = SpaceLists::default();
    for s in list_spaces(conn, me)? {
        if shown.contains(&s.space_id) {
            out.shown.push(s);
        } else if !accepted.contains(&s.space_id)
            && s.creator.is_some_and(|c| friends.contains(&c))
            && !dm::is_dm(&s.name)
        {
            out.invites.push(s);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::open_connection;

    const ME: PublicKey = [9; 32];
    const ANNA: PublicKey = [5; 32];
    const EVE: PublicKey = [6; 32];

    fn space(c: &Connection, id: u8, name: &str, admin: PublicKey, members: &[PublicKey]) {
        c.execute(
            "INSERT INTO spaces (space_id, name, joined_at) VALUES (?1, ?2, 0)",
            params![[id; 16], name],
        )
        .unwrap();
        c.execute(
            "INSERT INTO members (space_id, user_pubkey, added_by, role) VALUES (?1, ?2, ?2, 'admin')",
            params![[id; 16], admin],
        )
        .unwrap();
        for m in members {
            c.execute(
                "INSERT INTO members (space_id, user_pubkey, added_by) VALUES (?1, ?2, ?3)",
                params![[id; 16], m, admin],
            )
            .unwrap();
        }
    }

    fn friend(c: &Connection, user: PublicKey, status: &str) {
        c.execute(
            "INSERT INTO friends (user_pubkey, name, status) VALUES (?1, 'x', ?2)
             ON CONFLICT (user_pubkey) DO UPDATE SET status = ?2",
            params![user, status],
        )
        .unwrap();
    }

    fn ids(v: &[SpaceInfo]) -> Vec<u8> {
        v.iter().map(|s| s.space_id[0]).collect()
    }

    #[test]
    fn invitations_need_consent_and_blocked_creators_disappear() {
        let t = tempfile::tempdir().unwrap();
        let c = open_connection(&t.path().join("c.db")).unwrap();
        friend(&c, ANNA, "accepted");
        space(&c, 1, "Meins", ME, &[ANNA]);
        space(&c, 2, "Von Anna", ANNA, &[ME]);
        space(&c, 3, "Von Eve", EVE, &[ME, ANNA]);
        space(&c, 4, dm::DM_NAME, ANNA, &[ME]);
        space(&c, 5, dm::DM_NAME, EVE, &[ME]);
        let l = space_lists(&c, &ME).unwrap();
        // Eigener Space und DM mit Anna sofort, Annas Space als Einladung, Eve gar nicht.
        assert_eq!(ids(&l.shown), vec![1, 4]);
        assert_eq!(ids(&l.invites), vec![2]);
        assert_eq!(l.invites[0].creator, Some(ANNA));

        accept(&c, &[2; 16]).unwrap();
        assert_eq!(ids(&space_lists(&c, &ME).unwrap().shown), vec![1, 2, 4]);

        // Austritt (nicht mehr Mitglied): die Zustimmung verfaellt.
        c.execute(
            "DELETE FROM members WHERE space_id = ?1 AND user_pubkey = ?2",
            params![[2u8; 16], ME],
        )
        .unwrap();
        sync(&c, &ME).unwrap();
        c.execute(
            "INSERT INTO members (space_id, user_pubkey, added_by) VALUES (?1, ?2, ?3)",
            params![[2u8; 16], ME, ANNA],
        )
        .unwrap();
        assert_eq!(ids(&space_lists(&c, &ME).unwrap().invites), vec![2]);
        accept(&c, &[2; 16]).unwrap();

        // Blockiert: Annas Spaces verschwinden, auch die angenommenen und die DM.
        friend(&c, ANNA, "blocked");
        let l = space_lists(&c, &ME).unwrap();
        assert_eq!(ids(&l.shown), vec![1]);
        assert!(l.invites.is_empty());
        assert_eq!(blocked_users(&c).unwrap(), [ANNA].into_iter().collect());
    }
}

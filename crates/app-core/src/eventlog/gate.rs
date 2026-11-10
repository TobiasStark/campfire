//! Annahme fremder Events (M-3).
//!
//! - Nur Spaces, in denen ich Mitglied bin oder fuer die ich eine vom Gruender signierte
//!   Einladung habe (`Invite` in den Heads, gemerkt in `space_tickets`), solange die Einladung
//!   selbst noch nicht im Log liegt. Ein Freund kann mir so keine Spaces aufdraengen, in die er
//!   mich nie eingeladen hat, und nach einem Austritt kommt nichts mehr nach.
//! - Noch nicht angenommene Spaces (M-2) nur bis `pending_cap` Events; der Rest kommt nach der
//!   Zustimmung. Eigene Events zaehlen nicht (Wiederherstellung aus dem Backup).
//! - Je Autor und Space hoechstens `AUTHOR_EVENTS_PER_HOUR` Events pro Stunde Empfangszeit.
//!   Mehr wird abgelehnt und beim naechsten Abgleich erneut angefragt.

use super::{derive_space_id, row_to_envelope, verify, wants_from, Reject, SELECT_ENV};
use app_proto::event::{EventEnvelope, EventKind, SpaceId};
use app_proto::sync::{Invite, SpaceHeads, Want};
use app_proto::PublicKey;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;

/// Events eines noch nicht angenommenen Spaces, bevor die Annahme pausiert.
pub const PENDING_SPACE_EVENTS: i64 = 5000;
/// Events je Autor und Space pro Stunde Empfangszeit.
pub const AUTHOR_EVENTS_PER_HOUR: i64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admit {
    No,
    /// `pending_left`: wie viele fremde Events noch angenommen werden, solange der Space nicht
    /// angenommen ist (`None` = keine Grenze). `only`: waehrend die Einladung nachgeladen wird,
    /// nur die Events des Gruenders (bis einschliesslich der Einladung); alles andere folgt,
    /// sobald ich Mitglied bin.
    Yes {
        pending_left: Option<i64>,
        only: Option<PublicKey>,
    },
}

pub(super) fn admission(
    conn: &Connection,
    me: &PublicKey,
    space: &SpaceId,
    cap: i64,
) -> rusqlite::Result<Admit> {
    let member = conn
        .prepare_cached("SELECT 1 FROM members WHERE space_id = ?1 AND user_pubkey = ?2")?
        .exists(params![space, me])?;
    if !member {
        let creator: Option<PublicKey> = conn
            .prepare_cached(
                "SELECT creator FROM space_tickets t WHERE t.space_id = ?1
                   AND NOT EXISTS (SELECT 1 FROM events e WHERE e.event_id = t.add_event)",
            )?
            .query_row([space], |r| r.get(0))
            .optional()?;
        return Ok(match creator {
            Some(c) => Admit::Yes {
                pending_left: None,
                only: Some(c),
            },
            None => Admit::No,
        });
    }
    let accepted: bool = conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM accepted_spaces WHERE space_id = ?1)
                 OR EXISTS (SELECT 1 FROM members
                             WHERE space_id = ?1 AND user_pubkey = ?2 AND role = 'admin')
                 OR EXISTS (SELECT 1 FROM events
                             WHERE space_id = ?1 AND author = ?2 AND kind != 'MemberRemove')",
        )?
        .query_row(params![space, me], |r| r.get(0))?;
    if accepted {
        return Ok(Admit::Yes {
            pending_left: None,
            only: None,
        });
    }
    let n: i64 = conn
        .prepare_cached("SELECT count(*) FROM events WHERE space_id = ?1")?
        .query_row([space], |r| r.get(0))?;
    Ok(Admit::Yes {
        pending_left: Some(cap - n),
        only: None,
    })
}

/// Entscheidungen eines Stapels. Je Space wird einmal entschieden (die Projektion zieht erst
/// nach dem Stapel nach), je Autor einmal gezaehlt und dann mitgezaehlt.
#[derive(Default)]
pub(super) struct Gate {
    spaces: HashMap<SpaceId, Admit>,
    authors: HashMap<(SpaceId, PublicKey), i64>,
}

impl Gate {
    pub(super) fn admit(
        &mut self,
        conn: &Connection,
        me: &PublicKey,
        cap: i64,
        env: &EventEnvelope,
        now_s: i64,
    ) -> rusqlite::Result<Result<(), Reject>> {
        let b = &env.body;
        let a = match self.spaces.get(&b.space_id) {
            Some(a) => *a,
            None => {
                let a = admission(conn, me, &b.space_id, cap)?;
                self.spaces.insert(b.space_id, a);
                a
            }
        };
        match a {
            Admit::No => return Ok(Err(Reject::NotMember)),
            Admit::Yes { .. } if b.author == *me => return Ok(Ok(())),
            Admit::Yes { only: Some(c), .. } if b.author != c => return Ok(Err(Reject::NotMember)),
            Admit::Yes {
                pending_left: Some(n),
                ..
            } if n <= 0 => return Ok(Err(Reject::Quota)),
            Admit::Yes { .. } => {}
        }
        let key = (b.space_id, b.author);
        let used = match self.authors.get(&key) {
            Some(n) => *n,
            None => {
                let n: i64 = conn
                    .prepare_cached(
                        "SELECT count(*) FROM events
                         WHERE space_id = ?1 AND author = ?2 AND received_at > ?3",
                    )?
                    .query_row(params![b.space_id, b.author, now_s - 3600], |r| r.get(0))?;
                self.authors.insert(key, n);
                n
            }
        };
        if used >= AUTHOR_EVENTS_PER_HOUR {
            return Ok(Err(Reject::Quota));
        }
        Ok(Ok(()))
    }

    /// Ein zugelassenes Event wurde gespeichert.
    pub(super) fn stored(&mut self, env: &EventEnvelope) {
        let b = &env.body;
        if let Some(Admit::Yes {
            pending_left: Some(n),
            ..
        }) = self.spaces.get_mut(&b.space_id)
        {
            *n -= 1;
        }
        if let Some(n) = self.authors.get_mut(&(b.space_id, b.author)) {
            *n += 1;
        }
    }
}

/// Einladung von `peer` in `space` fuer die Heads: Gruendung und juengstes `MemberAdd` des
/// Gruenders fuer `peer`. `None` fuer den Gruender selbst.
pub(super) fn invite_for(
    conn: &Connection,
    space: &SpaceId,
    peer: &PublicKey,
) -> rusqlite::Result<Option<Box<Invite>>> {
    let create = conn
        .prepare_cached(&format!(
            "{SELECT_ENV} WHERE space_id = ?1 AND kind = 'SpaceCreate' LIMIT 1"
        ))?
        .query_row([space], row_to_envelope)
        .optional()?;
    let Some(create) = create else {
        return Ok(None);
    };
    let add = conn
        .prepare_cached(&format!(
            "{SELECT_ENV} WHERE space_id = ?1 AND kind = 'MemberAdd' AND subject = ?2
               AND author = ?3
             ORDER BY hlc_wall DESC, hlc_counter DESC, event_id DESC LIMIT 1"
        ))?
        .query_row(params![space, peer, create.body.author], row_to_envelope)
        .optional()?;
    Ok(add.map(|add| Box::new(Invite { create, add })))
}

/// Prueft eine Einladung fuer `me` und merkt sie sich (die juengste je Space).
fn remember_invite(
    conn: &Connection,
    me: &PublicKey,
    space: &SpaceId,
    inv: &Invite,
    now_ms: u64,
) -> rusqlite::Result<()> {
    let (c, a) = (&inv.create.body, &inv.add.body);
    let valid = verify(&inv.create, now_ms).is_ok()
        && verify(&inv.add, now_ms).is_ok()
        && matches!(c.kind, EventKind::SpaceCreate { .. })
        && c.space_id == *space
        && derive_space_id(&c.author, &c.hlc) == *space
        && a.space_id == *space
        && a.author == c.author
        && matches!(a.kind, EventKind::MemberAdd { user } if user == *me);
    if !valid {
        return Ok(());
    }
    conn.prepare_cached(
        "INSERT INTO space_tickets (space_id, add_event, add_wall, add_counter, creator)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (space_id) DO UPDATE SET
           add_event = excluded.add_event, add_wall = excluded.add_wall,
           add_counter = excluded.add_counter, creator = excluded.creator
         WHERE (excluded.add_wall, excluded.add_counter)
               > (space_tickets.add_wall, space_tickets.add_counter)",
    )?
    .execute(params![
        space,
        inv.add.id(),
        a.hlc.wall_ms as i64,
        a.hlc.counter as i64,
        c.author
    ])?;
    Ok(())
}

/// Verarbeitet die Heads eines Peers fuer `me`: gueltige Einladungen merken und nur anfordern,
/// was der Gate auch annehmen wuerde (spart Bandbreite bei fremden und pausierten Spaces).
pub fn wants_for(
    conn: &Connection,
    me: &PublicKey,
    remote: &[SpaceHeads],
    now_ms: u64,
    cap: i64,
) -> rusqlite::Result<Vec<Want>> {
    for sh in remote {
        if let Some(inv) = &sh.invite {
            remember_invite(conn, me, &sh.space_id, inv, now_ms)?;
        }
    }
    let mut seen: HashMap<SpaceId, Admit> = HashMap::new();
    let mut out = Vec::new();
    for w in wants_from(conn, remote)? {
        let a = match seen.get(&w.space_id) {
            Some(a) => *a,
            None => {
                let a = admission(conn, me, &w.space_id, cap)?;
                seen.insert(w.space_id, a);
                a
            }
        };
        match a {
            Admit::No => {}
            Admit::Yes { .. } if w.author == *me => out.push(w),
            Admit::Yes { only: Some(c), .. } if w.author != c => {}
            Admit::Yes {
                pending_left: Some(n),
                ..
            } if n <= 0 => {}
            Admit::Yes { .. } => out.push(w),
        }
    }
    Ok(out)
}

//! Freunde und Hello-Outbox in SQLite (Funktionen laufen auf dem DB-Thread).

use app_net::Access;
use app_proto::control::DeviceCert;
use app_proto::friend_code::FriendCode;
use app_proto::PublicKey;
use data_encoding::HEXLOWER;
use rusqlite::{Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FriendStatus {
    PendingIn,
    PendingOut,
    Accepted,
    Blocked,
}

impl FriendStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PendingIn => "pending_in",
            Self::PendingOut => "pending_out",
            Self::Accepted => "accepted",
            Self::Blocked => "blocked",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        Ok(match s {
            "pending_in" => Self::PendingIn,
            "pending_out" => Self::PendingOut,
            "accepted" => Self::Accepted,
            "blocked" => Self::Blocked,
            other => {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    format!("unbekannter Status {other}").into(),
                ))
            }
        })
    }
}

/// Adresshinweis aus dem Freundescode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddrHint {
    pub node_id: Option<String>,
    pub relay_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Friend {
    pub user: PublicKey,
    pub name: String,
    pub status: FriendStatus,
    pub note: Option<String>,
    pub devices: Vec<DeviceCert>,
    pub hint: AddrHint,
    pub last_seen: Option<i64>,
}

impl Friend {
    /// Bekannte Node-IDs: Geraete aus Zertifikaten plus Hinweis aus dem Code.
    pub fn node_ids(&self) -> Vec<PublicKey> {
        let mut v: Vec<PublicKey> = self.devices.iter().map(|d| d.device).collect();
        if let Some(n) = self.hint.node_id.as_deref().and_then(parse_key) {
            if !v.contains(&n) {
                v.push(n);
            }
        }
        v
    }
}

pub fn parse_key(hex: &str) -> Option<PublicKey> {
    HEXLOWER.decode(hex.as_bytes()).ok()?.try_into().ok()
}

fn encode_devices(devs: &[DeviceCert]) -> String {
    let v: Vec<String> = devs
        .iter()
        .filter_map(|d| app_proto::encode(d).ok())
        .map(|b| HEXLOWER.encode(&b))
        .collect();
    serde_json::to_string(&v).unwrap_or_else(|_| "[]".into())
}

fn decode_devices(json: &str) -> Vec<DeviceCert> {
    let Ok(v) = serde_json::from_str::<Vec<String>>(json) else {
        return Vec::new();
    };
    v.iter()
        .filter_map(|h| HEXLOWER.decode(h.as_bytes()).ok())
        .filter_map(|b| app_proto::decode::<DeviceCert>(&b).ok())
        .collect()
}

const COLS: &str = "user_pubkey, name, status, note, devices, addr_hints, last_seen";

fn row_to_friend(r: &Row<'_>) -> rusqlite::Result<Friend> {
    let user: Vec<u8> = r.get(0)?;
    let status: String = r.get(2)?;
    let devices: String = r.get(4)?;
    let hint: Option<String> = r.get(5)?;
    Ok(Friend {
        user: user.as_slice().try_into().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Blob,
                "Schluessellaenge".into(),
            )
        })?,
        name: r.get(1)?,
        status: FriendStatus::parse(&status)?,
        note: r.get(3)?,
        devices: decode_devices(&devices),
        hint: hint
            .and_then(|h| serde_json::from_str(&h).ok())
            .unwrap_or_default(),
        last_seen: r.get(6)?,
    })
}

pub fn list(conn: &Connection) -> rusqlite::Result<Vec<Friend>> {
    let mut st = conn.prepare(&format!("SELECT {COLS} FROM friends ORDER BY name"))?;
    let rows = st.query_map([], row_to_friend)?;
    rows.collect()
}

pub fn get(conn: &Connection, user: &PublicKey) -> rusqlite::Result<Option<Friend>> {
    conn.query_row(
        &format!("SELECT {COLS} FROM friends WHERE user_pubkey = ?1"),
        [user.as_slice()],
        row_to_friend,
    )
    .optional()
}

pub fn count(conn: &Connection, status: FriendStatus) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM friends WHERE status = ?1",
        [status.as_str()],
        |r| r.get(0),
    )
}

/// Zugriffstabelle fuer die Allowlist auf `app/peer/1`.
pub fn access_map(conn: &Connection) -> rusqlite::Result<HashMap<PublicKey, Access>> {
    let mut m = HashMap::new();
    for f in list(conn)? {
        match f.status {
            FriendStatus::Accepted => {
                m.insert(f.user, Access::Allowed);
            }
            FriendStatus::Blocked => {
                m.insert(f.user, Access::Blocked);
            }
            _ => {}
        }
    }
    Ok(m)
}

pub fn short_name(user: &PublicKey) -> String {
    format!("?{}", &HEXLOWER.encode(user)[..8])
}

/// Legt eine ausgehende Anfrage an (oder aktualisiert Hinweis/Notiz).
pub fn upsert_pending_out(
    conn: &Connection,
    code: &FriendCode,
    note: &str,
) -> rusqlite::Result<()> {
    let hint = serde_json::to_string(&AddrHint {
        node_id: Some(HEXLOWER.encode(&code.node_id)),
        relay_url: code.relay_url.clone(),
    })
    .unwrap_or_default();
    conn.execute(
        "INSERT INTO friends (user_pubkey, name, status, note, devices, addr_hints)
         VALUES (?1, ?2, 'pending_out', ?3, '[]', ?4)
         ON CONFLICT(user_pubkey) DO UPDATE SET note = ?3, addr_hints = ?4
         WHERE status = 'pending_out'",
        (code.user.as_slice(), short_name(&code.user), note, hint),
    )?;
    Ok(())
}

pub enum InResult {
    Stored,
    Full,
}

/// Speichert eine eingehende Anfrage; ueber `cap` ausstehenden Anfragen wird abgelehnt.
pub fn upsert_pending_in(
    conn: &Connection,
    cert: &DeviceCert,
    name: &str,
    note: &str,
    cap: i64,
) -> rusqlite::Result<InResult> {
    if count(conn, FriendStatus::PendingIn)? >= cap {
        return Ok(InResult::Full);
    }
    conn.execute(
        "INSERT INTO friends (user_pubkey, name, status, note, devices)
         VALUES (?1, ?2, 'pending_in', ?3, ?4)",
        (
            cert.user.as_slice(),
            name,
            note,
            encode_devices(std::slice::from_ref(cert)),
        ),
    )?;
    Ok(InResult::Stored)
}

pub fn set_status(
    conn: &Connection,
    user: &PublicKey,
    status: FriendStatus,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE friends SET status = ?2 WHERE user_pubkey = ?1",
        (user.as_slice(), status.as_str()),
    )
}

pub fn set_name_note(
    conn: &Connection,
    user: &PublicKey,
    name: &str,
    note: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE friends SET name = ?2, note = coalesce(?3, note) WHERE user_pubkey = ?1",
        (user.as_slice(), name, note),
    )?;
    Ok(())
}

/// Blockiert auch Unbekannte (legt dann eine Zeile an).
pub fn block(conn: &Connection, user: &PublicKey, name: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO friends (user_pubkey, name, status, devices) VALUES (?1, ?2, 'blocked', '[]')
         ON CONFLICT(user_pubkey) DO UPDATE SET status = 'blocked'",
        (user.as_slice(), name),
    )?;
    Ok(())
}

pub fn delete(conn: &Connection, user: &PublicKey) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM friends WHERE user_pubkey = ?1",
        [user.as_slice()],
    )?;
    Ok(())
}

pub fn add_device(conn: &Connection, user: &PublicKey, cert: &DeviceCert) -> rusqlite::Result<()> {
    let Some(mut f) = get(conn, user)? else {
        return Ok(());
    };
    f.devices.retain(|d| d.device != cert.device);
    f.devices.push(cert.clone());
    // Mehr als 16 Geraete pro Nutzer merken wir uns nicht.
    if f.devices.len() > 16 {
        f.devices.remove(0);
    }
    conn.execute(
        "UPDATE friends SET devices = ?2 WHERE user_pubkey = ?1",
        (user.as_slice(), encode_devices(&f.devices)),
    )?;
    Ok(())
}

// ---- Outbox ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxKind {
    Request,
    Accept,
    Decline,
}

impl OutboxKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Accept => "accept",
            Self::Decline => "decline",
        }
    }
}

#[derive(Debug, Clone)]
pub struct OutboxEntry {
    pub user: PublicKey,
    pub kind: OutboxKind,
    pub node_id: PublicKey,
    pub relay_url: Option<String>,
    pub attempts: i64,
}

pub fn outbox_put(
    conn: &Connection,
    user: &PublicKey,
    kind: OutboxKind,
    node_id: &PublicKey,
    relay_url: Option<&str>,
    now: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO hello_outbox (user_pubkey, kind, node_id, relay_url, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(user_pubkey) DO UPDATE SET kind = ?2, node_id = ?3, relay_url = ?4,
             attempts = 0, next_attempt = 0",
        (
            user.as_slice(),
            kind.as_str(),
            node_id.as_slice(),
            relay_url,
            now,
        ),
    )?;
    Ok(())
}

pub fn outbox_due(conn: &Connection, now: i64) -> rusqlite::Result<Vec<OutboxEntry>> {
    let mut st = conn.prepare(
        "SELECT user_pubkey, kind, node_id, relay_url, attempts FROM hello_outbox
         WHERE next_attempt <= ?1 ORDER BY created_at LIMIT 64",
    )?;
    let rows = st.query_map([now], |r| {
        let user: Vec<u8> = r.get(0)?;
        let kind: String = r.get(1)?;
        let node: Vec<u8> = r.get(2)?;
        Ok((
            user,
            kind,
            node,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (user, kind, node, relay_url, attempts) = row?;
        let (Ok(user), Ok(node)) = (user.try_into(), node.try_into()) else {
            continue;
        };
        let kind = match kind.as_str() {
            "request" => OutboxKind::Request,
            "accept" => OutboxKind::Accept,
            _ => OutboxKind::Decline,
        };
        out.push(OutboxEntry {
            user,
            kind,
            node_id: node,
            relay_url,
            attempts,
        });
    }
    Ok(out)
}

pub fn outbox_delete(conn: &Connection, user: &PublicKey) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM hello_outbox WHERE user_pubkey = ?1",
        [user.as_slice()],
    )?;
    Ok(())
}

/// Versuch fehlgeschlagen: exponentiell spaeter erneut (5 s, 10 s, ... max. 5 min).
pub fn outbox_failed(conn: &Connection, user: &PublicKey, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE hello_outbox SET attempts = attempts + 1,
             next_attempt = ?2 + min(300, 5 * (1 << min(attempts, 6)))
         WHERE user_pubkey = ?1",
        (user.as_slice(), now),
    )?;
    Ok(())
}

/// Alle wartenden Eintraege sofort wieder faellig machen (Netzwechsel, Start).
pub fn outbox_reset_backoff(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("UPDATE hello_outbox SET next_attempt = 0", [])?;
    Ok(())
}

pub fn outbox_len(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT count(*) FROM hello_outbox", [], |r| r.get(0))
}

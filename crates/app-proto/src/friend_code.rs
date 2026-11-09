//! Freundescode.
//!
//! Binaer: `[version u8][flags u8][user 32][node 32][relay_len u8, relay]?[check 4]`,
//! base32 (Grossbuchstaben, ohne Padding). `check` ist blake3 ueber alle
//! vorherigen Bytes und faengt Tippfehler ab (Abweichung von 6.2: Pruefsumme).
//! Der Code enthaelt keine geheimen Daten. Link-Form: `app://add/<code>`.

use crate::PublicKey;
use data_encoding::BASE32_NOPAD;

pub const VERSION: u8 = 1;
pub const LINK_PREFIX: &str = "app://add/";
const FLAG_RELAY: u8 = 1;
const CHECK_LEN: usize = 4;
const MAX_RELAY_LEN: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FriendCode {
    pub user: PublicKey,
    pub node_id: PublicKey,
    pub relay_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FriendCodeError {
    #[error("Code ist leer oder zu kurz")]
    TooShort,
    #[error("Ungueltige Zeichen im Code")]
    BadEncoding,
    #[error("Pruefsumme stimmt nicht (Tippfehler?)")]
    BadChecksum,
    #[error("Nicht unterstuetzte Code-Version {0}")]
    UnsupportedVersion(u8),
    #[error("Code ist beschaedigt")]
    Malformed,
    #[error("Relay-URL zu lang")]
    RelayTooLong,
}

fn checksum(data: &[u8]) -> [u8; CHECK_LEN] {
    blake3::hash(data).as_bytes()[..CHECK_LEN]
        .try_into()
        .unwrap()
}

impl FriendCode {
    pub fn encode(&self) -> Result<String, FriendCodeError> {
        let mut b = vec![VERSION, 0];
        if let Some(r) = &self.relay_url {
            if r.len() > MAX_RELAY_LEN {
                return Err(FriendCodeError::RelayTooLong);
            }
            b[1] |= FLAG_RELAY;
        }
        b.extend_from_slice(&self.user);
        b.extend_from_slice(&self.node_id);
        if let Some(r) = &self.relay_url {
            b.push(r.len() as u8);
            b.extend_from_slice(r.as_bytes());
        }
        let c = checksum(&b);
        b.extend_from_slice(&c);
        Ok(BASE32_NOPAD.encode(&b))
    }

    pub fn to_link(&self) -> Result<String, FriendCodeError> {
        Ok(format!("{LINK_PREFIX}{}", self.encode()?))
    }

    /// Akzeptiert Code oder Link; ignoriert Gross-/Kleinschreibung, Leerraum und `-`.
    pub fn decode(input: &str) -> Result<Self, FriendCodeError> {
        let s = input.trim();
        let s = s.strip_prefix(LINK_PREFIX).unwrap_or(s);
        let clean: String = s
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if clean.len() < 8 {
            return Err(FriendCodeError::TooShort);
        }
        let b = BASE32_NOPAD
            .decode(clean.as_bytes())
            .map_err(|_| FriendCodeError::BadEncoding)?;
        if b.len() < 2 + 64 + CHECK_LEN {
            return Err(FriendCodeError::TooShort);
        }
        let (body, check) = b.split_at(b.len() - CHECK_LEN);
        if checksum(body) != check {
            return Err(FriendCodeError::BadChecksum);
        }
        if body[0] != VERSION {
            return Err(FriendCodeError::UnsupportedVersion(body[0]));
        }
        let flags = body[1];
        if flags & !FLAG_RELAY != 0 {
            return Err(FriendCodeError::Malformed);
        }
        let user: PublicKey = body[2..34].try_into().unwrap();
        let node_id: PublicKey = body[34..66].try_into().unwrap();
        let rest = &body[66..];
        let relay_url = if flags & FLAG_RELAY != 0 {
            let (&len, r) = rest.split_first().ok_or(FriendCodeError::Malformed)?;
            if r.len() != len as usize {
                return Err(FriendCodeError::Malformed);
            }
            Some(String::from_utf8(r.to_vec()).map_err(|_| FriendCodeError::Malformed)?)
        } else if rest.is_empty() {
            None
        } else {
            return Err(FriendCodeError::Malformed);
        };
        Ok(Self {
            user,
            node_id,
            relay_url,
        })
    }
}

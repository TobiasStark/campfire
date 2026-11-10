//! Wiederherstellungsschluessel und Schluesselwechsel (M-7).
//!
//! Der User-Key liegt auf jedem Geraet und signiert jedes Event; wer ein Geraet erbeutet, hat
//! ihn. Deshalb gibt es einen zweiten Schluessel, der **nie** auf einem Geraet gespeichert wird
//! (Papier, Passwort-Manager). Freunde kennen nur seinen Hash (`commitment`, kommt im Handshake
//! mit und wird beim ersten Mal festgehalten). Mit ihm unterschreibt der Nutzer im Notfall eine
//! [`Rotation`] vom alten auf einen neuen User-Key; Freunde pruefen sie gegen den gespeicherten
//! Hash, sperren den alten Schluessel und uebernehmen die Freundschaft auf den neuen. Die
//! Rotation nennt den Hash des naechsten Wiederherstellungsschluessels, die Kette geht weiter.

use crate::{Hash, PublicKey, Signature};
use data_encoding::BASE32_NOPAD;
use serde::{Deserialize, Serialize};

/// Vorsatz der Textform, damit niemand den Schluessel mit einem Freundescode verwechselt.
pub const PREFIX: &str = "CFR";
const CHECK_LEN: usize = 4;

/// Hash eines Wiederherstellungsschluessels (oeffentlicher Teil), den Freunde speichern.
pub fn commitment(recovery: &PublicKey) -> Hash {
    blake3::derive_key("campfire 2026-11 recovery commitment", recovery)
}

fn checksum(b: &[u8]) -> [u8; CHECK_LEN] {
    let h = blake3::hash(b);
    let mut c = [0u8; CHECK_LEN];
    c.copy_from_slice(&h.as_bytes()[..CHECK_LEN]);
    c
}

/// Textform des geheimen Schluessels: `CFR-XXXX-XXXX-...` (Base32 mit Pruefsumme).
pub fn encode_secret(secret: &[u8; 32]) -> String {
    let mut b = secret.to_vec();
    b.extend_from_slice(&checksum(secret));
    let s = BASE32_NOPAD.encode(&b);
    let groups: Vec<&str> = s
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect();
    format!("{PREFIX}-{}", groups.join("-"))
}

/// Liest die Textform; ignoriert Gross-/Kleinschreibung, Leerraum und `-`.
pub fn decode_secret(input: &str) -> Option<[u8; 32]> {
    let clean: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let body = clean.strip_prefix(PREFIX)?;
    let b = BASE32_NOPAD.decode(body.as_bytes()).ok()?;
    if b.len() != 32 + CHECK_LEN {
        return None;
    }
    let (key, check) = b.split_at(32);
    if checksum(key) != check {
        return None;
    }
    key.try_into().ok()
}

/// Schluesselwechsel, unterschrieben mit dem Wiederherstellungsschluessel `recovery`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rotation {
    pub old: PublicKey,
    pub new: PublicKey,
    /// Oeffentlicher Teil des Wiederherstellungsschluessels; sein `commitment` muss beim
    /// Empfaenger fuer `old` gespeichert sein.
    pub recovery: PublicKey,
    /// `commitment` des naechsten Wiederherstellungsschluessels (gilt dann fuer `new`).
    pub next: Hash,
    pub signature: Signature,
}

impl Rotation {
    pub fn signing_bytes(old: &PublicKey, new: &PublicKey, next: &Hash) -> Vec<u8> {
        let mut v = b"app/rotation/1".to_vec();
        v.extend_from_slice(old);
        v.extend_from_slice(new);
        v.extend_from_slice(next);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_text_roundtrips_and_detects_typos() {
        let s = [7u8; 32];
        let t = encode_secret(&s);
        assert!(t.starts_with("CFR-"));
        assert_eq!(decode_secret(&t), Some(s));
        assert_eq!(decode_secret(&t.to_lowercase().replace('-', " ")), Some(s));
        let mut typo = t.clone().into_bytes();
        let i = typo.len() - 3;
        typo[i] = if typo[i] == b'A' { b'B' } else { b'A' };
        assert_eq!(decode_secret(std::str::from_utf8(&typo).unwrap()), None);
        assert_eq!(decode_secret("ABCD"), None);
        assert_ne!(commitment(&[1; 32]), commitment(&[2; 32]));
    }
}

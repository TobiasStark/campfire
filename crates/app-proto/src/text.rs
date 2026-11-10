//! Central cleanup for text that other people choose (names, channel names, messages).
//!
//! Display names are self-chosen and can be changed at any time. Without cleanup, invisible
//! characters (zero width, bidi overrides) make "Anna" and "An\u{200B}na" look identical. Every
//! name that comes from the network or from the user goes through [`clean_name`]; message
//! bodies go through [`clean_body`].

use unicode_normalization::UnicodeNormalization;

/// Longest display name (characters) after cleanup.
pub const MAX_NAME_CHARS: usize = 64;
/// Longest space/channel name (characters) after cleanup.
pub const MAX_TITLE_CHARS: usize = 100;
/// More combining marks in a row than this are dropped ("Zalgo" text that covers other lines).
const MAX_COMBINING_RUN: usize = 2;

/// Unicode categories Cc, Cf, Zl and Zp plus characters that render as empty space although
/// they are letters (Hangul fillers, braille blank). None of these belongs in a name.
pub fn is_invisible(c: char) -> bool {
    c.is_control()
        || matches!(
            c as u32,
            0x00AD
                | 0x034F
                | 0x0600..=0x0605
                | 0x061C
                | 0x06DD
                | 0x070F
                | 0x0890..=0x0891
                | 0x08E2
                | 0x115F..=0x1160
                | 0x17B4..=0x17B5
                | 0x180B..=0x180F
                | 0x200B..=0x200F
                | 0x2028..=0x202E
                | 0x2060..=0x206F
                | 0x2800
                | 0x3164
                | 0xFE00..=0xFE0F
                | 0xFEFF
                | 0xFFA0
                | 0xFFF0..=0xFFFB
                | 0x110BD
                | 0x110CD
                | 0x13430..=0x1343F
                | 0x1BCA0..=0x1BCA3
                | 0x1D173..=0x1D17A
                | 0xE0000..=0xE0FFF
        )
}

/// Characters that change the reading direction of the following text. They are removed from
/// message bodies too (they can make "exe.pdf" out of "fdp.exe").
pub fn is_bidi_control(c: char) -> bool {
    matches!(c as u32, 0x061C | 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069)
}

fn is_combining(c: char) -> bool {
    matches!(
        c as u32,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF
            | 0xFE20..=0xFE2F
    )
}

/// Cleans a name: NFC, without invisible characters, whitespace collapsed to single spaces,
/// at most [`MAX_COMBINING_RUN`] combining marks in a row, at most `max_chars` characters.
/// May return an empty string; callers decide on a fallback.
pub fn clean_name(raw: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max_chars.saturating_mul(4)));
    let mut n = 0usize;
    let mut combining = 0usize;
    let mut space = false;
    for c in raw.nfc() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if is_invisible(c) {
            continue;
        }
        if is_combining(c) {
            combining += 1;
            if combining > MAX_COMBINING_RUN {
                continue;
            }
        } else {
            combining = 0;
        }
        let need = 1 + usize::from(space);
        if n + need > max_chars {
            break;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
        n += need;
    }
    out
}

/// Cleans a message body: removes bidi overrides and other invisible characters except line
/// breaks, tabs and the joiners needed for emoji and many scripts (ZWJ, ZWNJ, variation
/// selectors); limits combining marks.
pub fn clean_body(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut combining = 0usize;
    for c in raw.chars() {
        let keep = matches!(c, '\n' | '\t' | '\u{200C}' | '\u{200D}')
            || matches!(c as u32, 0xFE00..=0xFE0F | 0xE0020..=0xE007F)
            || !(is_invisible(c) || is_bidi_control(c));
        if !keep {
            continue;
        }
        if is_combining(c) {
            combining += 1;
            if combining > MAX_COMBINING_RUN {
                continue;
            }
        } else {
            combining = 0;
        }
        out.push(c);
    }
    out
}

/// Comparison key for "looks the same": Unicode confusable skeleton, case-folded. Two names
/// with the same key can be mistaken for each other ("Anna" with Cyrillic "а", "rn" vs "m").
pub fn lookalike_key(name: &str) -> String {
    let cleaned = clean_name(name, MAX_NAME_CHARS);
    unicode_security::skeleton(&cleaned.to_lowercase())
        .collect::<String>()
        .to_lowercase()
}

/// Short, readable fingerprint of a public key for comparing out of band ("A1B2-C3D4-E5F6").
pub fn fingerprint(key: &[u8; 32]) -> String {
    let h = blake3::hash(key);
    let s = data_encoding::BASE32_NOPAD.encode(&h.as_bytes()[..8]);
    format!("{}-{}-{}", &s[..4], &s[4..8], &s[8..12])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invisible_and_direction_characters_are_removed() {
        assert_eq!(clean_name("An\u{200B}na", 64), "Anna");
        assert_eq!(clean_name("\u{202E}anna\u{2066}", 64), "anna");
        assert_eq!(clean_name("A\u{FEFF}\u{2060}\u{00AD}B\u{180E}", 64), "AB");
        assert_eq!(clean_name("\u{061C}x\u{2064}", 64), "x");
        assert_eq!(clean_name("\u{3164}\u{115F}", 64), "");
        assert_eq!(clean_name("a\u{2028}b\u{2029}c", 64), "a b c");
        assert_eq!(clean_name("x\u{E0041}y", 64), "xy");
    }

    #[test]
    fn whitespace_is_collapsed_and_length_limited() {
        assert_eq!(clean_name("  Max \t\n  Mustermann  ", 64), "Max Mustermann");
        assert_eq!(clean_name("abcdef", 3), "abc");
        assert_eq!(clean_name("ab cdef", 3), "ab");
        assert_eq!(clean_name(&"ä".repeat(100), 64).chars().count(), 64);
    }

    #[test]
    fn names_are_nfc_and_zalgo_is_cut() {
        assert_eq!(clean_name("A\u{0308}", 64), "Ä");
        let z = format!("a{}", "\u{0301}".repeat(30));
        assert_eq!(clean_name(&z, 64).chars().count(), 1 + MAX_COMBINING_RUN);
    }

    #[test]
    fn bodies_keep_lines_and_emoji_but_lose_overrides() {
        assert_eq!(clean_body("a\nb\tc"), "a\nb\tc");
        assert_eq!(clean_body("rechnung\u{202E}fdp.exe"), "rechnungfdp.exe");
        let family = "👨\u{200D}👩\u{200D}👧";
        assert_eq!(clean_body(family), family);
        assert_eq!(clean_body("❤\u{FE0F}"), "❤\u{FE0F}");
        assert_eq!(clean_body("x\u{200B}y\u{2066}z"), "xyz");
    }

    #[test]
    fn lookalikes_share_a_key() {
        // Kyrillisches "а" (U+0430) statt lateinischem "a".
        assert_eq!(lookalike_key("Anna"), lookalike_key("Ann\u{0430}"));
        assert_eq!(lookalike_key("anna"), lookalike_key("ANNA"));
        assert_eq!(lookalike_key("An\u{200B}na"), lookalike_key("Anna"));
        assert_ne!(lookalike_key("Anna"), lookalike_key("Hanna"));
    }

    #[test]
    fn fingerprints_are_short_and_stable() {
        let f = fingerprint(&[7; 32]);
        assert_eq!(f.len(), 14);
        assert_eq!(f, fingerprint(&[7; 32]));
        assert_ne!(f, fingerprint(&[8; 32]));
    }
}

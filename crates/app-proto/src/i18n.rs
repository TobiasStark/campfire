//! UI language. English is the default; German is selectable in the settings.
//!
//! Texts live next to the code that shows them: `tr("English", "Deutsch")`
//! for plain strings and `tr!("English {x}", "Deutsch {x}")` for formatted ones.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Language {
    #[default]
    English,
    German,
}

impl Language {
    /// Name of the language in that language (for the picker).
    pub fn native_name(self) -> &'static str {
        match self {
            Language::English => "English",
            Language::German => "Deutsch",
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn set_language(l: Language) {
    CURRENT.store(l as u8, Ordering::Relaxed);
}

pub fn language() -> Language {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Language::German,
        _ => Language::English,
    }
}

pub fn is_german() -> bool {
    language() == Language::German
}

/// Picks the text for the current language.
pub fn tr(en: &'static str, de: &'static str) -> &'static str {
    if is_german() {
        de
    } else {
        en
    }
}

/// Formatted variant of [`tr`]; both texts get the same arguments.
#[macro_export]
macro_rules! tr {
    ($en:literal, $de:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::is_german() {
            format!($de $(, $arg)*)
        } else {
            format!($en $(, $arg)*)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    // Only reads the global (never switches it), so it cannot race with other tests.
    #[test]
    fn english_is_the_default() {
        assert_eq!(Language::default(), Language::English);
        assert_eq!(language(), Language::English);
        assert_eq!(tr("Open", "Öffnen"), "Open");
        assert_eq!(tr!("{} files", "{} Dateien", 3), "3 files");
    }
}

//! Design-System von Campfire: Farben (hell/dunkel), Schriftstufen, Abstaende und die
//! Bausteine, aus denen alle Ansichten bestehen. Ansichten waehlen hier Stile aus, statt
//! Farben oder Groessen selbst festzulegen; so bleibt die Oberflaeche einheitlich.

use app_core::settings::{ColorOverrides, ThemeChoice};
use app_proto::control::PresenceStatus;
use iced::border::Radius;
use iced::font::Weight;
use iced::theme::Palette;
use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, text, text_editor, text_input,
    tooltip, Space,
};
use iced::{Background, Border, Color, Element, Font, Length, Shadow, Theme, Vector};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, RwLock};

// ---------- Masse ----------

/// Abstaende (4er-Raster).
pub mod space {
    pub const XS: f32 = 4.0;
    pub const S: f32 = 8.0;
    pub const M: f32 = 12.0;
    pub const L: f32 = 16.0;
    pub const XL: f32 = 24.0;
    pub const XXL: f32 = 32.0;
}

/// Schriftgroessen. Mehr Stufen gibt es nicht.
pub mod size {
    /// Abschnitts-Ueberschriften in Listen ("TEXTKANAELE").
    pub const OVERLINE: f32 = 11.0;
    /// Zeitstempel, Hinweise, Zaehler.
    pub const CAPTION: f32 = 12.0;
    /// Sekundaerer Text, Knoepfe, Listen.
    pub const SMALL: f32 = 13.0;
    /// Fliesstext, Nachrichten.
    pub const BODY: f32 = 14.0;
    /// Karten- und Abschnittstitel.
    pub const H3: f32 = 16.0;
    /// Seitentitel.
    pub const H2: f32 = 20.0;
    /// Begruessung.
    pub const H1: f32 = 26.0;
}

pub mod radius {
    pub const S: f32 = 4.0;
    pub const M: f32 = 6.0;
    pub const L: f32 = 8.0;
    /// Space-Symbole in der linken Leiste.
    pub const RAIL: f32 = 14.0;
    pub const FULL: f32 = 999.0;
}

/// Breite der Space-Leiste, der Seitenleiste und Hoehe der Kopfzeilen.
pub const RAIL_WIDTH: f32 = 72.0;
pub const SIDEBAR_WIDTH: f32 = 248.0;
pub const HEADER_HEIGHT: f32 = 52.0;
/// Lesbare Hoechstbreite fuer Formular- und Listenseiten.
pub const PAGE_WIDTH: f32 = 760.0;

pub const SEMIBOLD: Font = Font {
    weight: Weight::Semibold,
    ..Font::DEFAULT
};
pub const BOLD: Font = Font {
    weight: Weight::Bold,
    ..Font::DEFAULT
};

// ---------- Farben ----------

macro_rules! token_defs {
    ($( $group:literal / $group_de:literal { $( $field:ident => $label:literal / $label_de:literal, )* } )*) => {
        /// Alle Farben einer Variante. Hintergruende von dunkel nach hell (bzw. umgekehrt):
        /// Leiste < Seitenleiste < Inhalt < Flaeche (Karten) < erhoeht (Knoepfe).
        /// Jedes Feld ist vom Nutzer anpassbar (siehe [`TOKEN_META`]).
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct Tokens {
            pub dark: bool,
            $( $( pub $field: Color, )* )*
        }

        /// Schluessel, Beschriftung und Gruppe jeder einstellbaren Farbe, in Anzeigereihenfolge.
        pub const TOKEN_META: &[(&str, &str, &str)] = &[
            $( $( (stringify!($field), $label, $group), )* )*
        ];

        /// German texts for [`TOKEN_META`] (same order).
        pub const TOKEN_META_DE: &[(&str, &str, &str)] = &[
            $( $( (stringify!($field), $label_de, $group_de), )* )*
        ];

        impl Tokens {
            pub fn get(&self, key: &str) -> Option<Color> {
                match key {
                    $( $( stringify!($field) => Some(self.$field), )* )*
                    _ => None,
                }
            }

            pub fn set(&mut self, key: &str, c: Color) -> bool {
                match key {
                    $( $( stringify!($field) => self.$field = c, )* )*
                    _ => return false,
                }
                true
            }
        }
    };
}

token_defs! {
"Backgrounds" / "Hintergründe" {
        rail => "Left rail" / "Leiste links",
        sidebar => "Sidebar" / "Seitenleiste",
        content => "Content" / "Inhalt",
        surface => "Cards and surfaces" / "Karten und Flächen",
        raised => "Buttons" / "Knöpfe",
        raised_hover => "Buttons (hover)" / "Knöpfe (Maus darüber)",
        field => "Input fields" / "Eingabefelder",
        hover => "List hover" / "Hover in Listen",
        active => "List selection" / "Auswahl in Listen",
    }
"Lines" / "Linien" {
        border => "Border" / "Rahmen",
        border_strong => "Border (strong)" / "Rahmen (betont)",
    }
"Text" / "Text" {
        text => "Text" / "Text",
        text_muted => "Text (muted)" / "Text (gedämpft)",
        text_faint => "Text (faint)" / "Text (schwach)",
    }
"Accent" / "Akzent" {
        accent => "Accent" / "Akzent",
        accent_hover => "Accent (hover)" / "Akzent (Maus darüber)",
        accent_pressed => "Accent (pressed)" / "Akzent (gedrückt)",
        accent_fg => "Accent as text color" / "Akzent als Textfarbe",
        accent_soft => "Accent (soft surface)" / "Akzent (sanfte Fläche)",
        on_accent => "Text on accent" / "Text auf Akzent",
    }
"Success" / "Erfolg" {
        success => "Success / Online" / "Erfolg / Online",
        success_fg => "Success as text color" / "Erfolg als Textfarbe",
        success_soft => "Success (soft surface)" / "Erfolg (sanfte Fläche)",
        on_success => "Text on success" / "Text auf Erfolg",
    }
"Warning" / "Warnung" {
        warning => "Warning / Away" / "Warnung / Abwesend",
        warning_soft => "Warning (soft surface)" / "Warnung (sanfte Fläche)",
    }
"Danger" / "Gefahr" {
        danger => "Danger / Do not disturb" / "Gefahr / Nicht stören",
        danger_hover => "Danger (hover)" / "Gefahr (Maus darüber)",
        danger_fg => "Danger as text color" / "Gefahr als Textfarbe",
        danger_soft => "Danger (soft surface)" / "Gefahr (sanfte Fläche)",
        on_danger => "Text on danger" / "Text auf Gefahr",
    }
"Other elements" / "Weitere Elemente" {
        tooltip => "Tooltip (surface)" / "Kurzinfo (Fläche)",
        tooltip_text => "Tooltip (text)" / "Kurzinfo (Text)",
        shadow => "Shadow" / "Schatten",
        status_offline => "Status offline" / "Status Offline",
        knob => "Switch and slider knob" / "Griff von Schalter und Regler",
    }
"Video and images" / "Video und Bilder" {
        media_bg => "Video background" / "Videohintergrund",
        media_fg => "Text over video" / "Text über Video",
        media_fg_dim => "Text over video (muted)" / "Text über Video (gedämpft)",
        media_scrim => "Dimming over images" / "Abdunklung über Bildern",
    }
"Avatars" / "Avatare" {
        on_avatar => "Letters in avatar" / "Buchstaben im Avatar",
        avatar_1 => "Avatar color 1" / "Avatar-Farbe 1",
        avatar_2 => "Avatar color 2" / "Avatar-Farbe 2",
        avatar_3 => "Avatar color 3" / "Avatar-Farbe 3",
        avatar_4 => "Avatar color 4" / "Avatar-Farbe 4",
        avatar_5 => "Avatar color 5" / "Avatar-Farbe 5",
        avatar_6 => "Avatar color 6" / "Avatar-Farbe 6",
        avatar_7 => "Avatar color 7" / "Avatar-Farbe 7",
        avatar_8 => "Avatar color 8" / "Avatar-Farbe 8",
    }
"Overlay" / "Overlay" {
        overlay_bg => "Overlay background" / "Overlay-Hintergrund",
        overlay_speaking => "Overlay: speaking (dot)" / "Overlay: spricht (Punkt)",
        overlay_speaking_text => "Overlay: speaking (text)" / "Overlay: spricht (Text)",
        overlay_idle => "Overlay: idle" / "Overlay: still",
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb8(r, g, b)
}

const fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color {
    Color::from_rgba8(r, g, b, a)
}

pub static DARK: Tokens = Tokens {
    dark: true,
    rail: rgb(17, 18, 21),
    sidebar: rgb(24, 25, 29),
    content: rgb(31, 32, 37),
    surface: rgb(38, 40, 46),
    raised: rgb(46, 48, 55),
    raised_hover: rgb(56, 58, 66),
    field: rgb(21, 22, 26),
    hover: rgba(255, 255, 255, 0.05),
    active: rgba(255, 255, 255, 0.09),
    border: rgb(50, 52, 60),
    border_strong: rgb(70, 73, 83),
    text: rgb(236, 237, 241),
    text_muted: rgb(170, 173, 183),
    text_faint: rgb(140, 143, 154),
    accent: rgb(88, 101, 242),
    accent_hover: rgb(108, 119, 245),
    accent_pressed: rgb(71, 82, 196),
    accent_fg: rgb(144, 153, 255),
    accent_soft: rgba(88, 101, 242, 0.20),
    on_accent: rgb(255, 255, 255),
    success: rgb(35, 165, 90),
    success_fg: rgb(76, 200, 125),
    success_soft: rgba(35, 165, 90, 0.16),
    on_success: rgb(255, 255, 255),
    warning: rgb(240, 178, 50),
    warning_soft: rgba(240, 178, 50, 0.14),
    danger: rgb(218, 55, 60),
    danger_hover: rgb(190, 44, 49),
    danger_fg: rgb(255, 112, 114),
    danger_soft: rgba(237, 66, 69, 0.15),
    on_danger: rgb(255, 255, 255),
    tooltip: rgb(10, 11, 13),
    tooltip_text: rgb(255, 255, 255),
    shadow: rgba(0, 0, 0, 0.40),
    status_offline: rgb(128, 132, 142),
    knob: rgb(255, 255, 255),
    media_bg: rgb(0, 0, 0),
    media_fg: rgb(255, 255, 255),
    media_fg_dim: rgb(200, 202, 208),
    media_scrim: rgba(0, 0, 0, 0.6),
    on_avatar: rgb(255, 255, 255),
    avatar_1: rgb(88, 101, 242),
    avatar_2: rgb(214, 104, 72),
    avatar_3: rgb(46, 150, 128),
    avatar_4: rgb(194, 86, 148),
    avatar_5: rgb(196, 140, 34),
    avatar_6: rgb(56, 132, 204),
    avatar_7: rgb(132, 96, 206),
    avatar_8: rgb(76, 156, 88),
    overlay_bg: rgb(13, 13, 20),
    overlay_speaking: rgb(89, 230, 115),
    overlay_speaking_text: rgb(255, 255, 255),
    overlay_idle: rgb(179, 179, 191),
};

pub static LIGHT: Tokens = Tokens {
    dark: false,
    rail: rgb(226, 228, 234),
    sidebar: rgb(240, 241, 245),
    content: rgb(255, 255, 255),
    surface: rgb(247, 248, 250),
    raised: rgb(234, 236, 240),
    raised_hover: rgb(222, 225, 231),
    field: rgb(255, 255, 255),
    hover: rgba(20, 24, 40, 0.05),
    active: rgba(20, 24, 40, 0.09),
    border: rgb(220, 223, 229),
    border_strong: rgb(194, 198, 207),
    text: rgb(28, 30, 35),
    text_muted: rgb(78, 82, 92),
    text_faint: rgb(104, 108, 119),
    accent: rgb(88, 101, 242),
    accent_hover: rgb(71, 82, 196),
    accent_pressed: rgb(60, 69, 165),
    accent_fg: rgb(67, 78, 200),
    accent_soft: rgba(88, 101, 242, 0.12),
    on_accent: rgb(255, 255, 255),
    success: rgb(35, 165, 90),
    success_fg: rgb(22, 122, 64),
    success_soft: rgba(35, 165, 90, 0.12),
    on_success: rgb(255, 255, 255),
    warning: rgb(221, 150, 20),
    warning_soft: rgba(240, 178, 50, 0.16),
    danger: rgb(216, 60, 62),
    danger_hover: rgb(185, 44, 47),
    danger_fg: rgb(190, 38, 42),
    danger_soft: rgba(216, 60, 62, 0.10),
    on_danger: rgb(255, 255, 255),
    tooltip: rgb(28, 30, 35),
    tooltip_text: rgb(255, 255, 255),
    shadow: rgba(16, 20, 32, 0.16),
    status_offline: rgb(128, 132, 142),
    knob: rgb(255, 255, 255),
    media_bg: rgb(0, 0, 0),
    media_fg: rgb(255, 255, 255),
    media_fg_dim: rgb(200, 202, 208),
    media_scrim: rgba(0, 0, 0, 0.6),
    on_avatar: rgb(255, 255, 255),
    avatar_1: rgb(88, 101, 242),
    avatar_2: rgb(214, 104, 72),
    avatar_3: rgb(46, 150, 128),
    avatar_4: rgb(194, 86, 148),
    avatar_5: rgb(196, 140, 34),
    avatar_6: rgb(56, 132, 204),
    avatar_7: rgb(132, 96, 206),
    avatar_8: rgb(76, 156, 88),
    overlay_bg: rgb(13, 13, 20),
    overlay_speaking: rgb(89, 230, 115),
    overlay_speaking_text: rgb(255, 255, 255),
    overlay_idle: rgb(179, 179, 191),
};

// ---------- Eigene Farben ----------

/// Hex-Farbe (`#RRGGBB` oder `#RRGGBBAA`, `#` optional) lesen.
pub fn parse_hex(s: &str) -> Option<Color> {
    let h = s.trim().trim_start_matches('#');
    if !h.is_ascii() || (h.len() != 6 && h.len() != 8) {
        return None;
    }
    let b = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    let a = if h.len() == 8 { b(6)? } else { 255 };
    Some(Color::from_rgba8(b(0)?, b(2)?, b(4)?, f32::from(a) / 255.0))
}

/// Farbe als Hex-Text; der Alphakanal nur, wenn sie nicht deckend ist.
pub fn to_hex(c: Color) -> String {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    if q(c.a) == 255 {
        format!("#{:02X}{:02X}{:02X}", q(c.r), q(c.g), q(c.b))
    } else {
        format!("#{:02X}{:02X}{:02X}{:02X}", q(c.r), q(c.g), q(c.b), q(c.a))
    }
}

fn mix(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a,
    }
}

fn luminance(c: Color) -> f32 {
    let f = |v: f32| {
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b)
}

/// Lesbare Schriftfarbe (hell oder dunkel) auf einer Flaeche.
fn readable_on(c: Color) -> Color {
    if luminance(c) > 0.35 {
        rgb(20, 22, 28)
    } else {
        Color::WHITE
    }
}

/// Tokens einer Variante mit den Eingaben des Nutzers. Wer eine Grundfarbe (Akzent, Erfolg,
/// Warnung, Gefahr) aendert, bekommt Hover, Text- und sanfte Variante passend dazu, solange
/// er diese nicht selbst festlegt.
pub fn build_tokens(base: &Tokens, ov: &BTreeMap<String, String>) -> Tokens {
    let mut t = *base;
    let dark = base.dark;
    let own = |k: &str| ov.get(k).and_then(|v| parse_hex(v));
    let (white, black) = (Color::WHITE, Color::BLACK);
    let soft = |c: Color, like: Color| Color { a: like.a, ..c };
    if let Some(a) = own("accent") {
        t.accent_hover = if dark {
            mix(a, white, 0.12)
        } else {
            mix(a, black, 0.2)
        };
        t.accent_pressed = mix(a, black, if dark { 0.2 } else { 0.35 });
        t.accent_fg = if dark {
            mix(a, white, 0.35)
        } else {
            mix(a, black, 0.15)
        };
        t.accent_soft = soft(a, base.accent_soft);
        t.on_accent = readable_on(a);
    }
    if let Some(c) = own("danger") {
        t.danger_hover = mix(c, black, 0.15);
        t.danger_fg = if dark {
            mix(c, white, 0.3)
        } else {
            mix(c, black, 0.1)
        };
        t.danger_soft = soft(c, base.danger_soft);
        t.on_danger = readable_on(c);
    }
    if let Some(c) = own("success") {
        t.success_fg = if dark {
            mix(c, white, 0.25)
        } else {
            mix(c, black, 0.3)
        };
        t.success_soft = soft(c, base.success_soft);
        t.on_success = readable_on(c);
    }
    if let Some(c) = own("warning") {
        t.warning_soft = soft(c, base.warning_soft);
    }
    for (k, _, _) in TOKEN_META {
        if let Some(c) = own(k) {
            t.set(k, c);
        }
    }
    t
}

fn make_theme(name: &'static str, t: &Tokens) -> Theme {
    Theme::custom(
        name,
        Palette {
            background: t.content,
            text: t.text,
            primary: t.accent,
            success: t.success,
            warning: t.warning,
            danger: t.danger,
        },
    )
}

struct Live {
    tokens: [&'static Tokens; 2],
    themes: [Theme; 2],
}

impl Live {
    fn new(dark: &'static Tokens, light: &'static Tokens) -> Self {
        Live {
            tokens: [dark, light],
            themes: [
                make_theme("Campfire dunkel", dark),
                make_theme("Campfire hell", light),
            ],
        }
    }
}

static LIVE: LazyLock<RwLock<Live>> = LazyLock::new(|| RwLock::new(Live::new(&DARK, &LIGHT)));

/// Zuletzt gewaehlte Variante, fuer Stellen ohne Zugriff auf das Theme (Text-Spans).
static IS_DARK: AtomicBool = AtomicBool::new(true);

/// Eigene Farben uebernehmen; wirkt ab dem naechsten Zeichnen. Die Tokens werden bewusst
/// nicht freigegeben (Stil-Funktionen halten `&'static`); je Aenderung sind das ~1 KB.
pub fn apply_colors(ov: &ColorOverrides) {
    let pick = |base: &'static Tokens, m: &BTreeMap<String, String>| -> &'static Tokens {
        if m.is_empty() {
            base
        } else {
            Box::leak(Box::new(build_tokens(base, m)))
        }
    };
    let live = Live::new(pick(&DARK, &ov.dark), pick(&LIGHT, &ov.light));
    if let Ok(mut w) = LIVE.write() {
        *w = live;
    }
}

fn live_tokens(dark: bool) -> &'static Tokens {
    match LIVE.read() {
        Ok(l) => l.tokens[usize::from(!dark)],
        Err(_) => {
            if dark {
                &DARK
            } else {
                &LIGHT
            }
        }
    }
}

/// Farben zum aktuellen Theme.
pub fn tokens(_: &Theme) -> &'static Tokens {
    current()
}

/// Farben der aktuellen Variante ausserhalb von Stil-Funktionen.
pub fn current() -> &'static Tokens {
    live_tokens(IS_DARK.load(Ordering::Relaxed))
}

/// Standardfarben einer Variante (ohne Anpassungen), etwa als Vorgabe im Editor.
pub fn defaults(choice: ThemeChoice) -> &'static Tokens {
    match choice {
        ThemeChoice::Dark => &DARK,
        ThemeChoice::Light => &LIGHT,
    }
}

pub fn theme(choice: ThemeChoice) -> Theme {
    let dark = choice == ThemeChoice::Dark;
    IS_DARK.store(dark, Ordering::Relaxed);
    match LIVE.read() {
        Ok(l) => l.themes[usize::from(!dark)].clone(),
        Err(_) => make_theme("Campfire", live_tokens(dark)),
    }
}

// ---------- Text ----------

pub mod tx {
    //! Textfarben fuer `text(..).style(..)`.
    use super::tokens;
    use iced::widget::text::Style;
    use iced::Theme;

    pub fn normal(t: &Theme) -> Style {
        Style {
            color: Some(tokens(t).text),
        }
    }
    pub fn muted(t: &Theme) -> Style {
        Style {
            color: Some(tokens(t).text_muted),
        }
    }
    pub fn faint(t: &Theme) -> Style {
        Style {
            color: Some(tokens(t).text_faint),
        }
    }
    pub fn accent(t: &Theme) -> Style {
        Style {
            color: Some(tokens(t).accent_fg),
        }
    }
    pub fn success(t: &Theme) -> Style {
        Style {
            color: Some(tokens(t).success_fg),
        }
    }
    pub fn danger(t: &Theme) -> Style {
        Style {
            color: Some(tokens(t).danger_fg),
        }
    }
}

/// Seitentitel.
pub fn title<'a>(s: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(s).size(size::H2).font(BOLD).style(tx::normal)
}

/// Titel einer Karte oder eines Abschnitts.
pub fn heading<'a>(s: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(s).size(size::H3).font(SEMIBOLD).style(tx::normal)
}

/// Beschriftung eines Eingabefelds oder einer Einstellung.
pub fn label<'a>(s: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(s).size(size::SMALL).font(SEMIBOLD).style(tx::normal)
}

/// Erklaerender Text unter Titeln und Feldern.
pub fn muted<'a>(s: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(s).size(size::SMALL).style(tx::muted)
}

/// Zeitstempel, Zaehler, Nebensaechliches.
pub fn caption<'a>(s: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(s).size(size::CAPTION).style(tx::faint)
}

/// Kleine Abschnitts-Ueberschrift in Listen, in Grossbuchstaben.
pub fn overline<'a>(s: &str) -> text::Text<'a> {
    text(s.to_uppercase())
        .size(size::OVERLINE)
        .font(BOLD)
        .style(tx::faint)
}

// ---------- Flaechen ----------

pub mod bg {
    //! Container-Stile.
    use super::{radius, tokens, Tokens};
    use iced::widget::container::Style;
    use iced::{Background, Border, Color, Shadow, Theme, Vector};

    fn fill(c: Color) -> Style {
        Style {
            background: Some(Background::Color(c)),
            ..Style::default()
        }
    }

    pub fn rail(t: &Theme) -> Style {
        fill(tokens(t).rail)
    }
    pub fn sidebar(t: &Theme) -> Style {
        fill(tokens(t).sidebar)
    }
    pub fn content(t: &Theme) -> Style {
        Style {
            text_color: Some(tokens(t).text),
            ..fill(tokens(t).content)
        }
    }
    /// Karte: Flaeche mit feinem Rand.
    pub fn card(t: &Theme) -> Style {
        let k = tokens(t);
        Style {
            background: Some(Background::Color(k.surface)),
            border: Border::default()
                .rounded(radius::L)
                .width(1)
                .color(k.border),
            ..Style::default()
        }
    }
    /// Schwebende Flaeche (Menues, Vorschlaege) mit Schatten.
    pub fn popover(t: &Theme) -> Style {
        let k = tokens(t);
        Style {
            background: Some(Background::Color(if k.dark { k.raised } else { k.content })),
            border: Border::default()
                .rounded(radius::L)
                .width(1)
                .color(k.border),
            shadow: shadow(k, 12.0),
            ..Style::default()
        }
    }
    /// Feld ohne Rand (z. B. Vorschaubild-Platzhalter).
    pub fn well(t: &Theme) -> Style {
        let k = tokens(t);
        Style {
            background: Some(Background::Color(k.raised)),
            border: Border::default().rounded(radius::M),
            ..Style::default()
        }
    }
    pub fn tooltip(t: &Theme) -> Style {
        let k = tokens(t);
        Style {
            text_color: Some(k.tooltip_text),
            background: Some(Background::Color(k.tooltip)),
            border: Border::default().rounded(radius::M),
            shadow: shadow(k, 8.0),
            ..Style::default()
        }
    }
    /// Trennlinie (Container mit 1 px Hoehe oder Breite).
    pub fn divider(t: &Theme) -> Style {
        fill(tokens(t).border)
    }
    /// Hervorgehobener Hintergrund (Nachricht unter der Maus, Auswahl).
    pub fn hover(t: &Theme) -> Style {
        Style {
            background: Some(Background::Color(tokens(t).hover)),
            border: Border::default().rounded(radius::M),
            ..Style::default()
        }
    }
    /// Erwaehnung des eigenen Namens.
    pub fn mention(t: &Theme) -> Style {
        let k = tokens(t);
        Style {
            background: Some(Background::Color(k.warning_soft)),
            border: Border::default().rounded(radius::M),
            ..Style::default()
        }
    }
    /// Eingabebereich des Chats.
    pub fn composer(t: &Theme) -> Style {
        let k = tokens(t);
        Style {
            background: Some(Background::Color(if k.dark {
                k.surface
            } else {
                k.content
            })),
            border: Border::default()
                .rounded(radius::L)
                .width(1)
                .color(k.border_strong),
            ..Style::default()
        }
    }
    pub fn video(_: &Theme) -> Style {
        Style {
            background: Some(Background::Color(super::current().media_bg)),
            border: Border::default().rounded(radius::L),
            ..Style::default()
        }
    }

    fn banner(bgc: Color, line: Color, fg: Color) -> Style {
        Style {
            text_color: Some(fg),
            background: Some(Background::Color(bgc)),
            border: Border::default().rounded(radius::L).width(1).color(line),
            ..Style::default()
        }
    }
    pub fn info(t: &Theme) -> Style {
        let k = tokens(t);
        banner(k.accent_soft, k.accent.scale_alpha(0.35), k.text)
    }
    pub fn success(t: &Theme) -> Style {
        let k = tokens(t);
        banner(k.success_soft, k.success.scale_alpha(0.35), k.text)
    }
    pub fn warning(t: &Theme) -> Style {
        let k = tokens(t);
        banner(k.warning_soft, k.warning.scale_alpha(0.40), k.text)
    }
    pub fn danger(t: &Theme) -> Style {
        let k = tokens(t);
        banner(k.danger_soft, k.danger.scale_alpha(0.40), k.text)
    }

    /// Zaehler-Plakette.
    pub fn badge_accent(t: &Theme) -> Style {
        let k = tokens(t);
        Style {
            text_color: Some(k.on_accent),
            background: Some(Background::Color(k.accent)),
            border: Border::default().rounded(radius::FULL),
            ..Style::default()
        }
    }
    pub fn badge_danger(t: &Theme) -> Style {
        let k = tokens(t);
        Style {
            text_color: Some(k.on_danger),
            background: Some(Background::Color(k.danger)),
            border: Border::default().rounded(radius::FULL),
            ..Style::default()
        }
    }
    pub fn badge_neutral(t: &Theme) -> Style {
        let k = tokens(t);
        Style {
            text_color: Some(k.text_muted),
            background: Some(Background::Color(k.raised)),
            border: Border::default().rounded(radius::FULL),
            ..Style::default()
        }
    }
    /// "Im Gespraech"-Markierung.
    pub fn badge_success(t: &Theme) -> Style {
        let k = tokens(t);
        Style {
            text_color: Some(k.on_success),
            background: Some(Background::Color(k.success)),
            border: Border::default().rounded(radius::FULL),
            ..Style::default()
        }
    }
    pub fn badge_live(t: &Theme) -> Style {
        let k = tokens(t);
        Style {
            text_color: Some(k.on_danger),
            background: Some(Background::Color(k.danger)),
            border: Border::default().rounded(radius::S),
            ..Style::default()
        }
    }

    pub fn shadow(k: &Tokens, blur: f32) -> Shadow {
        Shadow {
            color: k.shadow,
            offset: Vector::new(0.0, blur / 3.0),
            blur_radius: blur,
        }
    }
}

// ---------- Knoepfe ----------

pub mod btn {
    //! Knopf-Stile. Jeder Stil hat Hover-, Druck- und deaktivierten Zustand.
    use super::{radius, tokens, Tokens};
    use iced::widget::button::{Status, Style};
    use iced::{Background, Border, Color, Theme};

    fn style(bg: Option<Color>, fg: Color, border: Border) -> Style {
        Style {
            background: bg.map(Background::Color),
            text_color: fg,
            border,
            ..Style::default()
        }
    }

    fn r(x: f32) -> Border {
        Border::default().rounded(x)
    }

    fn disabled(k: &Tokens) -> Style {
        style(Some(k.raised.scale_alpha(0.6)), k.text_faint, r(radius::M))
    }

    /// Hauptaktion einer Ansicht (hoechstens eine pro Bereich).
    pub fn primary(t: &Theme, s: Status) -> Style {
        let k = tokens(t);
        match s {
            Status::Active => style(Some(k.accent), k.on_accent, r(radius::M)),
            Status::Hovered => style(Some(k.accent_hover), k.on_accent, r(radius::M)),
            Status::Pressed => style(Some(k.accent_pressed), k.on_accent, r(radius::M)),
            Status::Disabled => disabled(k),
        }
    }

    /// Normale Aktion.
    pub fn secondary(t: &Theme, s: Status) -> Style {
        let k = tokens(t);
        match s {
            Status::Active => style(Some(k.raised), k.text, r(radius::M)),
            Status::Hovered => style(Some(k.raised_hover), k.text, r(radius::M)),
            Status::Pressed => style(Some(k.border_strong), k.text, r(radius::M)),
            Status::Disabled => disabled(k),
        }
    }

    /// Unaufdringliche Aktion ohne Flaeche (Symbolknoepfe, Nebenaktionen).
    pub fn ghost(t: &Theme, s: Status) -> Style {
        let k = tokens(t);
        match s {
            Status::Active => style(None, k.text_muted, r(radius::M)),
            Status::Hovered => style(Some(k.hover), k.text, r(radius::M)),
            Status::Pressed => style(Some(k.active), k.text, r(radius::M)),
            Status::Disabled => style(None, k.text_faint.scale_alpha(0.6), r(radius::M)),
        }
    }

    /// Zerstoererische Aktion (Loeschen, Auflegen, Blockieren).
    pub fn danger(t: &Theme, s: Status) -> Style {
        let k = tokens(t);
        match s {
            Status::Active => style(Some(k.danger), k.on_danger, r(radius::M)),
            Status::Hovered | Status::Pressed => {
                style(Some(k.danger_hover), k.on_danger, r(radius::M))
            }
            Status::Disabled => disabled(k),
        }
    }

    /// Zerstoererische Nebenaktion: nur rote Schrift, Flaeche erst beim Hover.
    pub fn danger_ghost(t: &Theme, s: Status) -> Style {
        let k = tokens(t);
        match s {
            Status::Active => style(None, k.danger_fg, r(radius::M)),
            Status::Hovered | Status::Pressed => {
                style(Some(k.danger_soft), k.danger_fg, r(radius::M))
            }
            Status::Disabled => style(None, k.text_faint, r(radius::M)),
        }
    }

    pub fn success(t: &Theme, s: Status) -> Style {
        let k = tokens(t);
        let dark = Color {
            a: 1.0,
            ..k.success.scale_alpha(1.0)
        };
        match s {
            Status::Active => style(Some(dark), k.on_success, r(radius::M)),
            Status::Hovered | Status::Pressed => style(
                Some(Color {
                    r: dark.r * 0.88,
                    g: dark.g * 0.88,
                    b: dark.b * 0.88,
                    a: 1.0,
                }),
                k.on_success,
                r(radius::M),
            ),
            Status::Disabled => disabled(k),
        }
    }

    /// Text-Link.
    pub fn link(t: &Theme, s: Status) -> Style {
        let k = tokens(t);
        match s {
            Status::Active => style(None, k.accent_fg, r(radius::S)),
            Status::Hovered | Status::Pressed => style(None, k.text, r(radius::S)),
            Status::Disabled => style(None, k.text_faint, r(radius::S)),
        }
    }

    /// Eintrag einer Navigationsliste (Channels, Einstellungs-Bereiche, Direktnachrichten).
    pub fn nav(selected: bool) -> impl Fn(&Theme, Status) -> Style {
        nav_item(selected, false)
    }

    /// Wie `nav`; ungelesene Eintraege haben volle Schriftfarbe.
    pub fn nav_item(selected: bool, unread: bool) -> impl Fn(&Theme, Status) -> Style {
        move |t, s| {
            let k = tokens(t);
            let idle = if unread { k.text } else { k.text_muted };
            match (selected, s) {
                (true, _) => style(Some(k.active), k.text, r(radius::M)),
                (false, Status::Hovered) => style(Some(k.hover), k.text, r(radius::M)),
                (false, Status::Pressed) => style(Some(k.active), k.text, r(radius::M)),
                (false, _) => style(None, idle, r(radius::M)),
            }
        }
    }

    /// Symbol in der Space-Leiste.
    pub fn rail(selected: bool) -> impl Fn(&Theme, Status) -> Style {
        move |t, s| {
            let k = tokens(t);
            let on = style(Some(k.accent), k.on_accent, r(radius::RAIL));
            match (selected, s) {
                (true, _) | (false, Status::Hovered | Status::Pressed) => on,
                (false, _) => style(Some(k.raised), k.text, r(radius::RAIL)),
            }
        }
    }

    /// Auswahl aus wenigen Moeglichkeiten (Segmente, Ecken, Bildrate).
    pub fn chip(selected: bool) -> impl Fn(&Theme, Status) -> Style {
        move |t, s| {
            let k = tokens(t);
            let b = |c: Color| Border::default().rounded(radius::M).width(1).color(c);
            match (selected, s) {
                (true, _) => style(Some(k.accent_soft), k.accent_fg, b(k.accent)),
                (false, Status::Hovered | Status::Pressed) => {
                    style(Some(k.hover), k.text, b(k.border_strong))
                }
                (false, Status::Disabled) => style(None, k.text_faint, b(k.border)),
                (false, Status::Active) => style(None, k.text_muted, b(k.border)),
            }
        }
    }

    /// Schwebender Knopf ueber Inhalt ("Neueste Nachrichten").
    pub fn floating(t: &Theme, s: Status) -> Style {
        let k = tokens(t);
        let fill = match s {
            Status::Hovered | Status::Pressed => k.raised_hover,
            _ => k.raised,
        };
        Style {
            background: Some(Background::Color(fill)),
            text_color: k.text,
            border: Border::default()
                .rounded(radius::FULL)
                .width(1)
                .color(k.border_strong),
            shadow: super::bg::shadow(k, 10.0),
            ..Style::default()
        }
    }

    /// Symbolknopf, der aktiv wie ein gewaehlter Chip aussieht.
    pub fn chip_or_ghost(active: bool) -> impl Fn(&Theme, Status) -> Style {
        move |t, s| {
            if active {
                chip(true)(t, s)
            } else {
                ghost(t, s)
            }
        }
    }

    /// Knopf, der nur als Klickflaeche dient (Bilder, Karten).
    pub fn bare(_: &Theme, _: Status) -> Style {
        Style::default()
    }

    /// Ganze Karte als Knopf (Quellen-Auswahl).
    pub fn card(selected: bool) -> impl Fn(&Theme, Status) -> Style {
        move |t, s| {
            let k = tokens(t);
            let b = |c: Color, w: f32| Border::default().rounded(radius::L).width(w).color(c);
            match (selected, s) {
                (true, _) => style(Some(k.accent_soft), k.text, b(k.accent, 2.0)),
                (false, Status::Hovered | Status::Pressed) => {
                    style(Some(k.raised), k.text, b(k.border_strong, 1.0))
                }
                _ => style(Some(k.surface), k.text, b(k.border, 1.0)),
            }
        }
    }
}

// ---------- Eingaben ----------

pub fn input_style(t: &Theme, s: text_input::Status) -> text_input::Style {
    let k = tokens(t);
    let border = |c: Color| Border::default().rounded(radius::M).width(1).color(c);
    let base = text_input::Style {
        background: Background::Color(k.field),
        border: border(k.border),
        icon: k.text_muted,
        placeholder: k.text_faint,
        value: k.text,
        selection: k.accent.scale_alpha(0.35),
    };
    match s {
        text_input::Status::Active => base,
        text_input::Status::Hovered => text_input::Style {
            border: border(k.border_strong),
            ..base
        },
        text_input::Status::Focused { .. } => text_input::Style {
            border: border(k.accent),
            ..base
        },
        text_input::Status::Disabled => text_input::Style {
            background: Background::Color(k.raised.scale_alpha(0.5)),
            value: k.text_muted,
            ..base
        },
    }
}

/// Einheitliches Eingabefeld.
pub fn input<'a, M: Clone + 'a>(placeholder: &str, value: &str) -> text_input::TextInput<'a, M> {
    text_input(placeholder, value)
        .padding([7, 10])
        .size(size::BODY)
        .style(input_style)
}

/// Mehrzeiliges Feld ohne eigenen Rahmen (liegt in einer Karte).
pub fn editor_plain(t: &Theme, _: text_editor::Status) -> text_editor::Style {
    let k = tokens(t);
    text_editor::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border::default(),
        placeholder: k.text_faint,
        value: k.text,
        selection: k.accent.scale_alpha(0.35),
    }
}

pub fn pick_style(t: &Theme, s: pick_list::Status) -> pick_list::Style {
    let k = tokens(t);
    let border = |c: Color| Border::default().rounded(radius::M).width(1).color(c);
    let base = pick_list::Style {
        text_color: k.text,
        placeholder_color: k.text_faint,
        handle_color: k.text_muted,
        background: Background::Color(k.field),
        border: border(k.border),
    };
    match s {
        pick_list::Status::Active => base,
        pick_list::Status::Hovered => pick_list::Style {
            border: border(k.border_strong),
            ..base
        },
        pick_list::Status::Opened { .. } => pick_list::Style {
            border: border(k.accent),
            ..base
        },
    }
}

pub fn menu_style(t: &Theme) -> iced::overlay::menu::Style {
    let k = tokens(t);
    iced::overlay::menu::Style {
        background: Background::Color(if k.dark { k.raised } else { k.content }),
        border: Border::default()
            .rounded(radius::M)
            .width(1)
            .color(k.border),
        text_color: k.text,
        selected_text_color: k.on_accent,
        selected_background: Background::Color(k.accent),
        shadow: bg::shadow(k, 12.0),
    }
}

/// Einheitliche Auswahlliste.
pub fn select<'a, T, L, V, M>(
    options: L,
    selected: Option<V>,
    on_select: impl Fn(T) -> M + 'a,
) -> pick_list::PickList<'a, T, L, V, M>
where
    T: ToString + PartialEq + Clone + 'a,
    L: std::borrow::Borrow<[T]> + 'a,
    V: std::borrow::Borrow<T> + 'a,
    M: Clone + 'a,
{
    pick_list(options, selected, on_select)
        .padding([7, 10])
        .text_size(size::BODY)
        .style(pick_style)
        .menu_style(menu_style)
}

pub fn check_style(t: &Theme, s: checkbox::Status) -> checkbox::Style {
    let k = tokens(t);
    let (checked, hovered, disabled) = match s {
        checkbox::Status::Active { is_checked } => (is_checked, false, false),
        checkbox::Status::Hovered { is_checked } => (is_checked, true, false),
        checkbox::Status::Disabled { is_checked } => (is_checked, false, true),
    };
    let border = |c: Color| Border::default().rounded(radius::S).width(1.5).color(c);
    let mut st = if checked {
        checkbox::Style {
            background: Background::Color(if hovered { k.accent_hover } else { k.accent }),
            icon_color: k.on_accent,
            border: border(if hovered { k.accent_hover } else { k.accent }),
            text_color: Some(k.text),
        }
    } else {
        checkbox::Style {
            background: Background::Color(k.field),
            icon_color: k.on_accent,
            border: border(if hovered { k.accent } else { k.border_strong }),
            text_color: Some(k.text),
        }
    };
    if disabled {
        st.text_color = Some(k.text_faint);
        st.background = Background::Color(k.raised);
    }
    st
}

/// Einheitliches Kontrollkaestchen.
pub fn check<'a, M: 'a>(value: bool, label: impl Into<String>) -> checkbox::Checkbox<'a, M> {
    checkbox(value)
        .label(label.into())
        .size(16)
        .spacing(10)
        .text_size(size::BODY)
        .style(check_style)
}

pub fn scroll_style(t: &Theme, s: scrollable::Status) -> scrollable::Style {
    let k = tokens(t);
    let hovered = matches!(
        s,
        scrollable::Status::Hovered {
            is_vertical_scrollbar_hovered: true,
            ..
        } | scrollable::Status::Dragged {
            is_vertical_scrollbar_dragged: true,
            ..
        }
    );
    let rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            background: Background::Color(if hovered {
                k.text_faint
            } else {
                k.border_strong
            }),
            border: Border::default().rounded(radius::FULL),
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        auto_scroll: scrollable::AutoScroll {
            background: Background::Color(k.raised),
            border: Border::default()
                .rounded(radius::FULL)
                .width(1)
                .color(k.border),
            shadow: bg::shadow(k, 8.0),
            icon: k.text_muted,
        },
    }
}

/// Senkrecht scrollender Bereich mit schmaler Leiste.
pub fn scroll<'a, M: 'a>(content: impl Into<Element<'a, M>>) -> scrollable::Scrollable<'a, M> {
    scrollable(content)
        .direction(scrollable::Direction::Vertical(
            scrollable::Scrollbar::new()
                .width(6)
                .scroller_width(6)
                .margin(2),
        ))
        .style(scroll_style)
}

pub fn toggle_style(t: &Theme, s: iced::widget::toggler::Status) -> iced::widget::toggler::Style {
    use iced::widget::toggler::Status as S;
    let k = tokens(t);
    let (on, hovered, disabled) = match s {
        S::Active { is_toggled } => (is_toggled, false, false),
        S::Hovered { is_toggled } => (is_toggled, true, false),
        S::Disabled { is_toggled } => (is_toggled, false, true),
    };
    let track = match (on, hovered) {
        (true, false) => k.accent,
        (true, true) => k.accent_hover,
        (false, false) => k.border_strong,
        (false, true) => k.text_faint,
    };
    iced::widget::toggler::Style {
        background: Background::Color(if disabled {
            track.scale_alpha(0.5)
        } else {
            track
        }),
        background_border_width: 0.0,
        background_border_color: Color::TRANSPARENT,
        foreground: Background::Color(k.knob),
        foreground_border_width: 0.0,
        foreground_border_color: Color::TRANSPARENT,
        text_color: Some(if disabled { k.text_faint } else { k.text }),
        border_radius: None,
        padding_ratio: 0.12,
    }
}

/// Schalter (an/aus) fuer Einstellungen.
pub fn toggle<'a, M: 'a>(value: bool) -> iced::widget::Toggler<'a, M> {
    iced::widget::toggler(value).size(20).style(toggle_style)
}

pub fn slider_style(t: &Theme, s: iced::widget::slider::Status) -> iced::widget::slider::Style {
    use iced::widget::slider::{Handle, HandleShape, Rail, Status as S, Style};
    let k = tokens(t);
    let active = matches!(s, S::Hovered | S::Dragged);
    Style {
        rail: Rail {
            backgrounds: (
                Background::Color(if active { k.accent_hover } else { k.accent }),
                Background::Color(k.border_strong),
            ),
            width: 4.0,
            border: Border::default().rounded(radius::FULL),
        },
        handle: Handle {
            shape: HandleShape::Circle {
                radius: if active { 8.0 } else { 7.0 },
            },
            background: Background::Color(k.knob),
            border_width: 2.0,
            border_color: k.accent,
        },
    }
}

/// Schieberegler im einheitlichen Stil.
pub fn slider<'a, T, M>(
    range: std::ops::RangeInclusive<T>,
    value: T,
    on_change: impl Fn(T) -> M + 'a,
) -> iced::widget::Slider<'a, T, M>
where
    T: Copy + From<u8> + PartialOrd + 'a,
    M: Clone + 'a,
{
    iced::widget::slider(range, value, on_change).style(slider_style)
}

/// Pegelanzeige (Mikrofon).
pub fn meter_style(t: &Theme) -> iced::widget::progress_bar::Style {
    let k = tokens(t);
    iced::widget::progress_bar::Style {
        background: Background::Color(k.raised),
        bar: Background::Color(k.success),
        border: Border::default().rounded(radius::FULL),
    }
}

// ---------- Bausteine ----------

/// Tooltip im einheitlichen Stil.
pub fn tip<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    label: impl Into<String>,
    position: tooltip::Position,
) -> Element<'a, M> {
    tooltip(
        content,
        container(text(label.into()).size(size::CAPTION)).padding([5, 9]),
        position,
    )
    .gap(6)
    .style(bg::tooltip)
    .into()
}

/// Knopf mit Text in einheitlicher Groesse und Polsterung.
pub fn button_text<'a, M: Clone + 'a>(label: impl text::IntoFragment<'a>) -> button::Button<'a, M> {
    button(text(label).size(size::SMALL).font(SEMIBOLD)).padding([7, 14])
}

/// Kleiner Knopf (in Listen und Karten).
pub fn button_small<'a, M: Clone + 'a>(
    label: impl text::IntoFragment<'a>,
) -> button::Button<'a, M> {
    button(text(label).size(size::CAPTION).font(SEMIBOLD)).padding([4, 10])
}

/// Quadratischer Symbolknopf mit Tooltip.
pub fn icon_button<'a, M: Clone + 'a>(
    glyph: &'a str,
    label: impl Into<String>,
    on_press: Option<M>,
) -> Element<'a, M> {
    tip(
        button(
            container(text(glyph).size(16).line_height(1.0))
                .center_x(Length::Fixed(18.0))
                .center_y(Length::Fixed(18.0)),
        )
        .padding(6)
        .style(btn::ghost)
        .on_press_maybe(on_press),
        label,
        tooltip::Position::Top,
    )
}

/// Waagerechte Trennlinie.
pub fn divider<'a, M: 'a>() -> Element<'a, M> {
    container(Space::new())
        .width(Length::Fill)
        .height(1)
        .style(bg::divider)
        .into()
}

/// Dauer als "m:ss" bzw. "h:mm:ss" (Gespraechsdauer).
pub fn fmt_duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Zaehler-Plakette ("3", "99+").
pub fn count_badge<'a, M: 'a>(n: u32, danger: bool) -> Element<'a, M> {
    let label = if n > 99 {
        "99+".to_owned()
    } else {
        n.to_string()
    };
    container(text(label).size(size::OVERLINE).font(BOLD).line_height(1.0))
        .padding([3, 6])
        .style(if danger {
            bg::badge_danger
        } else {
            bg::badge_accent
        })
        .into()
}

/// Kleine Markierung ("Admin", "LIVE").
pub fn tag<'a, M: 'a>(
    label: impl text::IntoFragment<'a>,
    style: fn(&Theme) -> container::Style,
) -> Element<'a, M> {
    container(text(label).size(size::OVERLINE).font(BOLD).line_height(1.0))
        .padding([3, 6])
        .style(style)
        .into()
}

/// Hinweisband: Info, Erfolg, Warnung oder Fehler.
#[derive(Clone, Copy)]
pub enum Tone {
    Info,
    Success,
    Warning,
    Danger,
}

pub fn banner<'a, M: 'a>(tone: Tone, content: impl Into<Element<'a, M>>) -> Element<'a, M> {
    container(content)
        .padding([10, 14])
        .width(Length::Fill)
        .style(match tone {
            Tone::Info => bg::info,
            Tone::Success => bg::success,
            Tone::Warning => bg::warning,
            Tone::Danger => bg::danger,
        })
        .into()
}

/// Hinweisband nur mit Text.
pub fn notice<'a, M: 'a>(tone: Tone, msg: impl text::IntoFragment<'a>) -> Element<'a, M> {
    banner(tone, text(msg).size(size::SMALL))
}

/// Karte mit Titel, Erklaerung und Inhalt.
pub fn section<'a, M: 'a>(
    title: impl text::IntoFragment<'a>,
    description: Option<&'a str>,
    body: impl Into<Element<'a, M>>,
) -> Element<'a, M> {
    let mut head = column![heading(title)].spacing(space::XS);
    if let Some(d) = description {
        head = head.push(muted(d));
    }
    container(column![head, body.into()].spacing(space::L))
        .padding(space::L + space::XS)
        .width(Length::Fill)
        .style(bg::card)
        .into()
}

/// Feld mit Beschriftung oben und optionalem Hinweis darunter.
pub fn field<'a, M: 'a>(
    label_text: impl text::IntoFragment<'a>,
    control: impl Into<Element<'a, M>>,
    hint: Option<String>,
) -> Element<'a, M> {
    let mut col = column![label(label_text), control.into()].spacing(6);
    if let Some(h) = hint {
        col = col.push(caption(h));
    }
    col.into()
}

/// Einstellung als Zeile: Beschriftung und Erklaerung links, Bedienelement rechts.
pub fn setting<'a, M: 'a>(
    label_text: impl text::IntoFragment<'a>,
    hint: Option<String>,
    control: impl Into<Element<'a, M>>,
) -> Element<'a, M> {
    let mut left = column![label(label_text)].spacing(2);
    if let Some(h) = hint {
        left = left.push(caption(h));
    }
    row![left.width(Length::Fill), control.into()]
        .spacing(space::L)
        .align_y(iced::Alignment::Center)
        .into()
}

/// Schieberegler mit Beschriftung und aktuellem Wert.
pub fn slider_field<'a, M: 'a>(
    label_text: &'a str,
    value: String,
    control: impl Into<Element<'a, M>>,
) -> Element<'a, M> {
    column![
        row![
            label(label_text).width(Length::Fill),
            text(value).size(size::CAPTION).style(tx::muted),
        ],
        control.into(),
    ]
    .spacing(6)
    .into()
}

/// Leerer Zustand: Symbol, Titel, Erklaerung, optional eine Aktion.
pub fn empty_state<'a, M: 'a>(
    glyph: &'a str,
    title_text: impl text::IntoFragment<'a>,
    description: impl text::IntoFragment<'a>,
    action: Option<Element<'a, M>>,
) -> Element<'a, M> {
    let mut col = column![
        container(text(glyph).size(28).line_height(1.0))
            .center_x(Length::Fixed(64.0))
            .center_y(Length::Fixed(64.0))
            .style(|t: &Theme| container::Style {
                background: Some(Background::Color(tokens(t).raised)),
                border: Border::default().rounded(radius::FULL),
                ..container::Style::default()
            }),
        heading(title_text),
        muted(description).align_x(iced::Alignment::Center),
    ]
    .spacing(space::S + space::XS)
    .max_width(380)
    .align_x(iced::Alignment::Center);
    if let Some(a) = action {
        col = col.push(Space::new().height(space::XS)).push(a);
    }
    container(col)
        .center(Length::Fill)
        .padding(space::XL)
        .into()
}

/// Seite mit lesbarer Hoechstbreite und Rand.
pub fn page<'a, M: 'a>(content: impl Into<Element<'a, M>>) -> Element<'a, M> {
    page_scroll(content).into()
}

/// Wie [`page`], aber mit Kennung, damit sich die Seite per Task scrollen laesst.
pub fn page_with_id<'a, M: 'a>(
    id: &'static str,
    content: impl Into<Element<'a, M>>,
) -> Element<'a, M> {
    page_scroll(content).id(id).into()
}

fn page_scroll<'a, M: 'a>(content: impl Into<Element<'a, M>>) -> scrollable::Scrollable<'a, M> {
    scroll(
        container(container(content).max_width(PAGE_WIDTH).width(Length::Fill))
            .padding([space::XL, space::XL + space::S])
            .center_x(Length::Fill),
    )
    .height(Length::Fill)
}

/// Kopfzeile eines Bereichs: Inhalt links, Aktionen rechts, Linie darunter.
pub fn header<'a, M: 'a>(
    left: impl Into<Element<'a, M>>,
    right: impl Into<Element<'a, M>>,
) -> Element<'a, M> {
    column![
        container(
            row![container(left.into()).width(Length::Fill), right.into()]
                .spacing(space::M)
                .align_y(iced::Alignment::Center)
        )
        .height(HEADER_HEIGHT)
        .padding([0, space::L as u16])
        .center_y(HEADER_HEIGHT),
        divider(),
    ]
    .into()
}

// ---------- Personen ----------

/// Feste Farbe je Person bzw. Space (aus dem Schluessel abgeleitet, also auf allen Geraeten gleich).
pub fn avatar_color(seed: &[u8]) -> Color {
    let h = seed.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    });
    let k = current();
    [
        k.avatar_1, k.avatar_2, k.avatar_3, k.avatar_4, k.avatar_5, k.avatar_6, k.avatar_7,
        k.avatar_8,
    ][(h % 8) as usize]
}

/// Anfangsbuchstabe(n) eines Namens.
/// Zusaetze wie "(du)" zaehlen nicht mit.
pub fn initials(name: &str) -> String {
    let mut words = name
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .filter(|c| c.is_alphanumeric());
    match (words.next(), words.next()) {
        (Some(a), Some(b)) => format!("{}{}", a.to_uppercase(), b.to_uppercase()),
        (Some(a), None) => a.to_uppercase().to_string(),
        _ => "?".to_owned(),
    }
}

/// Runder Avatar mit Initialen.
pub fn avatar<'a, M: 'a>(name: &str, seed: &[u8], diameter: f32) -> Element<'a, M> {
    avatar_ring(name, seed, diameter, None)
}

/// Avatar mit farbigem Ring (z. B. gruen, solange jemand spricht).
pub fn avatar_ring<'a, M: 'a>(
    name: &str,
    seed: &[u8],
    diameter: f32,
    ring: Option<fn(&Tokens) -> Color>,
) -> Element<'a, M> {
    let fill = avatar_color(seed);
    container(
        text(initials(name))
            .size((diameter * 0.40).round())
            .font(SEMIBOLD)
            .line_height(1.0)
            .color(current().on_avatar),
    )
    .center_x(Length::Fixed(diameter))
    .center_y(Length::Fixed(diameter))
    .style(move |t: &Theme| {
        let border = match ring {
            Some(f) => Border::default()
                .rounded(radius::FULL)
                .width(2)
                .color(f(tokens(t))),
            None => Border::default().rounded(radius::FULL),
        };
        container::Style {
            background: Some(Background::Color(fill)),
            border,
            ..container::Style::default()
        }
    })
    .into()
}

pub fn status_color(k: &Tokens, s: Option<PresenceStatus>) -> Color {
    match s {
        Some(PresenceStatus::Online) => k.success,
        Some(PresenceStatus::Away) => k.warning,
        Some(PresenceStatus::DoNotDisturb) => k.danger,
        _ => k.status_offline,
    }
}

/// Color list with labels in the current UI language.
pub fn token_meta() -> &'static [(&'static str, &'static str, &'static str)] {
    if app_core::i18n::is_german() {
        TOKEN_META_DE
    } else {
        TOKEN_META
    }
}

pub fn status_label(s: Option<PresenceStatus>) -> &'static str {
    match s {
        Some(PresenceStatus::Online) => "Online",
        Some(PresenceStatus::Away) => app_core::i18n::tr("Away", "Abwesend"),
        Some(PresenceStatus::DoNotDisturb) => {
            app_core::i18n::tr("Do not disturb", "Bitte nicht stören")
        }
        Some(PresenceStatus::Invisible) => app_core::i18n::tr("Invisible", "Unsichtbar"),
        None => "Offline",
    }
}

/// Status-Punkt mit Ring in der Farbe des Untergrunds.
pub fn status_dot<'a, M: 'a>(
    s: Option<PresenceStatus>,
    diameter: f32,
    ring: fn(&Tokens) -> Color,
) -> Element<'a, M> {
    container(Space::new())
        .width(diameter)
        .height(diameter)
        .style(move |t: &Theme| {
            let k = tokens(t);
            container::Style {
                background: Some(Background::Color(status_color(k, s))),
                border: Border::default()
                    .rounded(radius::FULL)
                    .width(2.5)
                    .color(ring(k)),
                ..container::Style::default()
            }
        })
        .into()
}

/// Avatar mit Status-Punkt unten rechts.
pub fn avatar_status<'a, M: 'a>(
    name: &str,
    seed: &[u8],
    diameter: f32,
    s: Option<PresenceStatus>,
    ring: fn(&Tokens) -> Color,
) -> Element<'a, M> {
    let dot = (diameter * 0.38).max(10.0);
    iced::widget::stack![
        avatar(name, seed, diameter),
        container(status_dot(s, dot, ring))
            .align_right(Length::Fixed(diameter + 2.0))
            .align_bottom(Length::Fixed(diameter + 2.0)),
    ]
    .width(Length::Fixed(diameter + 2.0))
    .height(Length::Fixed(diameter + 2.0))
    .into()
}

/// Untergrund-Farben fuer `status_dot`-Ringe.
pub fn on_rail(k: &Tokens) -> Color {
    k.rail
}
pub fn on_sidebar(k: &Tokens) -> Color {
    k.sidebar
}
pub fn on_content(k: &Tokens) -> Color {
    k.content
}
pub fn on_surface(k: &Tokens) -> Color {
    k.surface
}
pub fn speaking_ring(k: &Tokens) -> Color {
    k.success_fg
}

/// Firmenzeichen: abgerundetes Quadrat mit "K".
pub fn logo<'a, M: 'a>(edge: f32) -> Element<'a, M> {
    container(
        text("K")
            .size((edge * 0.5).round())
            .font(BOLD)
            .line_height(1.0)
            .color(current().on_avatar),
    )
    .center_x(Length::Fixed(edge))
    .center_y(Length::Fixed(edge))
    .style(move |t: &Theme| container::Style {
        background: Some(Background::Color(tokens(t).accent)),
        border: Border::default().rounded(Radius::from(edge * 0.3)),
        shadow: Shadow {
            color: tokens(t).accent.scale_alpha(0.35),
            offset: Vector::new(0.0, 4.0),
            blur_radius: 14.0,
        },
        ..container::Style::default()
    })
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lum(c: Color) -> f32 {
        let f = |x: f32| {
            if x <= 0.039_28 {
                x / 12.92
            } else {
                ((x + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b)
    }

    fn contrast(a: Color, b: Color) -> f32 {
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn text_colors_meet_wcag_aa_on_every_background() {
        for k in [&DARK, &LIGHT] {
            for bgc in [k.sidebar, k.content, k.surface] {
                assert!(contrast(k.text, bgc) >= 7.0, "Text");
                assert!(contrast(k.text_muted, bgc) >= 4.5, "gedaempft");
                assert!(contrast(k.text_faint, bgc) >= 4.5, "schwach");
                assert!(contrast(k.danger_fg, bgc) >= 4.5, "Fehler");
            }
            assert!(contrast(k.accent_fg, k.content) >= 4.5, "Akzent-Text");
            assert!(contrast(k.on_accent, k.accent) >= 4.5, "Text auf Akzent");
        }
    }

    #[test]
    fn initials_and_colors_are_stable() {
        assert_eq!(initials("tobi"), "T");
        assert_eq!(initials("Max Mustermann"), "MM");
        assert_eq!(initials("  "), "?");
        assert_eq!(initials("Tobi (du)"), "T");
        assert_eq!(avatar_color(b"abc"), avatar_color(b"abc"));
    }
}

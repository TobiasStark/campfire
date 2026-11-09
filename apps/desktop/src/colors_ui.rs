//! Farben-Editor: Jede Farbe der Oberflaeche (siehe `ui::TOKEN_META`) laesst sich einzeln
//! aendern, zuruecksetzen, als Voreinstellung waehlen sowie exportieren und importieren.
//! Bearbeitet wird immer die gerade aktive Variante (dunkel oder hell).

use crate::ui::{self, btn, size, space, TOKEN_META};
use app_core::settings::{ColorOverrides, ThemeChoice};
use iced::widget::{button, column, container, row, text, Space};
use iced::{Alignment, Background, Border, Color, Element, Length, Task, Theme};

#[derive(Debug, Clone)]
pub enum ColorMsg {
    Toggle(&'static str),
    Search(String),
    Hex(&'static str, String),
    /// 0 = Rot, 1 = Gruen, 2 = Blau, 3 = Deckkraft; Wert 0..=255.
    Channel(&'static str, usize, f32),
    Reset(&'static str),
    ResetAll,
    Preset(usize),
    Export,
    Import,
    Imported(Option<String>),
}

#[derive(Default)]
pub struct State {
    open: Option<&'static str>,
    search: String,
    /// Noch unvollstaendige Hex-Eingabe: (Farbe, Text).
    draft: Option<(&'static str, String)>,
    pub info: Option<String>,
}

/// Voreinstellung: Name, nur dunkel?, Farben (Name, Hex).
struct Preset {
    name: &'static str,
    dark_only: bool,
    colors: &'static [(&'static str, &'static str)],
}

const ACCENT_KEYS: [&str; 6] = [
    "accent",
    "accent_hover",
    "accent_pressed",
    "accent_fg",
    "accent_soft",
    "on_accent",
];

const PRESETS: &[Preset] = &[
    Preset {
        name: "Standard",
        dark_only: false,
        colors: &[],
    },
    Preset {
        name: "Ozean",
        dark_only: false,
        colors: &[("accent", "#1E88E5")],
    },
    Preset {
        name: "Wald",
        dark_only: false,
        colors: &[("accent", "#2E9E5B")],
    },
    Preset {
        name: "Rose",
        dark_only: false,
        colors: &[("accent", "#E0578A")],
    },
    Preset {
        name: "Bernstein",
        dark_only: false,
        colors: &[("accent", "#E69A1C")],
    },
    Preset {
        name: "Violett",
        dark_only: false,
        colors: &[("accent", "#8E5CE0")],
    },
    Preset {
        name: "Mitternacht (OLED)",
        dark_only: true,
        colors: &[
            ("rail", "#000000"),
            ("sidebar", "#050608"),
            ("content", "#000000"),
            ("surface", "#0D0E11"),
            ("raised", "#181920"),
            ("raised_hover", "#23242C"),
            ("field", "#000000"),
            ("border", "#1A1B21"),
            ("border_strong", "#33353E"),
        ],
    },
];

fn known(key: &str) -> bool {
    TOKEN_META.iter().any(|m| m.0 == key)
}

fn set(ov: &mut ColorOverrides, theme: ThemeChoice, key: &str, c: Color) {
    ov.map_mut(theme).insert(key.to_owned(), ui::to_hex(c));
}

/// Nur bekannte Farben mit gueltigem Hex-Wert uebernehmen.
fn sanitize(m: &mut std::collections::BTreeMap<String, String>) {
    m.retain(|k, v| known(k) && ui::parse_hex(v).is_some());
    for v in m.values_mut() {
        if let Some(c) = ui::parse_hex(v) {
            *v = ui::to_hex(c);
        }
    }
}

/// Verarbeitet eine Nachricht. Gibt `true` zurueck, wenn sich die Farben geaendert haben
/// (dann uebernehmen und speichern).
pub fn update(
    ov: &mut ColorOverrides,
    st: &mut State,
    theme: ThemeChoice,
    msg: ColorMsg,
) -> (bool, Task<ColorMsg>) {
    let mut changed = false;
    match msg {
        ColorMsg::Toggle(k) => {
            st.open = (st.open != Some(k)).then_some(k);
            st.draft = None;
        }
        ColorMsg::Search(s) => st.search = s,
        ColorMsg::Hex(k, s) => {
            if let Some(c) = ui::parse_hex(&s) {
                set(ov, theme, k, c);
                changed = true;
            }
            st.draft = Some((k, s));
        }
        ColorMsg::Channel(k, ch, v) => {
            if let Some(mut c) = ui::current().get(k) {
                let v = (v / 255.0).clamp(0.0, 1.0);
                match ch {
                    0 => c.r = v,
                    1 => c.g = v,
                    2 => c.b = v,
                    _ => c.a = v,
                }
                set(ov, theme, k, c);
                st.draft = None;
                changed = true;
            }
        }
        ColorMsg::Reset(k) => {
            changed = ov.map_mut(theme).remove(k).is_some();
            st.draft = None;
        }
        ColorMsg::ResetAll => {
            changed = !ov.map(theme).is_empty();
            ov.map_mut(theme).clear();
            st.draft = None;
            st.info = Some("Alle Farben dieser Variante zurückgesetzt.".into());
        }
        ColorMsg::Preset(i) => {
            if let Some(p) = PRESETS.get(i) {
                let m = ov.map_mut(theme);
                // Akzent-Voreinstellungen lassen eigene Hintergruende unangetastet.
                let accent_only =
                    !p.colors.is_empty() && p.colors.iter().all(|(k, _)| *k == "accent");
                if accent_only {
                    m.retain(|k, _| !ACCENT_KEYS.contains(&k.as_str()));
                } else {
                    m.clear();
                }
                for (k, v) in p.colors {
                    m.insert((*k).to_owned(), (*v).to_owned());
                }
                st.draft = None;
                st.info = Some(format!("Voreinstellung „{}“ angewendet.", p.name));
                changed = true;
            }
        }
        ColorMsg::Export => {
            let json = serde_json::to_string_pretty(ov).unwrap_or_default();
            st.info = Some("Farben in die Zwischenablage kopiert.".into());
            return (false, iced::clipboard::write(json));
        }
        ColorMsg::Import => return (false, iced::clipboard::read().map(ColorMsg::Imported)),
        ColorMsg::Imported(text) => {
            match text.and_then(|t| serde_json::from_str::<ColorOverrides>(&t).ok()) {
                Some(mut n) => {
                    sanitize(&mut n.dark);
                    sanitize(&mut n.light);
                    *ov = n;
                    st.draft = None;
                    st.info = Some("Farben aus der Zwischenablage übernommen.".into());
                    changed = true;
                }
                None => st.info = Some(
                    "In der Zwischenablage liegen keine gültigen Farben (zuerst „Exportieren“)."
                        .into(),
                ),
            }
        }
    }
    (changed, Task::none())
}

fn swatch<'a, M: 'a>(c: Color, edge: f32) -> Element<'a, M> {
    container(Space::new())
        .width(edge)
        .height(edge)
        .style(move |t: &Theme| container::Style {
            background: Some(Background::Color(c)),
            border: Border {
                color: ui::tokens(t).border_strong,
                width: 1.0,
                radius: ui::radius::S.into(),
            },
            ..container::Style::default()
        })
        .into()
}

fn channels<'a>(key: &'static str, c: Color) -> Element<'a, ColorMsg> {
    let v = [c.r, c.g, c.b, c.a].map(|x| (x * 255.0).round());
    let line = |name: &'static str, i: usize| {
        row![
            container(ui::caption(name)).width(Length::Fixed(80.0)),
            ui::slider(0.0..=255.0, v[i], move |x| ColorMsg::Channel(key, i, x)).step(1.0),
            container(ui::caption(format!("{}", v[i] as u32)))
                .width(Length::Fixed(34.0))
                .align_x(Alignment::End),
        ]
        .spacing(space::S)
        .align_y(Alignment::Center)
    };
    column![
        line("Rot", 0),
        line("Grün", 1),
        line("Blau", 2),
        line("Deckkraft", 3)
    ]
    .spacing(space::XS)
    .into()
}

pub fn view<'a>(
    ov: &'a ColorOverrides,
    st: &'a State,
    theme: ThemeChoice,
) -> Element<'a, ColorMsg> {
    let cur = ui::current();
    let def = ui::defaults(theme);
    let custom = ov.map(theme);
    let q = st.search.trim().to_lowercase();

    let presets = row(PRESETS
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.dark_only || theme == ThemeChoice::Dark)
        .map(|(i, p)| {
            Element::from(
                ui::button_small(p.name)
                    .style(btn::secondary)
                    .on_press(ColorMsg::Preset(i)),
            )
        }))
    .spacing(6)
    .wrap();

    let tools = column![
        presets,
        ui::input("Farbe suchen …", &st.search).on_input(ColorMsg::Search),
        row![
            ui::button_small("Exportieren")
                .style(btn::secondary)
                .on_press(ColorMsg::Export),
            ui::button_small("Importieren")
                .style(btn::secondary)
                .on_press(ColorMsg::Import),
            Space::new().width(Length::Fill),
            ui::button_small("Alle zurücksetzen")
                .style(btn::danger_ghost)
                .on_press_maybe((!custom.is_empty()).then_some(ColorMsg::ResetAll)),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    ]
    .spacing(space::S);

    let mut out = column![ui::section(
        "Voreinstellungen",
        Some("Ein Klick setzt die Farben dieser Variante. Danach lässt sich jede Farbe einzeln ändern."),
        tools
    )]
    .spacing(space::L);
    if let Some(i) = &st.info {
        out = out.push(ui::notice(ui::Tone::Info, i.as_str()));
    }

    let mut groups: Vec<&'static str> = Vec::new();
    for m in TOKEN_META {
        if !groups.contains(&m.2) {
            groups.push(m.2);
        }
    }
    for g in groups {
        let mut rows = column![].spacing(2);
        let mut any = false;
        for &(key, label, _) in TOKEN_META.iter().filter(|m| m.2 == g) {
            if !q.is_empty() && !label.to_lowercase().contains(&q) && !key.contains(&q) {
                continue;
            }
            any = true;
            let c = cur.get(key).unwrap_or(Color::BLACK);
            let is_custom = custom.contains_key(key);
            let open = st.open == Some(key);
            let mut head = row![
                swatch(c, 22.0),
                text(label)
                    .size(size::BODY)
                    .style(ui::tx::normal)
                    .width(Length::Fill),
            ]
            .spacing(space::M)
            .align_y(Alignment::Center);
            if is_custom {
                head = head.push(ui::tag("angepasst", ui::bg::badge_accent));
            }
            head = head.push(
                text(ui::to_hex(c))
                    .size(size::CAPTION)
                    .font(iced::Font::MONOSPACE)
                    .style(ui::tx::muted),
            );
            rows = rows.push(
                button(head)
                    .padding([6, 8])
                    .width(Length::Fill)
                    .style(btn::nav(open))
                    .on_press(ColorMsg::Toggle(key)),
            );
            if open {
                let shown = match &st.draft {
                    Some((k, s)) if *k == key => s.clone(),
                    _ => ui::to_hex(c),
                };
                let valid = ui::parse_hex(&shown).is_some();
                let d = def.get(key).unwrap_or(Color::BLACK);
                let mut hex = row![
                    container(ui::caption("Hex")).width(Length::Fixed(80.0)),
                    ui::input("#RRGGBB oder #RRGGBBAA", &shown)
                        .on_input(move |s| ColorMsg::Hex(key, s))
                        .width(Length::Fixed(190.0)),
                ]
                .spacing(space::S)
                .align_y(Alignment::Center);
                if !valid {
                    hex = hex.push(ui::caption("ungültig"));
                }
                hex = hex
                    .push(Space::new().width(Length::Fill))
                    .push(ui::caption(format!("Standard {}", ui::to_hex(d))))
                    .push(
                        ui::button_small("Zurücksetzen")
                            .style(btn::secondary)
                            .on_press_maybe(is_custom.then_some(ColorMsg::Reset(key))),
                    );
                rows = rows.push(
                    container(column![channels(key, c), hex].spacing(space::S))
                        .padding([space::S, space::L]),
                );
            }
        }
        if any {
            out = out.push(ui::section(g, None, rows));
        }
    }
    out.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_token_has_unique_key_and_roundtrips_hex() {
        let mut seen = std::collections::HashSet::new();
        for (k, _, _) in TOKEN_META {
            assert!(seen.insert(*k), "doppelt: {k}");
            let c = ui::defaults(ThemeChoice::Dark).get(k).unwrap();
            assert_eq!(
                ui::parse_hex(&ui::to_hex(c)).map(ui::to_hex),
                Some(ui::to_hex(c))
            );
        }
        assert_eq!(ui::parse_hex("#zzzzzz"), None);
        assert_eq!(ui::parse_hex("#12345"), None);
        assert_eq!(ui::parse_hex("ÄÄÄÄ"), None);
    }

    #[test]
    fn override_wins_and_accent_derives_dependents() {
        let mut m = std::collections::BTreeMap::new();
        m.insert("accent".to_owned(), "#FF0000".to_owned());
        let t = ui::build_tokens(ui::defaults(ThemeChoice::Dark), &m);
        assert_eq!(ui::to_hex(t.accent), "#FF0000");
        assert_ne!(t.accent_hover, ui::defaults(ThemeChoice::Dark).accent_hover);
        m.insert("accent_hover".to_owned(), "#00FF00".to_owned());
        let t = ui::build_tokens(ui::defaults(ThemeChoice::Dark), &m);
        assert_eq!(ui::to_hex(t.accent_hover), "#00FF00");
        // Kaputte Werte werden ignoriert.
        m.insert("rail".to_owned(), "kaputt".to_owned());
        let t = ui::build_tokens(ui::defaults(ThemeChoice::Dark), &m);
        assert_eq!(t.rail, ui::defaults(ThemeChoice::Dark).rail);
    }

    #[test]
    fn sanitize_drops_unknown_and_invalid() {
        let mut m = std::collections::BTreeMap::new();
        m.insert("accent".into(), "1e88e5".into());
        m.insert("nope".into(), "#000000".into());
        m.insert("rail".into(), "xx".into());
        sanitize(&mut m);
        assert_eq!(m.len(), 1);
        assert_eq!(m["accent"], "#1E88E5");
    }
}

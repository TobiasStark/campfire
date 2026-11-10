//! Overlay (M1-15): kleines transparentes, klick-durchlaessiges Fenster ueber dem Spiel.
//! Es ist ein normales Fenster (keine Injection) und zeigt, wer im Channel ist und wer spricht.

use crate::core::Core;
use app_core::friends::Friend;
use app_proto::lock::Locked;
use iced::widget::{column, container, text};
use iced::{Color, Element, Point, Size};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlaySettings {
    pub enabled: bool,
    /// 0 = oben links, 1 = oben rechts, 2 = unten links, 3 = unten rechts
    pub corner: u8,
    /// Groesse, 0.7 bis 2.0
    pub scale: f32,
    /// Deckkraft des Hintergrunds, 0.1 bis 1.0
    pub opacity: f32,
    pub only_speaking: bool,
}

impl Default for OverlaySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            corner: 1,
            scale: 1.0,
            opacity: 0.7,
            only_speaking: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub name: String,
    pub speaking: bool,
    pub muted: bool,
}

const MARGIN: f32 = 20.0;
const BASE_W: f32 = 240.0;
const BASE_ROW: f32 = 28.0;
const PAD: f32 = 10.0;

/// Eintraege des aktuellen Channels/Gespraechs, bei Bedarf nur die Sprechenden.
pub fn rows(core: &Core, friends: &[Friend], s: &OverlaySettings) -> Vec<Row> {
    let (muted, deafened) = core.voice.local_state();
    let (level, open) = core.input_level();
    let speaking_devices = core.voice.core().locked().speaking();
    let mut v = vec![Row {
        name: core.name.clone(),
        speaking: open && !muted && !deafened && level > app_audio::voice::SPEAKING_LEVEL,
        muted: muted || deafened,
    }];
    for p in core.voice.participants() {
        let name = friends.iter().find(|f| f.user == p.user).map_or_else(
            || app_core::i18n::tr("Unknown", "Unbekannt").to_owned(),
            |f| f.name.clone(),
        );
        v.push(Row {
            name,
            speaking: speaking_devices.contains(&p.device),
            muted: p.muted || p.deafened,
        });
    }
    if s.only_speaking {
        v.retain(|r| r.speaking);
    }
    v
}

/// Fenstergroesse und -position (logische Pixel) fuer `n` Zeilen.
pub fn geometry(s: &OverlaySettings, n: usize, screen: Size) -> (Size, Point) {
    let k = s.scale.clamp(0.7, 2.0);
    let size = Size::new(
        BASE_W * k,
        (BASE_ROW * k * n.max(1) as f32 + 2.0 * PAD * k).ceil(),
    );
    let x = if s.corner.is_multiple_of(2) {
        MARGIN
    } else {
        (screen.width - size.width - MARGIN).max(0.0)
    };
    let y = if s.corner < 2 {
        MARGIN
    } else {
        (screen.height - size.height - MARGIN).max(0.0)
    };
    (size, Point::new(x, y))
}

pub fn view<'a, M: 'a>(s: &OverlaySettings, rows: &[Row]) -> Element<'a, M> {
    let k = s.scale.clamp(0.7, 2.0);
    let ck = crate::ui::current();
    let mut col = column![].spacing(2.0 * k);
    for r in rows {
        let (dot, color) = if r.speaking {
            ("●", ck.overlay_speaking)
        } else {
            ("○", ck.overlay_idle)
        };
        let label = format!(
            "{dot} {}{}",
            r.name,
            if r.muted {
                app_core::i18n::tr(" (muted)", " (stumm)")
            } else {
                ""
            }
        );
        col = col.push(text(label).size(18.0 * k).color(if r.speaking {
            ck.overlay_speaking_text
        } else {
            color
        }));
    }
    let bg = if rows.is_empty() {
        0.0
    } else {
        s.opacity.clamp(0.1, 1.0)
    };
    container(col)
        .padding(PAD * k)
        .style(move |_| container::Style {
            background: Some(
                Color {
                    a: bg,
                    ..ck.overlay_bg
                }
                .into(),
            ),
            border: iced::Border {
                radius: (10.0 * k).into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_and_scaling() {
        let screen = Size::new(1920.0, 1080.0);
        let mut s = OverlaySettings {
            corner: 0,
            ..OverlaySettings::default()
        };
        let (sz, p) = geometry(&s, 3, screen);
        assert_eq!((p.x, p.y), (MARGIN, MARGIN));
        s.corner = 3;
        let (sz2, p2) = geometry(&s, 3, screen);
        assert_eq!(sz, sz2);
        assert_eq!(p2.x + sz.width + MARGIN, screen.width);
        assert_eq!(p2.y + sz.height + MARGIN, screen.height);
        s.scale = 2.0;
        let (big, _) = geometry(&s, 3, screen);
        assert!(big.width > sz.width * 1.9);
        // Mindestens eine Zeile hoch, auch ohne Eintraege.
        assert!(geometry(&s, 0, screen).0.height > 0.0);
        // Alte oder unvollstaendige Einstellungen bleiben lesbar.
        let d: OverlaySettings = OverlaySettings::default();
        assert!(!d.enabled);
    }
}

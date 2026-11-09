//! Encoder-Auswahl (M3-02), ohne ffmpeg: Optionen, Rangfolge, Rueckfall.
//!
//! "Auto" waehlt den besten Hardware-Encoder und den effizientesten Codec, den **alle**
//! Zuschauer dekodieren koennen. H.264 koennen alle (`DecoderCaps` enthaelt es immer); ohne
//! Zuschauer sind alle Codecs erlaubt, ein spaeter hinzukommender Zuschauer kann einen
//! Wechsel nach sich ziehen.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub const ALL: [Codec; 3] = [Codec::H264, Codec::Hevc, Codec::Av1];

    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
        }
    }

    fn bit(self) -> u8 {
        match self {
            Codec::H264 => 1,
            Codec::Hevc => 2,
            Codec::Av1 => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Backend {
    Nvenc,
    Amf,
    QuickSync,
    Software,
}

impl Backend {
    pub const ALL: [Backend; 4] = [
        Backend::Nvenc,
        Backend::Amf,
        Backend::QuickSync,
        Backend::Software,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Backend::Nvenc => "NVENC (NVIDIA)",
            Backend::Amf => "AMF (AMD)",
            Backend::QuickSync => "QuickSync (Intel)",
            Backend::Software => "Software",
        }
    }

    pub fn is_hardware(self) -> bool {
        self != Backend::Software
    }

    /// Reihenfolge bei Gleichstand: NVENC vor AMF vor QuickSync.
    fn rank(self) -> u8 {
        match self {
            Backend::Nvenc => 0,
            Backend::Amf => 1,
            Backend::QuickSync => 2,
            Backend::Software => 3,
        }
    }
}

/// Ein Encoder, den ffmpeg kennen kann.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EncoderInfo {
    /// ffmpeg-Name, z. B. `h264_nvenc`.
    pub name: &'static str,
    pub codec: Codec,
    pub backend: Backend,
}

/// Alle Kandidaten. Media Foundation (`h264_mf`) fehlt absichtlich: schwache Qualitaet.
/// `libsvtav1` ist zu schwer fuer Echtzeit-Spiele und nur ein letzter Ausweg.
pub const CANDIDATES: &[EncoderInfo] = &[
    EncoderInfo {
        name: "h264_nvenc",
        codec: Codec::H264,
        backend: Backend::Nvenc,
    },
    EncoderInfo {
        name: "hevc_nvenc",
        codec: Codec::Hevc,
        backend: Backend::Nvenc,
    },
    EncoderInfo {
        name: "av1_nvenc",
        codec: Codec::Av1,
        backend: Backend::Nvenc,
    },
    EncoderInfo {
        name: "h264_amf",
        codec: Codec::H264,
        backend: Backend::Amf,
    },
    EncoderInfo {
        name: "hevc_amf",
        codec: Codec::Hevc,
        backend: Backend::Amf,
    },
    EncoderInfo {
        name: "av1_amf",
        codec: Codec::Av1,
        backend: Backend::Amf,
    },
    EncoderInfo {
        name: "h264_qsv",
        codec: Codec::H264,
        backend: Backend::QuickSync,
    },
    EncoderInfo {
        name: "hevc_qsv",
        codec: Codec::Hevc,
        backend: Backend::QuickSync,
    },
    EncoderInfo {
        name: "av1_qsv",
        codec: Codec::Av1,
        backend: Backend::QuickSync,
    },
    EncoderInfo {
        name: "libopenh264",
        codec: Codec::H264,
        backend: Backend::Software,
    },
    EncoderInfo {
        name: "libsvtav1",
        codec: Codec::Av1,
        backend: Backend::Software,
    },
];

/// Welche Codecs ein Zuschauer dekodieren kann. H.264 ist immer dabei.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecoderCaps(u8);

impl DecoderCaps {
    pub const H264_ONLY: DecoderCaps = DecoderCaps(1);

    pub fn new(codecs: &[Codec]) -> Self {
        Self(codecs.iter().fold(Codec::H264.bit(), |a, c| a | c.bit()))
    }

    pub fn supports(self, c: Codec) -> bool {
        self.0 & c.bit() != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum EncoderChoice {
    #[default]
    Auto,
    Only(Backend),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CodecChoice {
    #[default]
    Auto,
    Only(Codec),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Resolution {
    P720,
    #[default]
    P1080,
    P1440,
    Native,
}

impl Resolution {
    pub const ALL: [Resolution; 4] = [
        Resolution::P720,
        Resolution::P1080,
        Resolution::P1440,
        Resolution::Native,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Resolution::P720 => "720p",
            Resolution::P1080 => "1080p",
            Resolution::P1440 => "1440p",
            Resolution::Native => "Original",
        }
    }

    pub fn lines(self) -> Option<u32> {
        match self {
            Resolution::P720 => Some(720),
            Resolution::P1080 => Some(1080),
            Resolution::P1440 => Some(1440),
            Resolution::Native => None,
        }
    }

    /// Zielgroesse bei Quellgroesse `w`x`h`: Seitenverhaeltnis bleibt, nie hochskaliert,
    /// beide Seiten gerade (Encoder-Pflicht).
    pub fn target(self, w: u32, h: u32) -> (u32, u32) {
        let (w, h) = (w.max(2), h.max(2));
        let (tw, th) = match self.lines() {
            Some(l) if h > l => ((u64::from(w) * u64::from(l) / u64::from(h)) as u32, l),
            _ => (w, h),
        };
        (tw.max(2) & !1, th.max(2) & !1)
    }
}

/// Welcher Ton in den Stream kommt (M3-07).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AudioMode {
    Off,
    /// Alles, was der PC abspielt, ausser Campfire selbst (also ohne den Voice-Chat).
    #[default]
    AllExceptApp,
    /// Nur das gestreamte Programm (geht nur bei einem Fenster als Quelle).
    OnlyProgram,
}

impl AudioMode {
    pub const ALL: [AudioMode; 3] = [
        AudioMode::Off,
        AudioMode::AllExceptApp,
        AudioMode::OnlyProgram,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AudioMode::Off => "Kein Ton",
            AudioMode::AllExceptApp => "Alles außer dieser App",
            AudioMode::OnlyProgram => "Nur das gestreamte Programm",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamOptions {
    pub encoder: EncoderChoice,
    pub codec: CodecChoice,
    pub resolution: Resolution,
    /// 30 oder 60.
    pub fps: u32,
    /// Obergrenze der Bitrate in Mbit/s.
    pub max_mbps: u32,
    /// Zusaetzlich eine kleine Stufe (720p30) fuer Zuschauer mit schwacher Leitung (M3-05).
    /// Kostet einen zweiten Encoder, deshalb abschaltbar.
    #[serde(default)]
    pub simulcast: bool,
    /// Ton im Stream; Bitrate fest 160 kbit/s Stereo.
    #[serde(default)]
    pub audio: AudioMode,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            encoder: EncoderChoice::Auto,
            codec: CodecChoice::Auto,
            resolution: Resolution::P1080,
            fps: 60,
            max_mbps: 12,
            simulcast: false,
            audio: AudioMode::default(),
        }
    }
}

impl StreamOptions {
    /// Bringt Werte aus Dateien in einen sinnvollen Bereich.
    pub fn sanitized(mut self) -> Self {
        self.fps = if self.fps >= 45 { 60 } else { 30 };
        self.max_mbps = self.max_mbps.clamp(1, 100);
        self
    }

    /// Die Stufen, die kodiert werden: Stufe 0 sind diese Optionen, Stufe 1 (falls Simulcast
    /// an und sinnvoll) ist 720p30 mit hoechstens 3 Mbit/s. Eine Stufe, die nicht kleiner
    /// waere als die Originalstufe, entfaellt.
    pub fn layers(&self) -> Vec<StreamOptions> {
        let top = self.sanitized();
        let mut v = vec![top];
        let small_enough =
            (top.fps <= 30 && top.resolution == Resolution::P720) || top.max_mbps <= 3;
        if top.simulcast && !small_enough {
            v.push(StreamOptions {
                resolution: Resolution::P720,
                fps: 30,
                max_mbps: top.max_mbps.min(3),
                simulcast: false,
                ..top
            });
        }
        v
    }
}

/// Ergebnis der Auswahl: der beste Encoder, die Ersatzliste in Reihenfolge und ein Hinweis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub chain: Vec<EncoderInfo>,
    pub note: Option<String>,
}

impl Selection {
    pub fn best(&self) -> Option<EncoderInfo> {
        self.chain.first().copied()
    }
}

/// Rangfolge fuer "Auto": Hardware vor Software, dann effizientester Codec, dann Hersteller.
fn auto_key(e: &EncoderInfo) -> (bool, std::cmp::Reverse<Codec>, u8) {
    (
        !e.backend.is_hardware(),
        std::cmp::Reverse(e.codec),
        e.backend.rank(),
    )
}

/// Waehlt aus den tatsaechlich funktionierenden Encodern (`available`).
pub fn choose(
    opts: &StreamOptions,
    available: &[EncoderInfo],
    viewers: &[DecoderCaps],
) -> Selection {
    let viewers_ok = |c: Codec| viewers.iter().all(|v| v.supports(c));
    let mut notes = Vec::new();

    let mut pool: Vec<EncoderInfo> = available.to_vec();
    if let EncoderChoice::Only(b) = opts.encoder {
        if pool.iter().any(|e| e.backend == b) {
            pool.retain(|e| e.backend == b);
        } else {
            notes.push(format!(
                "{} ist nicht verfügbar, Auto statt dessen",
                b.label()
            ));
        }
    }
    if let CodecChoice::Only(c) = opts.codec {
        if pool.iter().any(|e| e.codec == c) {
            pool.retain(|e| e.codec == c);
            if !viewers_ok(c) {
                notes.push(format!(
                    "Nicht alle Zuschauer können {} dekodieren",
                    c.label()
                ));
            }
        } else {
            notes.push(format!(
                "{} ist nicht verfügbar, Auto statt dessen",
                c.label()
            ));
        }
    } else {
        // Auto: nur Codecs, die alle Zuschauer koennen.
        let ok: Vec<EncoderInfo> = pool
            .iter()
            .filter(|e| viewers_ok(e.codec))
            .copied()
            .collect();
        if !ok.is_empty() {
            pool = ok;
        }
    }
    // Software-AV1 ist fuer Spiele zu langsam: nur, wenn nichts anderes da ist.
    pool.sort_by_key(|e| (e.name == "libsvtav1", auto_key(e)));

    // Ersatzkette: erst die gewaehlte Menge, dann alles uebrige, was die Zuschauer koennen.
    let mut chain = pool;
    let mut rest: Vec<EncoderInfo> = available
        .iter()
        .filter(|e| !chain.contains(e) && viewers_ok(e.codec))
        .copied()
        .collect();
    rest.sort_by_key(|e| (e.name == "libsvtav1", auto_key(e)));
    chain.extend(rest);

    if chain.is_empty() {
        notes.push("Kein funktionierender Encoder gefunden".into());
    }
    Selection {
        chain,
        note: (!notes.is_empty()).then(|| notes.join("; ")),
    }
}

/// Merkt sich, welcher Encoder der Kette gerade laeuft; bei Ausfall geht es zum naechsten.
#[derive(Debug, Clone)]
pub struct Failover {
    chain: Vec<EncoderInfo>,
    idx: usize,
}

impl Failover {
    pub fn new(sel: &Selection) -> Self {
        Self {
            chain: sel.chain.clone(),
            idx: 0,
        }
    }

    pub fn current(&self) -> Option<EncoderInfo> {
        self.chain.get(self.idx).copied()
    }

    /// Der aktuelle Encoder ist ausgefallen: weiter zum naechsten, mit Hinweis fuer den Nutzer.
    pub fn fail(&mut self) -> Option<(EncoderInfo, String)> {
        let old = self.current()?;
        self.idx += 1;
        let new = self.current()?;
        Some((
            new,
            format!(
                "Encoder {} ist ausgefallen, wechsle zu {} ({})",
                old.name,
                new.name,
                new.backend.label()
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by(names: &[&str]) -> Vec<EncoderInfo> {
        CANDIDATES
            .iter()
            .filter(|c| names.contains(&c.name))
            .copied()
            .collect()
    }

    fn names(s: &Selection) -> Vec<&'static str> {
        s.chain.iter().map(|e| e.name).collect()
    }

    #[test]
    fn auto_prefers_hardware_then_efficient_codec_that_all_viewers_decode() {
        let avail = by(&[
            "h264_nvenc",
            "hevc_nvenc",
            "av1_nvenc",
            "h264_amf",
            "libopenh264",
        ]);
        let all = DecoderCaps::new(&Codec::ALL);
        let o = StreamOptions::default();
        // Niemand schaut zu: nur die sichere Grundlage H.264 zaehlt nicht als Zwang, alle
        // Codecs sind erlaubt -> AV1 auf NVENC.
        assert_eq!(choose(&o, &avail, &[]).best().unwrap().name, "av1_nvenc");
        assert_eq!(choose(&o, &avail, &[all]).best().unwrap().name, "av1_nvenc");
        // Ein Zuschauer ohne AV1 -> HEVC.
        let hevc = DecoderCaps::new(&[Codec::Hevc]);
        assert_eq!(
            choose(&o, &avail, &[all, hevc]).best().unwrap().name,
            "hevc_nvenc"
        );
        // Ein Zuschauer, der nur H.264 kann -> H.264 auf NVENC, dann AMF, dann Software.
        let s = choose(&o, &avail, &[all, DecoderCaps::H264_ONLY]);
        assert_eq!(names(&s)[..3], ["h264_nvenc", "h264_amf", "libopenh264"]);
        // Die Ersatzkette enthaelt nie einen Codec, den ein Zuschauer nicht kann.
        assert!(s.chain.iter().all(|e| e.codec == Codec::H264));
    }

    #[test]
    fn explicit_backend_and_codec_are_honoured_with_notes() {
        let avail = by(&["h264_nvenc", "hevc_nvenc", "h264_amf", "libopenh264"]);
        let mut o = StreamOptions {
            encoder: EncoderChoice::Only(Backend::Amf),
            ..StreamOptions::default()
        };
        let s = choose(&o, &avail, &[]);
        assert_eq!(s.best().unwrap().name, "h264_amf");
        assert!(s.note.is_none());
        // Gewuenschter Hersteller fehlt: Auto mit Hinweis.
        o.encoder = EncoderChoice::Only(Backend::QuickSync);
        let s = choose(&o, &avail, &[]);
        assert_eq!(s.best().unwrap().name, "hevc_nvenc");
        assert!(s.note.unwrap().contains("QuickSync"));
        // Codec erzwungen, obwohl ein Zuschauer ihn nicht kann: Hinweis, aber befolgt.
        let o = StreamOptions {
            codec: CodecChoice::Only(Codec::Hevc),
            ..StreamOptions::default()
        };
        let s = choose(&o, &avail, &[DecoderCaps::H264_ONLY]);
        assert_eq!(s.best().unwrap().name, "hevc_nvenc");
        assert!(s.note.unwrap().contains("Zuschauer"));
        // Und die Kette fuehrt danach zu H.264 weiter.
        assert_eq!(choose(&o, &avail, &[]).chain.len(), 4);
    }

    #[test]
    fn software_av1_is_last_resort_only() {
        let avail = by(&["libsvtav1", "libopenh264"]);
        let s = choose(&StreamOptions::default(), &avail, &[]);
        assert_eq!(names(&s), ["libopenh264", "libsvtav1"]);
        let s = choose(&StreamOptions::default(), &[], &[]);
        assert!(s.best().is_none() && s.note.is_some());
    }

    #[test]
    fn failover_walks_the_chain_with_a_notice() {
        let avail = by(&["h264_nvenc", "h264_amf", "libopenh264"]);
        let s = choose(&StreamOptions::default(), &avail, &[]);
        let mut f = Failover::new(&s);
        assert_eq!(f.current().unwrap().name, "h264_nvenc");
        let (e, note) = f.fail().unwrap();
        assert_eq!(e.name, "h264_amf");
        assert!(note.contains("h264_nvenc") && note.contains("h264_amf"));
        assert_eq!(f.fail().unwrap().0.name, "libopenh264");
        assert!(f.fail().is_none());
        assert!(f.current().is_none());
    }

    #[test]
    fn target_size_keeps_aspect_never_upscales_and_is_even() {
        assert_eq!(Resolution::P1080.target(2560, 1440), (1920, 1080));
        assert_eq!(Resolution::P720.target(1920, 1080), (1280, 720));
        assert_eq!(Resolution::P1080.target(1280, 720), (1280, 720));
        assert_eq!(Resolution::Native.target(2561, 1441), (2560, 1440));
        // Fenster im Hochformat und mit ungeraden Massen.
        let (w, h) = Resolution::P720.target(999, 1501);
        assert!(w % 2 == 0 && h == 720 && w <= 999);
        assert_eq!(Resolution::P1440.target(1, 1), (2, 2));
    }

    #[test]
    fn simulcast_adds_a_small_layer_only_when_it_makes_sense() {
        let o = StreamOptions::default();
        assert_eq!(o.layers().len(), 1);
        let on = StreamOptions {
            simulcast: true,
            ..o
        };
        let l = on.layers();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0], on);
        assert_eq!(
            (l[1].resolution, l[1].fps, l[1].max_mbps),
            (Resolution::P720, 30, 3)
        );
        // Wer ohnehin 720p30 sendet, braucht keine zweite Stufe.
        let small = StreamOptions {
            resolution: Resolution::P720,
            fps: 30,
            ..on
        };
        assert_eq!(small.layers().len(), 1);
        // Alte Einstellungsdateien ohne das Feld bleiben lesbar.
        let old =
            r#"{"encoder":"Auto","codec":"Auto","resolution":"P1080","fps":60,"max_mbps":12}"#;
        assert!(
            !serde_json::from_str::<StreamOptions>(old)
                .unwrap()
                .simulcast
        );
    }

    #[test]
    fn options_roundtrip_and_sanitize() {
        let o = StreamOptions {
            encoder: EncoderChoice::Only(Backend::Nvenc),
            codec: CodecChoice::Only(Codec::Av1),
            resolution: Resolution::P1440,
            fps: 99,
            max_mbps: 0,
            simulcast: true,
            audio: AudioMode::OnlyProgram,
        };
        let json = serde_json::to_string(&o).unwrap();
        assert_eq!(serde_json::from_str::<StreamOptions>(&json).unwrap(), o);
        let s = o.sanitized();
        assert_eq!((s.fps, s.max_mbps), (60, 1));
        assert_eq!(StreamOptions { fps: 20, ..o }.sanitized().fps, 30);
    }
}

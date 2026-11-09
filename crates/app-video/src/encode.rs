//! ffmpeg-Encoder (M3-02): Probe-Encode, Kodierung von BGRA-Bildern, Keyframe auf Anforderung,
//! Bildraten-Begrenzung und Neustart bei geaenderter Bildgroesse (Quellenwechsel).
//!
//! Braucht das Feature `ffmpeg` (siehe docs/building.md fuer die Build-Voraussetzungen).

extern crate ffmpeg_next as ffmpeg;

use crate::select::{EncoderInfo, StreamOptions, CANDIDATES};
use ffmpeg::{
    codec, encoder, format::Pixel, frame, picture, software::scaling, Dictionary,
    Packet as FfPacket,
};
use std::sync::OnceLock;

/// Software-Encoder wollen YUV420P, Hardware-Encoder NV12.
fn pix_for(name: &str) -> Pixel {
    if name.starts_with("lib") {
        Pixel::YUV420P
    } else {
        Pixel::NV12
    }
}

fn options_for(name: &str) -> Dictionary<'static> {
    let mut o = Dictionary::new();
    if name.ends_with("_nvenc") {
        o.set("preset", "p1");
        o.set("tune", "ll");
        o.set("zerolatency", "1");
        o.set("rc", "cbr");
        o.set("delay", "0");
        o.set("forced-idr", "1");
    } else if name.ends_with("_amf") {
        o.set("usage", "ultralowlatency");
        o.set("rc", "cbr");
        o.set("quality", "speed");
        o.set("forced_idr", "1");
    } else if name.ends_with("_qsv") {
        o.set("preset", "veryfast");
        o.set("look_ahead", "0");
        o.set("async_depth", "1");
        o.set("forced_idr", "1");
    } else if name == "libopenh264" {
        o.set("allow_skip_frames", "0");
    } else if name == "libsvtav1" {
        o.set("preset", "10");
        o.set("svtav1-params", "tune=0");
    }
    o
}

fn open_raw(name: &str, w: u32, h: u32, fps: u32, kbps: u32) -> Result<encoder::Video, String> {
    ffmpeg::init().map_err(|e| e.to_string())?;
    let codec = encoder::find_by_name(name).ok_or("nicht im Build")?;
    let mut enc = codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .map_err(|e| e.to_string())?;
    enc.set_width(w);
    enc.set_height(h);
    enc.set_format(pix_for(name));
    enc.set_time_base((1, 1000));
    enc.set_frame_rate(Some((fps as i32, 1)));
    let bps = kbps as usize * 1000;
    enc.set_bit_rate(bps);
    enc.set_max_b_frames(0);
    enc.set_gop(fps * 2);
    let mut opts = options_for(name);
    opts.set("maxrate", &bps.to_string());
    opts.set("bufsize", &(bps / fps as usize * 2).to_string());
    enc.open_with(opts).map_err(|e| e.to_string())
}

/// Welche Encoder auf diesem Rechner wirklich funktionieren (Probe-Encode mit 1080p60).
/// Das Ergebnis wird fuer die Laufzeit des Programms gemerkt.
pub fn probe() -> &'static [EncoderInfo] {
    static CACHE: OnceLock<Vec<EncoderInfo>> = OnceLock::new();
    CACHE.get_or_init(|| {
        CANDIDATES
            .iter()
            .filter(|c| open_raw(c.name, 1920, 1080, 60, 20).is_ok())
            .copied()
            .collect()
    })
}

/// Ein kodiertes Paket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub data: Vec<u8>,
    pub key: bool,
    pub pts_ms: i64,
}

/// Ein BGRA-Bild mit Zeilenabstand `pitch`.
pub struct Bgra<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub pitch: usize,
}

struct Pipe {
    enc: encoder::Video,
    scaler: scaling::Context,
    src: frame::Video,
    dst: frame::Video,
    in_size: (u32, u32),
    out_size: (u32, u32),
}

pub struct Encoder {
    info: EncoderInfo,
    opts: StreamOptions,
    /// Aktuelles Bitratenziel (kbit/s); Aenderungen starten den Encoder neu.
    kbps: u32,
    restart: bool,
    pipe: Option<Pipe>,
    force_key: bool,
    first_ts: Option<i64>,
    last_pts: i64,
    /// Naechster Soll-Zeitpunkt (ms) fuer die Bildraten-Begrenzung.
    due: f64,
}

// SAFETY: Die ffmpeg-Objekte enthalten rohe Zeiger, werden aber nur von einem Thread zur
// Zeit benutzt (`&mut self`); es gibt keinen geteilten Zugriff.
unsafe impl Send for Encoder {}

fn drain(enc: &mut encoder::Video, out: &mut Vec<Packet>) {
    let mut pkt = FfPacket::empty();
    while enc.receive_packet(&mut pkt).is_ok() {
        if let Some(d) = pkt.data() {
            out.push(Packet {
                data: d.to_vec(),
                key: pkt.is_key(),
                pts_ms: pkt.pts().unwrap_or(0),
            });
        }
    }
}

impl Encoder {
    /// Der Encoder wird erst mit dem ersten Bild geoeffnet (die Quellgroesse ist vorher
    /// unbekannt). Ob er sich oeffnen laesst, zeigt `probe`.
    pub fn new(info: EncoderInfo, opts: StreamOptions) -> Self {
        Self {
            info,
            opts: opts.sanitized(),
            kbps: opts.sanitized().max_mbps * 1000,
            restart: false,
            pipe: None,
            force_key: false,
            first_ts: None,
            last_pts: -1,
            due: 0.0,
        }
    }

    pub fn info(&self) -> EncoderInfo {
        self.info
    }

    /// Groesse der kodierten Bilder, sobald das erste Bild da war.
    pub fn out_size(&self) -> Option<(u32, u32)> {
        self.pipe.as_ref().map(|p| p.out_size)
    }

    /// Das naechste kodierte Bild wird ein Keyframe (neuer Zuschauer, Verlust).
    pub fn request_keyframe(&mut self) {
        self.force_key = true;
    }

    /// Stellt die Bitrate (kbit/s) um. Aenderungen unter 10 % werden ignoriert; sonst startet der
    /// Encoder beim naechsten Bild neu und beginnt mit einem Keyframe. Die Bitrate bleibt unter
    /// der in den Optionen gewaehlten Grenze.
    pub fn set_bitrate_kbps(&mut self, kbps: u32) {
        let k = kbps.clamp(100, self.opts.max_mbps * 1000);
        if k.abs_diff(self.kbps) * 10 >= self.kbps {
            self.kbps = k;
            self.restart = true;
        }
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.kbps
    }

    fn open_pipe(&self, w: u32, h: u32) -> Result<Pipe, String> {
        let (tw, th) = self.opts.resolution.target(w, h);
        let enc = open_raw(self.info.name, tw, th, self.opts.fps, self.kbps)?;
        let pix = pix_for(self.info.name);
        let flags = if (tw, th) == (w, h) {
            scaling::Flags::FAST_BILINEAR
        } else {
            scaling::Flags::BILINEAR
        };
        let scaler = scaling::Context::get(Pixel::BGRA, w, h, pix, tw, th, flags)
            .map_err(|e| e.to_string())?;
        Ok(Pipe {
            enc,
            scaler,
            src: frame::Video::new(Pixel::BGRA, w, h),
            dst: frame::Video::new(pix, tw, th),
            in_size: (w, h),
            out_size: (tw, th),
        })
    }

    /// Kodiert ein Bild. `ts_ms` ist ein beliebiger, steigender Zeitstempel in Millisekunden.
    /// Gibt nichts zurueck, wenn das Bild wegen der Bildrate ausgelassen wird oder der Encoder
    /// noch puffert. Aendert sich die Bildgroesse, wird der Encoder neu gestartet; die Pakete
    /// des alten kommen noch vor denen des neuen.
    pub fn encode(&mut self, img: &Bgra<'_>, ts_ms: i64) -> Result<Vec<Packet>, String> {
        let first = *self.first_ts.get_or_insert(ts_ms);
        let t = (ts_ms - first) as f64;
        let interval = 1000.0 / f64::from(self.opts.fps);
        if t < self.due - interval / 2.0 {
            return Ok(Vec::new());
        }
        self.due = if self.due < t - interval {
            t + interval
        } else {
            self.due + interval
        };

        let mut out = Vec::new();
        let size = (img.width, img.height);
        if self.pipe.as_ref().is_some_and(|p| p.in_size != size) || self.restart {
            self.flush_into(&mut out);
            self.pipe = None;
            self.restart = false;
        }
        if self.pipe.is_none() {
            self.pipe = Some(self.open_pipe(size.0, size.1)?);
            // Der neue Stream beginnt mit einem Keyframe, auch wenn keiner verlangt wurde.
            self.force_key = true;
        }
        let pipe = self.pipe.as_mut().ok_or("kein Encoder")?;

        let stride = pipe.src.stride(0);
        let row = img.width as usize * 4;
        if img.pitch < row || img.data.len() < img.pitch * (img.height as usize - 1) + row {
            return Err("Bildpuffer zu klein".into());
        }
        for y in 0..img.height as usize {
            pipe.src.data_mut(0)[y * stride..y * stride + row]
                .copy_from_slice(&img.data[y * img.pitch..y * img.pitch + row]);
        }
        pipe.scaler
            .run(&pipe.src, &mut pipe.dst)
            .map_err(|e| e.to_string())?;

        let mut pts = t as i64;
        if pts <= self.last_pts {
            pts = self.last_pts + 1;
        }
        self.last_pts = pts;
        pipe.dst.set_pts(Some(pts));
        if std::mem::take(&mut self.force_key) {
            pipe.dst.set_kind(picture::Type::I);
        } else {
            pipe.dst.set_kind(picture::Type::None);
        }
        pipe.enc.send_frame(&pipe.dst).map_err(|e| e.to_string())?;
        drain(&mut pipe.enc, &mut out);
        Ok(out)
    }

    fn flush_into(&mut self, out: &mut Vec<Packet>) {
        if let Some(p) = self.pipe.as_mut() {
            if p.enc.send_eof().is_ok() {
                drain(&mut p.enc, out);
            }
        }
    }

    /// Holt alle noch gepufferten Pakete und beendet den Encoder.
    pub fn finish(mut self) -> Vec<Packet> {
        let mut out = Vec::new();
        self.flush_into(&mut out);
        out
    }
}

/// Mehrere Qualitaetsstufen desselben Bildes (M3-05): fuer jede Stufe ein eigener Encoder.
/// Das Bild wird je Stufe einzeln skaliert; die Stufen laufen unabhaengig voneinander
/// (eigene Bitrate, eigene Keyframes).
pub struct Simulcast {
    layers: Vec<Encoder>,
}

unsafe impl Send for Simulcast {}

impl Simulcast {
    /// Stufen laut `StreamOptions::layers`. Alle benutzen denselben Encoder (`info`).
    pub fn new(info: EncoderInfo, opts: &StreamOptions) -> Self {
        Self {
            layers: opts
                .layers()
                .into_iter()
                .map(|o| Encoder::new(info, o))
                .collect(),
        }
    }

    pub fn layer_count(&self) -> u8 {
        self.layers.len() as u8
    }

    /// Groesse der kodierten Bilder einer Stufe.
    pub fn out_size(&self, layer: u8) -> Option<(u32, u32)> {
        self.layers.get(usize::from(layer))?.out_size()
    }

    pub fn request_keyframe(&mut self, layer: u8) {
        if let Some(e) = self.layers.get_mut(usize::from(layer)) {
            e.request_keyframe();
        }
    }

    pub fn set_bitrate_kbps(&mut self, layer: u8, kbps: u32) {
        if let Some(e) = self.layers.get_mut(usize::from(layer)) {
            e.set_bitrate_kbps(kbps);
        }
    }

    pub fn bitrate_kbps(&self, layer: u8) -> Option<u32> {
        self.layers
            .get(usize::from(layer))
            .map(Encoder::bitrate_kbps)
    }

    /// Kodiert ein Bild in allen Stufen. Pakete kommen als `(Stufe, Paket)`.
    pub fn encode(&mut self, img: &Bgra<'_>, ts_ms: i64) -> Result<Vec<(u8, Packet)>, String> {
        let mut out = Vec::new();
        for (i, e) in self.layers.iter_mut().enumerate() {
            out.extend(e.encode(img, ts_ms)?.into_iter().map(|p| (i as u8, p)));
        }
        Ok(out)
    }

    pub fn finish(self) -> Vec<(u8, Packet)> {
        let mut out = Vec::new();
        for (i, e) in self.layers.into_iter().enumerate() {
            out.extend(e.finish().into_iter().map(|p| (i as u8, p)));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::select::Resolution;

    /// Bewegtes Farbmuster, damit der Encoder echte Arbeit hat.
    fn pattern(w: u32, h: u32, n: u32) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let o = ((y * w + x) * 4) as usize;
                v[o] = ((x + n * 3) % 256) as u8;
                v[o + 1] = ((y + n * 2) % 256) as u8;
                v[o + 2] = ((x ^ y) % 256) as u8;
                v[o + 3] = 255;
            }
        }
        v
    }

    fn run(
        info: EncoderInfo,
        opts: StreamOptions,
        w: u32,
        h: u32,
        frames: u32,
        step_ms: i64,
    ) -> Vec<Packet> {
        let mut e = Encoder::new(info, opts);
        let mut out = Vec::new();
        for n in 0..frames {
            let d = pattern(w, h, n);
            out.extend(
                e.encode(
                    &Bgra {
                        data: &d,
                        width: w,
                        height: h,
                        pitch: (w * 4) as usize,
                    },
                    i64::from(n) * step_ms,
                )
                .unwrap(),
            );
        }
        out.extend(e.finish());
        out
    }

    #[test]
    fn probe_finds_at_least_one_encoder_and_all_of_them_encode() {
        let found = probe();
        println!(
            "Gefunden: {}",
            found.iter().map(|e| e.name).collect::<Vec<_>>().join(", ")
        );
        assert!(!found.is_empty(), "kein Encoder auf diesem Rechner");
        let opts = StreamOptions {
            resolution: Resolution::P720,
            ..StreamOptions::default()
        };
        for info in found {
            // libsvtav1 laesst sich oeffnen, kodiert aber in dieser Konfiguration nichts
            // Brauchbares (siehe docs/building.md): hier nur das Oeffnen pruefen.
            if info.name == "libsvtav1" {
                continue;
            }
            let pk = run(*info, opts, 1280, 720, 90, 17);
            assert!(!pk.is_empty(), "{}: keine Pakete", info.name);
            assert!(pk[0].key, "{}: erstes Paket ist kein Keyframe", info.name);
            let bytes: usize = pk.iter().map(|p| p.data.len()).sum();
            println!("{}: {} Pakete, {} KiB", info.name, pk.len(), bytes / 1024);
        }
    }

    #[test]
    fn bitrate_change_restarts_with_keyframe() {
        let found = probe();
        let Some(info) = found
            .iter()
            .find(|e| e.name == "h264_nvenc" || e.name == "libopenh264")
            .copied()
        else {
            return;
        };
        let mut e = Encoder::new(info, StreamOptions::default());
        assert_eq!(e.bitrate_kbps(), 12_000);
        e.set_bitrate_kbps(1_000_000); // ueber der Grenze: wird begrenzt
        assert_eq!(e.bitrate_kbps(), 12_000);
        let mut first_after = None;
        for n in 0..60u32 {
            if n == 30 {
                e.set_bitrate_kbps(3_000);
                e.set_bitrate_kbps(2_950); // unter 10 %: kein weiterer Neustart
            }
            let d = pattern(640, 360, n);
            let out = e
                .encode(
                    &Bgra {
                        data: &d,
                        width: 640,
                        height: 360,
                        pitch: 640 * 4,
                    },
                    i64::from(n) * 17,
                )
                .unwrap();
            if n >= 30 && first_after.is_none() {
                first_after = out.first().map(|p| p.key);
            }
        }
        assert_eq!(e.bitrate_kbps(), 3_000);
        assert_eq!(first_after, Some(true), "{}", info.name);
    }

    #[test]
    fn keyframe_request_and_resize_and_fps_cap() {
        let found = probe();
        let Some(info) = found
            .iter()
            .find(|e| e.name == "h264_nvenc" || e.name == "libopenh264")
            .copied()
        else {
            return;
        };
        let opts = StreamOptions {
            resolution: Resolution::Native,
            ..StreamOptions::default()
        };
        // Keyframe auf Anforderung.
        let mut e = Encoder::new(info, opts);
        let mut keys_after_request = 0;
        for n in 0..120u32 {
            if n == 60 {
                e.request_keyframe();
            }
            let d = pattern(640, 360, n);
            let out = e
                .encode(
                    &Bgra {
                        data: &d,
                        width: 640,
                        height: 360,
                        pitch: 640 * 4,
                    },
                    i64::from(n) * 17,
                )
                .unwrap();
            if (60..66).contains(&n) {
                keys_after_request += out.iter().filter(|p| p.key).count();
            }
        }
        assert!(
            keys_after_request >= 1,
            "{}: kein Keyframe nach Anforderung",
            info.name
        );

        // Groessenwechsel: das neue Bild hat die neue Groesse und beginnt mit einem Keyframe.
        let mut e = Encoder::new(info, opts);
        let big = pattern(1280, 720, 0);
        let small = pattern(640, 360, 0);
        let mut first_after = None;
        for n in 0..30u32 {
            let (d, w, h) = if n < 15 {
                (&big, 1280, 720)
            } else {
                (&small, 640, 360)
            };
            let out = e
                .encode(
                    &Bgra {
                        data: d,
                        width: w,
                        height: h,
                        pitch: (w * 4) as usize,
                    },
                    i64::from(n) * 17,
                )
                .unwrap();
            if n >= 15 && first_after.is_none() {
                first_after = out.iter().find(|p| p.pts_ms >= 0 && p.key).cloned();
            }
        }
        assert_eq!(e.out_size(), Some((640, 360)));
        assert!(
            first_after.is_some(),
            "kein Keyframe nach dem Groessenwechsel"
        );

        // Bildrate: 120 Bilder pro Sekunde hinein, hoechstens ~60 heraus.
        let pk = run(info, opts, 640, 360, 240, 8);
        let secs = 240.0 * 8.0 / 1000.0;
        let fps = pk.len() as f64 / secs;
        println!("{}: {fps:.0} fps bei 125 Bildern/s Eingang", info.name);
        assert!((50.0..=70.0).contains(&fps), "{fps} fps");
    }
    #[test]
    fn simulcast_delivers_both_layers_and_costs_are_measured() {
        let found = probe();
        let Some(info) = found
            .iter()
            .find(|e| e.name == "h264_nvenc" || e.name == "libopenh264")
            .copied()
        else {
            return;
        };
        let single = StreamOptions {
            resolution: Resolution::Native,
            max_mbps: 8,
            ..StreamOptions::default()
        };
        let both = StreamOptions {
            simulcast: true,
            ..single
        };
        let (w, h, n) = (1920u32, 1080u32, 180u32);
        let frames: Vec<Vec<u8>> = (0..n).map(|i| pattern(w, h, i)).collect();
        let img = |i: usize| Bgra {
            data: &frames[i],
            width: w,
            height: h,
            pitch: (w * 4) as usize,
        };

        let t = std::time::Instant::now();
        let mut e = Encoder::new(info, single);
        let mut n1 = 0;
        for i in 0..n as usize {
            n1 += e.encode(&img(i), i as i64 * 17).unwrap().len();
        }
        let t_single = t.elapsed();
        assert!(n1 > 0);

        let t = std::time::Instant::now();
        let mut s = Simulcast::new(info, &both);
        assert_eq!(s.layer_count(), 2);
        let mut got = [0usize; 2];
        let mut first_key = [false; 2];
        for i in 0..n as usize {
            for (l, p) in s.encode(&img(i), i as i64 * 17).unwrap() {
                if got[usize::from(l)] == 0 {
                    first_key[usize::from(l)] = p.key;
                }
                got[usize::from(l)] += 1;
            }
        }
        let t_both = t.elapsed();
        println!(
            "{}: {} Bilder 1080p: eine Stufe {:?} ({:.1} ms/Bild), mit Simulcast {:?} ({:.1} ms/Bild), Mehraufwand {:.0} %",
            info.name,
            n,
            t_single,
            t_single.as_secs_f64() * 1000.0 / f64::from(n),
            t_both,
            t_both.as_secs_f64() * 1000.0 / f64::from(n),
            (t_both.as_secs_f64() / t_single.as_secs_f64() - 1.0) * 100.0
        );
        assert!(got[0] > 0 && got[1] > 0, "{got:?}");
        assert_eq!(first_key, [true, true]);
        assert_eq!(s.out_size(0), Some((1920, 1080)));
        assert_eq!(s.out_size(1), Some((1280, 720)));
        // Stufe 1 hat ihre eigene Bitrate und Keyframes.
        s.set_bitrate_kbps(1, 1_500);
        assert_eq!(s.bitrate_kbps(1), Some(1_500));
        assert_eq!(s.bitrate_kbps(0), Some(8_000));
        // Die zweite Stufe darf die Bildrate nicht ueber 30 treiben (60 fps hinein, 30 heraus).
        assert!(
            got[1] * 10 <= got[0] * 6 + 20,
            "Stufe 1 hat zu viele Bilder: {got:?}"
        );
    }
}

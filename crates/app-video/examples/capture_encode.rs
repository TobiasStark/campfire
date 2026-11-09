//! M0-05: WGC-Capture + ffmpeg-Encoder (Spike).
//!
//!   capture_encode list                         alle Encoder auf dieser Maschine testen
//!   capture_encode run [optionen]               aufnehmen und kodieren
//!     --encoder h264_nvenc   (Standard)
//!     --target monitor|foreground     (Standard monitor)
//!     --secs 10   --fps 60   --mbps 20   --out capture.h264
//!
//! Braucht FFMPEG_DIR / LIBCLANG_PATH beim Bauen (siehe docs/building.md) und die
//! ffmpeg-DLLs im PATH oder neben der .exe.

extern crate ffmpeg_next as ffmpeg;

use std::{
    fs::File,
    io::{BufWriter, Write},
    time::{Duration, Instant},
};

use ffmpeg::{
    codec, encoder, format::Pixel, frame, picture, software::scaling, Dictionary, Packet,
};
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
    window::Window,
};

const CANDIDATES: &[&str] = &[
    "h264_nvenc",
    "h264_amf",
    "h264_qsv",
    "hevc_nvenc",
    "hevc_amf",
    "hevc_qsv",
    "av1_nvenc",
    "av1_amf",
    "av1_qsv",
    "h264_mf",
    "libopenh264",
    "libsvtav1",
];

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

fn open_encoder(
    name: &str,
    w: u32,
    h: u32,
    fps: i32,
    mbps: usize,
) -> Result<encoder::Video, String> {
    let codec = encoder::find_by_name(name).ok_or("nicht im Build")?;
    let mut enc = codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .map_err(|e| e.to_string())?;
    enc.set_width(w);
    enc.set_height(h);
    enc.set_format(pix_for(name));
    enc.set_time_base((1, 1000));
    enc.set_frame_rate(Some((fps, 1)));
    enc.set_bit_rate(mbps * 1_000_000);
    enc.set_max_b_frames(0);
    enc.set_gop((fps * 2) as u32);
    let mut opts = options_for(name);
    let bps = mbps * 1_000_000;
    opts.set("maxrate", &bps.to_string());
    opts.set("bufsize", &(bps / fps as usize * 2).to_string());
    enc.open_with(opts).map_err(|e| e.to_string())
}

fn list() {
    println!("Encoder-Test (1920x1080, 60 fps, 20 Mbit/s):\n");
    let mut working = vec![];
    for name in CANDIDATES {
        match open_encoder(name, 1920, 1080, 60, 20) {
            Ok(_) => {
                println!("  OK     {name}");
                working.push(*name);
            }
            Err(e) => println!("  FEHLT  {name}  ({e})"),
        }
    }
    println!("\nFunktionierend: {}", working.join(", "));
}

struct Cfg {
    encoder: String,
    secs: u64,
    fps: i32,
    mbps: usize,
    out: String,
}

/// ffmpeg-Objekte enthalten rohe Zeiger. Sie werden hier nur auf dem Capture-Thread benutzt
/// (Erzeugung in `new`, Nutzung in `on_frame_arrived`, beides im selben Thread).
struct Gear {
    enc: encoder::Video,
    scaler: Option<scaling::Context>,
    src: Option<frame::Video>,
    dst: Option<frame::Video>,
}
// SAFETY: siehe Kommentar an `Gear`; kein gleichzeitiger Zugriff aus mehreren Threads.
unsafe impl Send for Gear {}

struct Rec {
    g: Gear,
    out: BufWriter<File>,
    cfg: Cfg,
    start: Instant,
    first_ts: Option<Duration>,
    frames: u64,
    bytes: u64,
    t_convert: Vec<f64>,
    t_encode: Vec<f64>,
    t_total: Vec<f64>,
    force_at: u64,
    force_result: Option<bool>,
    force_pending: bool,
}

impl GraphicsCaptureApiHandler for Rec {
    type Flags = Cfg;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let cfg = ctx.flags;
        // Größe wird erst im ersten Frame bekannt -> Encoder lazy; hier Platzhalter 1920x1080.
        let enc = open_encoder(&cfg.encoder, 1920, 1080, cfg.fps, cfg.mbps)
            .map_err(|e| format!("Encoder {}: {e}", cfg.encoder))?;
        let out = BufWriter::new(File::create(&cfg.out)?);
        println!("Aufnahme läuft ({} s) ...", cfg.secs);
        Ok(Self {
            g: Gear {
                enc,
                scaler: None,
                src: None,
                dst: None,
            },
            out,
            cfg,
            start: Instant::now(),
            first_ts: None,
            frames: 0,
            bytes: 0,
            t_convert: vec![],
            t_encode: vec![],
            t_total: vec![],
            force_at: 120,
            force_result: None,
            force_pending: false,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let t0 = Instant::now();
        let (w, h) = (frame.width(), frame.height());
        if (w, h) != (1920, 1080) {
            control.stop();
            return Err(format!(
                "Quelle ist {w}x{h}, der Spike erwartet 1920x1080 (Monitor/Fenster auf 1080p stellen)"
            )
            .into());
        }
        if self.g.scaler.is_none() {
            self.g.scaler = Some(scaling::Context::get(
                Pixel::BGRA,
                w,
                h,
                pix_for(&self.cfg.encoder),
                w,
                h,
                scaling::Flags::FAST_BILINEAR,
            )?);
            self.g.src = Some(frame::Video::new(Pixel::BGRA, w, h));
            self.g.dst = Some(frame::Video::new(pix_for(&self.cfg.encoder), w, h));
        }
        let ts = frame
            .timestamp()
            .map(|t| Duration::from_nanos(t.Duration as u64 * 100))
            .unwrap_or_default();
        let first = *self.first_ts.get_or_insert(ts);
        let pts_ms = ts.saturating_sub(first).as_millis() as i64;

        // BGRA -> NV12
        let mut buf = frame.buffer()?;
        let pitch = buf.row_pitch() as usize;
        let raw = buf.as_raw_buffer();
        let src = self.g.src.as_mut().ok_or("src")?;
        let stride = src.stride(0);
        let row = w as usize * 4;
        for y in 0..h as usize {
            src.data_mut(0)[y * stride..y * stride + row]
                .copy_from_slice(&raw[y * pitch..y * pitch + row]);
        }
        let dst = self.g.dst.as_mut().ok_or("dst")?;
        self.g.scaler.as_mut().ok_or("scaler")?.run(src, dst)?;
        let t1 = Instant::now();

        dst.set_pts(Some(pts_ms));
        if self.frames == self.force_at {
            dst.set_kind(picture::Type::I);
            self.force_pending = true;
        } else {
            dst.set_kind(picture::Type::None);
        }
        self.g.enc.send_frame(dst)?;
        let mut pkt = Packet::empty();
        while self.g.enc.receive_packet(&mut pkt).is_ok() {
            if self.force_pending {
                self.force_result = Some(pkt.is_key());
                self.force_pending = false;
            }
            if let Some(d) = pkt.data() {
                self.bytes += d.len() as u64;
                self.out.write_all(d)?;
            }
        }
        let t2 = Instant::now();

        self.frames += 1;
        self.t_convert.push((t1 - t0).as_secs_f64() * 1000.0);
        self.t_encode.push((t2 - t1).as_secs_f64() * 1000.0);
        self.t_total.push((t2 - t0).as_secs_f64() * 1000.0);

        if self.start.elapsed() >= Duration::from_secs(self.cfg.secs) {
            self.finish()?;
            control.stop();
        }
        Ok(())
    }
}

fn stats(v: &mut [f64]) -> String {
    if v.is_empty() {
        return "-".into();
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let avg = v.iter().sum::<f64>() / v.len() as f64;
    format!(
        "avg {:.2} / p95 {:.2} / max {:.2} ms",
        avg,
        v[(v.len() as f64 * 0.95) as usize],
        v[v.len() - 1]
    )
}

impl Rec {
    fn finish(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.g.enc.send_eof()?;
        let mut pkt = Packet::empty();
        while self.g.enc.receive_packet(&mut pkt).is_ok() {
            if let Some(d) = pkt.data() {
                self.bytes += d.len() as u64;
                self.out.write_all(d)?;
            }
        }
        self.out.flush()?;
        let secs = self.start.elapsed().as_secs_f64();
        println!("\n----- ERGEBNIS {} -----", self.cfg.encoder);
        println!(
            "Frames:            {} in {:.1} s = {:.1} fps (nur geänderte Frames werden geliefert)",
            self.frames,
            secs,
            self.frames as f64 / secs
        );
        println!(
            "Bitrate real:      {:.1} Mbit/s",
            self.bytes as f64 * 8.0 / 1e6 / secs
        );
        println!("Konvertierung:     {}", stats(&mut self.t_convert));
        println!("Encode (send+rcv): {}", stats(&mut self.t_encode));
        println!("Gesamt pro Frame:  {}", stats(&mut self.t_total));
        println!(
            "Keyframe auf Anforderung (Frame {}): {}",
            self.force_at,
            match self.force_result {
                Some(true) => "OK (Paket war Keyframe)",
                Some(false) => "NEIN (Paket war kein Keyframe)",
                None => "nicht getestet (zu wenige Frames)",
            }
        );
        println!("Datei:             {}", self.cfg.out);
        Ok(())
    }
}

fn run(args: &[String]) {
    let get = |k: &str, d: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| d.to_string())
    };
    let cfg = Cfg {
        encoder: get("--encoder", "h264_nvenc"),
        secs: get("--secs", "10").parse().unwrap_or(10),
        fps: get("--fps", "60").parse().unwrap_or(60),
        mbps: get("--mbps", "20").parse().unwrap_or(20),
        out: get("--out", "capture.h264"),
    };
    let target = get("--target", "monitor");
    macro_rules! go {
        ($item:expr) => {{
            let s = Settings::new(
                $item,
                CursorCaptureSettings::Default,
                DrawBorderSettings::WithoutBorder,
                SecondaryWindowSettings::Default,
                MinimumUpdateIntervalSettings::Default,
                DirtyRegionSettings::Default,
                ColorFormat::Bgra8,
                cfg,
            );
            if let Err(e) = Rec::start(s) {
                println!("FEHLER: {e}");
            }
        }};
    }
    match target.as_str() {
        "foreground" => go!(Window::foreground().expect("Vordergrundfenster")),
        _ => go!(Monitor::primary().expect("Monitor")),
    }
}

fn main() {
    ffmpeg::init().expect("ffmpeg init");
    ffmpeg::log::set_level(ffmpeg::log::Level::Error);
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("list") => list(),
        Some("run") => run(&args[1..]),
        _ => println!("Aufruf: capture_encode list | run [--encoder X] [--target monitor|foreground] [--secs N] [--fps N] [--mbps N] [--out datei]"),
    }
}

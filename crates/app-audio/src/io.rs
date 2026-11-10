//! Audio-Geraete (cpal/WASAPI): Aufnahme und Wiedergabe mit Resampling und Wiederanlauf
//! nach Ab-/Anstecken von Geraeten.
//!
//! Ein Arbeits-Thread besitzt die cpal-Streams. Er
//! - holt 20-ms-Frames aus dem Aufnahme-Ringpuffer, kodiert und sendet sie,
//! - haelt den Wiedergabe-Ringpuffer mit gemischten Frames gefuellt,
//! - baut die Streams neu auf, wenn ein Fehler gemeldet wird, sich das Standardgeraet
//!   aendert oder noch keine Geraete geoeffnet sind.

use crate::process::{Action, InputSettings, Processor};
use crate::voice::VoiceCore;
use crate::{FRAME, RATE};
use app_proto::lock::Locked;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// So viele Frames (20 ms) sollen anfangs vor dem Lautsprecher bereitliegen.
const OUT_TARGET_FRAMES: usize = 3;
/// Obergrenze, wenn der Puffer nach Aussetzern waechst (160 ms).
const OUT_MAX_FRAMES: usize = 8;
/// Ohne Aussetzer so lange, dann wird der Ausgabepuffer wieder um einen Frame kleiner.
const OUT_SHRINK_AFTER: Duration = Duration::from_secs(30);
const TICK: Duration = Duration::from_millis(5);
const RETRY: Duration = Duration::from_millis(500);
const DEVICE_CHECK: Duration = Duration::from_secs(1);
/// Ueber dieses Fenster wird der groesste Aufnahme-Schub gemessen.
const BURST_WINDOW: Duration = Duration::from_secs(5);
/// Kommt der Arbeits-Thread mehr als so spaet dran, zaehlt das als Verspaetung.
const LATE_TICK: Duration = Duration::from_millis(20);

/// Gewuenschte Geraete (Namen); `None` = Standardgeraet des Systems.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IoConfig {
    pub input: Option<String>,
    pub output: Option<String>,
}

/// Sichtbarer Zustand fuer die UI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IoStatus {
    pub input: Option<String>,
    pub output: Option<String>,
    pub restarts: u32,
}

/// Messwerte des Audio-Threads fuer die Diagnose-Anzeige.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IoDiag {
    /// Wie oft der Lautsprecher leer lief (seit Start, ueber Geraetewechsel hinweg).
    pub underruns: u32,
    /// Wie oft der Arbeits-Thread mehr als einen Frame zu spaet aufwachte.
    pub late_ticks: u32,
    /// Groesste Zahl Mikrofon-Frames, die in einem Durchlauf auf einmal anlagen (letzte 5 s).
    /// 1 bis 2 ist normal; mehr heisst, dass Sprache stossweise gesendet wird.
    pub capture_burst: u32,
    /// Aktuelle Zieltiefe des Ausgabepuffers in ms.
    pub out_ms: u32,
}

fn dev_name(d: &cpal::Device) -> String {
    d.description()
        .map(|x| x.name().to_string())
        .unwrap_or_else(|_| "?".into())
}

pub fn list_inputs() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|it| it.map(|d| dev_name(&d)).collect())
        .unwrap_or_default()
}

pub fn list_outputs() -> Vec<String> {
    cpal::default_host()
        .output_devices()
        .map(|it| it.map(|d| dev_name(&d)).collect())
        .unwrap_or_default()
}

/// Lineares Resampling im Datenstrom (fuer Sprache ausreichend).
pub struct Resampler {
    /// Eingangs-Samples pro Ausgangs-Sample.
    step: f64,
    pos: f64,
    prev: f32,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            step: from as f64 / to as f64,
            pos: 0.0,
            prev: 0.0,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if (self.step - 1.0).abs() < 1e-9 {
            out.extend_from_slice(input);
            return;
        }
        // Position 0 liegt auf `prev`; input[i] liegt auf i + 1.
        let n = input.len() as f64;
        while self.pos < n {
            let i = self.pos.floor();
            let frac = (self.pos - i) as f32;
            let a = if i < 1.0 {
                self.prev
            } else {
                input[i as usize - 1]
            };
            let b = input[i as usize];
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        self.pos -= n;
        if let Some(l) = input.last() {
            self.prev = *l;
        }
    }
}

struct Shared {
    core: Arc<Mutex<VoiceCore>>,
    send: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
    stop: AtomicBool,
    cfg: Mutex<IoConfig>,
    status: Mutex<IoStatus>,
    /// Anzahl neuer Konfigurationen (aendert sich -> Streams neu aufbauen).
    cfg_gen: AtomicU32,
    /// Mikrofon oeffnen? Ohne Aufnahme (z. B. beim Klingeln) laeuft nur die Wiedergabe.
    capture: AtomicBool,
    input: Mutex<InputSettings>,
    input_gen: AtomicU32,
    /// Eingangspegel (f32-Bits) und ob gerade gesendet wird.
    level: AtomicU32,
    open: AtomicBool,
    underruns: AtomicU32,
    late_ticks: AtomicU32,
    capture_burst: AtomicU32,
    out_frames: AtomicU32,
}

/// Laufende Audio-Ein-/Ausgabe. Beim Drop wird alles beendet.
pub struct AudioIo {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

type InProd = <HeapRb<f32> as Split>::Prod;
type InCons = <HeapRb<f32> as Split>::Cons;

struct Streams {
    _in: Option<cpal::Stream>,
    _out: Option<cpal::Stream>,
    in_cons: Option<InCons>,
    out_prod: Option<InProd>,
    out_rate: u32,
    out_resampler: Resampler,
    out_resampler_r: Resampler,
    in_name: Option<String>,
    out_name: Option<String>,
    broken: Arc<AtomicBool>,
    /// Wie oft der Lautsprecher-Callback den Ring leer vorfand (Aussetzer).
    underruns: Arc<AtomicU32>,
}

fn find<I: Iterator<Item = cpal::Device>>(it: I, name: &str) -> Option<cpal::Device> {
    it.into_iter().find(|d| dev_name(d) == name)
}

fn open(cfg: &IoConfig, capture: bool) -> Streams {
    let host = cpal::default_host();
    let broken = Arc::new(AtomicBool::new(false));
    let mut s = Streams {
        _in: None,
        _out: None,
        in_cons: None,
        out_prod: None,
        out_rate: RATE,
        out_resampler: Resampler::new(RATE, RATE),
        out_resampler_r: Resampler::new(RATE, RATE),
        in_name: None,
        out_name: None,
        broken: broken.clone(),
        underruns: Arc::new(AtomicU32::new(0)),
    };

    // Aufnahme
    let dev = match &cfg.input {
        Some(n) => host.input_devices().ok().and_then(|it| find(it, n)),
        None => host.default_input_device(),
    }
    .or_else(|| host.default_input_device())
    .filter(|_| capture);
    if let Some(dev) = dev {
        if let Ok(conf) = dev.default_input_config() {
            let channels = conf.channels() as usize;
            let rate = conf.sample_rate();
            let (mut prod, cons) = HeapRb::<f32>::new(RATE as usize).split();
            let mut rs = Resampler::new(rate, RATE);
            let mut mono = Vec::<f32>::new();
            let mut conv = Vec::<f32>::new();
            let b = broken.clone();
            let stream = dev.build_input_stream(
                conf.config(),
                move |data: &[f32], _| {
                    mono.clear();
                    mono.extend(
                        data.chunks(channels)
                            .map(|f| f.iter().sum::<f32>() / channels as f32),
                    );
                    conv.clear();
                    rs.process(&mono, &mut conv);
                    let _ = prod.push_slice(&conv);
                },
                move |_| b.store(true, Ordering::Relaxed),
                None,
            );
            if let Ok(st) = stream {
                if st.play().is_ok() {
                    s._in = Some(st);
                    s.in_cons = Some(cons);
                    s.in_name = Some(dev_name(&dev));
                }
            }
        }
    }

    // Wiedergabe
    let dev = match &cfg.output {
        Some(n) => host.output_devices().ok().and_then(|it| find(it, n)),
        None => host.default_output_device(),
    }
    .or_else(|| host.default_output_device());
    if let Some(dev) = dev {
        if let Ok(conf) = dev.default_output_config() {
            let channels = conf.channels() as usize;
            let rate = conf.sample_rate();
            // Der Ring enthaelt verschraenkte Stereo-Samples (links, rechts).
            let (prod, mut cons) = HeapRb::<f32>::new(rate as usize * 2).split();
            let b = broken.clone();
            let under = s.underruns.clone();
            let mut primed = false;
            let stream = dev.build_output_stream(
                conf.config(),
                move |data: &mut [f32], _| {
                    // Erst nach dem ersten Fuellen zaehlen Leerlaeufe als Aussetzer.
                    let need = data.len() / channels.max(1) * 2;
                    if cons.occupied_len() >= need {
                        primed = true;
                    } else if primed {
                        under.fetch_add(1, Ordering::Relaxed);
                    }
                    for f in data.chunks_mut(channels) {
                        let l = cons.try_pop().unwrap_or(0.0);
                        let r = cons.try_pop().unwrap_or(0.0);
                        match f.len() {
                            1 => f[0] = (l + r) * 0.5,
                            _ => {
                                f.fill((l + r) * 0.5);
                                f[0] = l;
                                f[1] = r;
                            }
                        }
                    }
                },
                move |_| b.store(true, Ordering::Relaxed),
                None,
            );
            if let Ok(st) = stream {
                if st.play().is_ok() {
                    s._out = Some(st);
                    s.out_prod = Some(prod);
                    s.out_rate = rate;
                    s.out_resampler = Resampler::new(RATE, rate);
                    s.out_resampler_r = Resampler::new(RATE, rate);
                    s.out_name = Some(dev_name(&dev));
                }
            }
        }
    }
    s
}

fn default_names() -> (Option<String>, Option<String>) {
    let h = cpal::default_host();
    (
        h.default_input_device().map(|d| dev_name(&d)),
        h.default_output_device().map(|d| dev_name(&d)),
    )
}

fn worker(sh: Arc<Shared>) {
    let mut streams: Option<Streams> = None;
    let mut last_open = Instant::now() - RETRY;
    let mut last_check = Instant::now();
    let mut seen_gen = sh.cfg_gen.load(Ordering::Relaxed);
    let mut defaults = default_names();
    let mut restarts = 0u32;
    let mut pcm = vec![0f32; FRAME];
    let mut mixed = vec![0f32; FRAME * 2];
    let (mut left, mut right) = (vec![0f32; FRAME], vec![0f32; FRAME]);
    let (mut conv_r, mut mix_l) = (Vec::<f32>::new(), Vec::<f32>::new());
    let mut conv = Vec::<f32>::new();
    let mut next_tick = Instant::now();
    let mut proc = Processor::new(sh.input.locked().clone());
    let mut seen_input = sh.input_gen.load(Ordering::Relaxed);
    // Adaptiver Ausgabepuffer: waechst bei Aussetzern, schrumpft nach langer Ruhe.
    let mut out_frames = OUT_TARGET_FRAMES;
    let mut seen_under = 0u32;
    let mut calm_since = Instant::now();
    let (mut burst_max, mut burst_since) = (0u32, Instant::now());

    while !sh.stop.load(Ordering::Relaxed) {
        // Streams (neu) aufbauen, wenn noetig.
        let gen = sh.cfg_gen.load(Ordering::Relaxed);
        let mut rebuild = streams.is_none() || gen != seen_gen;
        if let Some(s) = &streams {
            if s.broken.load(Ordering::Relaxed) {
                rebuild = true;
            }
            let need_in = sh.capture.load(Ordering::Relaxed) && s.in_cons.is_none();
            if need_in || s.out_prod.is_none() {
                // Es fehlt ein Geraet: regelmaessig neu versuchen (z. B. Headset eingesteckt).
                rebuild |= last_open.elapsed() >= Duration::from_secs(2);
            }
            if last_check.elapsed() >= DEVICE_CHECK {
                last_check = Instant::now();
                let now = default_names();
                let cfg = sh.cfg.locked().clone();
                if now != defaults {
                    defaults = now;
                    rebuild |= cfg.input.is_none() || cfg.output.is_none();
                }
            }
        }
        if rebuild && last_open.elapsed() >= RETRY {
            drop(streams.take()); // alte Streams zuerst schliessen
            let cfg = sh.cfg.locked().clone();
            let s = open(&cfg, sh.capture.load(Ordering::Relaxed));
            *sh.status.locked() = IoStatus {
                input: s.in_name.clone(),
                output: s.out_name.clone(),
                restarts,
            };
            restarts += 1;
            seen_gen = gen;
            seen_under = 0;
            calm_since = Instant::now();
            last_open = Instant::now();
            defaults = default_names();
            streams = Some(s);
        }

        if let Some(s) = streams.as_mut() {
            // Aufnahme -> Encoder -> Netz
            if let Some(c) = s.in_cons.as_mut() {
                let mut popped = 0u32;
                while c.occupied_len() >= FRAME {
                    c.pop_slice(&mut pcm);
                    popped += 1;
                    let ig = sh.input_gen.load(Ordering::Relaxed);
                    if ig != seen_input {
                        seen_input = ig;
                        proc.set_settings(sh.input.locked().clone());
                    }
                    let actions = proc.process(&pcm);
                    sh.level.store(proc.level().to_bits(), Ordering::Relaxed);
                    sh.open.store(proc.is_open(), Ordering::Relaxed);
                    for a in actions {
                        match a {
                            Action::Skip => sh.core.locked().skip_frame(),
                            Action::Frame(f) => {
                                let pkt = {
                                    let mut core = sh.core.locked();
                                    core.feed_monitor(&f);
                                    core.encode_frame(&f)
                                };
                                if let Some(p) = pkt {
                                    (sh.send)(p);
                                }
                            }
                        }
                    }
                }
                burst_max = burst_max.max(popped);
                if burst_since.elapsed() >= BURST_WINDOW {
                    sh.capture_burst.store(burst_max, Ordering::Relaxed);
                    burst_max = 0;
                    burst_since = Instant::now();
                }
            }
            // Mischer -> Wiedergabe
            if let Some(p) = s.out_prod.as_mut() {
                let under = s.underruns.load(Ordering::Relaxed);
                if under != seen_under {
                    sh.underruns
                        .fetch_add(under.wrapping_sub(seen_under), Ordering::Relaxed);
                    seen_under = under;
                    out_frames = (out_frames + 1).min(OUT_MAX_FRAMES);
                    calm_since = Instant::now();
                } else if out_frames > OUT_TARGET_FRAMES && calm_since.elapsed() >= OUT_SHRINK_AFTER
                {
                    out_frames -= 1;
                    calm_since = Instant::now();
                }
                sh.out_frames.store(out_frames as u32, Ordering::Relaxed);
                let target = 2 * out_frames * FRAME * s.out_rate as usize / RATE as usize;
                while p.occupied_len() < target {
                    sh.core.locked().mix_stereo(&mut mixed);
                    for (i, fr) in mixed.chunks_exact(2).enumerate() {
                        left[i] = fr[0];
                        right[i] = fr[1];
                    }
                    mix_l.clear();
                    conv_r.clear();
                    s.out_resampler.process(&left, &mut mix_l);
                    s.out_resampler_r.process(&right, &mut conv_r);
                    conv.clear();
                    for (l, r) in mix_l.iter().zip(&conv_r) {
                        conv.push(*l);
                        conv.push(*r);
                    }
                    p.push_slice(&conv);
                }
            }
        } else {
            // Ohne Ausgabegeraet laeuft der Mischer weiter, damit Jitter-Buffer nicht anstauen.
            let mut c = sh.core.locked();
            c.mix_stereo(&mut mixed);
            std::thread::sleep(Duration::from_millis(15));
        }
        next_tick += TICK;
        let now = Instant::now();
        if next_tick > now {
            std::thread::sleep(next_tick - now);
        } else {
            if now - next_tick > LATE_TICK {
                sh.late_ticks.fetch_add(1, Ordering::Relaxed);
            }
            next_tick = now;
        }
    }
}

impl AudioIo {
    pub fn start(
        core: Arc<Mutex<VoiceCore>>,
        send: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
        cfg: IoConfig,
        input: InputSettings,
        capture: bool,
    ) -> Self {
        let shared = Arc::new(Shared {
            core,
            send,
            stop: AtomicBool::new(false),
            cfg: Mutex::new(cfg),
            status: Mutex::new(IoStatus::default()),
            cfg_gen: AtomicU32::new(0),
            capture: AtomicBool::new(capture),
            input: Mutex::new(input),
            input_gen: AtomicU32::new(0),
            level: AtomicU32::new(0),
            open: AtomicBool::new(false),
            underruns: AtomicU32::new(0),
            late_ticks: AtomicU32::new(0),
            capture_burst: AtomicU32::new(0),
            out_frames: AtomicU32::new(OUT_TARGET_FRAMES as u32),
        });
        let sh = shared.clone();
        let thread = std::thread::Builder::new()
            .name("audio-io".into())
            .spawn(move || worker(sh))
            .ok();
        Self { shared, thread }
    }

    /// Andere Geraete waehlen; wirkt innerhalb einer halben Sekunde.
    pub fn set_devices(&self, cfg: IoConfig) {
        *self.shared.cfg.locked() = cfg;
        self.shared.cfg_gen.fetch_add(1, Ordering::Relaxed);
    }

    /// Mikrofon ein-/ausschalten (schliesst bzw. oeffnet die Aufnahme).
    pub fn set_capture(&self, on: bool) {
        if self.shared.capture.swap(on, Ordering::Relaxed) != on {
            self.shared.cfg_gen.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Rauschunterdrueckung, Sprachaktivierung, Verstaerkung usw. aendern (wirkt sofort).
    pub fn set_input(&self, s: InputSettings) {
        *self.shared.input.locked() = s;
        self.shared.input_gen.fetch_add(1, Ordering::Relaxed);
    }

    /// (Pegel 0 bis 1, sendet gerade) fuer die Pegelanzeige.
    pub fn input_level(&self) -> (f32, bool) {
        (
            f32::from_bits(self.shared.level.load(Ordering::Relaxed)),
            self.shared.open.load(Ordering::Relaxed),
        )
    }

    pub fn status(&self) -> IoStatus {
        self.shared.status.locked().clone()
    }

    /// Messwerte des Audio-Threads (Aussetzer, Verspaetungen, Aufnahme-Schuebe).
    pub fn diag(&self) -> IoDiag {
        let sh = &self.shared;
        IoDiag {
            underruns: sh.underruns.load(Ordering::Relaxed),
            late_ticks: sh.late_ticks.load(Ordering::Relaxed),
            capture_burst: sh.capture_burst.load(Ordering::Relaxed),
            out_ms: sh.out_frames.load(Ordering::Relaxed) * 20,
        }
    }
}

impl Drop for AudioIo {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_keeps_duration_and_pitch() {
        for (from, to) in [
            (44_100u32, 48_000u32),
            (48_000, 44_100),
            (16_000, 48_000),
            (96_000, 48_000),
        ] {
            let mut r = Resampler::new(from, to);
            let mut out = Vec::new();
            // 1 s 440-Hz-Ton in Bloecken wie in einem Audio-Callback.
            let total = from as usize;
            let mut i = 0usize;
            while i < total {
                let n = 441.min(total - i);
                let blk: Vec<f32> = (i..i + n)
                    .map(|k| (2.0 * std::f32::consts::PI * 440.0 * k as f32 / from as f32).sin())
                    .collect();
                r.process(&blk, &mut out);
                i += n;
            }
            let err = out.len() as i64 - to as i64;
            assert!(err.abs() <= 2, "{from}->{to}: {} Samples", out.len());
            // Nulldurchgaenge zaehlen: 440 Hz => ca. 880 pro Sekunde.
            let zc = out.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
            assert!((435..=445).contains(&zc), "{from}->{to}: {zc} Perioden");
        }
    }

    #[test]
    fn device_listing_does_not_panic() {
        // Auf Build-Servern ohne Audio sind die Listen leer; wichtig ist, dass nichts abstuerzt.
        let _ = list_inputs();
        let _ = list_outputs();
    }

    #[test]
    fn io_starts_and_stops_without_devices_or_with_them() {
        let core = Arc::new(Mutex::new(VoiceCore::new(64).unwrap()));
        let sent = Arc::new(AtomicU32::new(0));
        let s2 = sent.clone();
        let io = AudioIo::start(
            core,
            Arc::new(move |_| {
                s2.fetch_add(1, Ordering::Relaxed);
            }),
            IoConfig::default(),
            InputSettings::default(),
            true,
        );
        std::thread::sleep(Duration::from_millis(700));
        let _ = io.status();
        io.set_devices(IoConfig {
            input: Some("gibt es nicht".into()),
            output: None,
        });
        std::thread::sleep(Duration::from_millis(1200));
        drop(io); // darf nicht haengen
    }
}

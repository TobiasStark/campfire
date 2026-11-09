//! Streams (M3-06): eigenen Bildschirm/Fenster an den Voice-Channel senden und fremde Streams
//! ansehen. Verbindet Capture, Encoder, `VideoSender` und – auf der Zuschauerseite –
//! `ReceiverTracker`, Decoder und Anzeige.
//!
//! * Sender: Quelle aufnehmen -> Simulcast-Encoder -> `VideoSender`. Einmal pro Sekunde werden die
//!   Berichte der Zuschauer ausgewertet (Bitrate, Stufenwahl) und der Stream an alle
//!   Voice-Teilnehmer angekuendigt.
//! * Zuschauer: `Subscribe` an den Sender, jeder Frame kommt auf einem eigenen Stream, wird
//!   vermessen, dekodiert und im `FrameSlot` abgelegt; einmal pro Sekunde geht ein Bericht zurueck.
//!
//! Nur Teilnehmer des eigenen Voice-Channels duerfen einen Stream ansehen oder ankuendigen.

use app_audio::service::VoiceService;
#[cfg(feature = "video")]
use app_net::rate::Budget;
use app_net::video::{read_video_frame, Arrival, ReceiverTracker, VideoSender};
use app_net::{Net, NetEvent, PathKind};
use app_proto::control::ControlMessage;
use app_proto::video::{FrameHeader, LayerInfo, RefuseReason, VideoCodec, VideoMsg};
use app_proto::PublicKey;
use app_video::capture::Capturer;
use app_video::frame::FrameSlot;
use app_video::select::{Codec, StreamOptions};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Jeder Absender hat hoechstens einen Stream.
/// Ungefaehre Verzoegerung zwischen Mischen und Hoerbarwerden (Ringpuffer + Soundkarte).
const OUTPUT_LATENCY_MS: u32 = 40;
const STREAM_ID: u8 = 1;
/// Eine Ankuendigung gilt so lange, ohne dass eine neue kommt.
const ANNOUNCE_TTL: Duration = Duration::from_secs(8);
/// Mindestabstand zwischen zwei Keyframe-Anforderungen des Zuschauers.
const KEY_GAP: Duration = Duration::from_millis(300);

pub fn to_wire(c: Codec) -> VideoCodec {
    match c {
        Codec::H264 => VideoCodec::H264,
        Codec::Hevc => VideoCodec::Hevc,
        Codec::Av1 => VideoCodec::Av1,
    }
}

pub fn from_wire(c: VideoCodec) -> Codec {
    match c {
        VideoCodec::H264 => Codec::H264,
        VideoCodec::Hevc => Codec::Hevc,
        VideoCodec::Av1 => Codec::Av1,
    }
}

/// Ein Stream, den jemand im Voice-Channel anbietet.
#[derive(Debug, Clone)]
pub struct AvailableStream {
    pub device: PublicKey,
    pub name: String,
    /// Beschreibung der groessten Stufe, z. B. "1080p60".
    pub quality: String,
    pub audio: bool,
}

struct Announced {
    name: String,
    layers: Vec<LayerInfo>,
    audio: bool,
    seen: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchState {
    Connecting,
    Playing,
    Refused(RefuseReason),
    Ended,
    /// Dieser Build kann nicht dekodieren (kein ffmpeg).
    Unsupported,
}

/// Messwerte fuer das Statistik-Overlay.
#[derive(Debug, Clone)]
pub struct ViewStats {
    pub fps: u32,
    pub kbps: u32,
    pub codec: Option<VideoCodec>,
    pub size: Option<(u32, u32)>,
    pub layer: u8,
    pub path: PathKind,
    /// Verzoegerung gegenueber der besten bisher gesehenen Zustellung (die Uhren von Sender und
    /// Zuschauer sind nicht abgeglichen, eine absolute Latenz gibt es deshalb nicht).
    pub delay_ms: u32,
    pub jitter_ms: u32,
    pub lost_percent: u32,
    pub hardware_decode: bool,
}

#[derive(Default)]
struct WatchCounters {
    decoded: AtomicU64,
    fps: AtomicU32,
    kbps: AtomicU32,
    jitter: AtomicU32,
    lost_pct: AtomicU32,
    layer: AtomicU32,
    codec: AtomicU32,
    width: AtomicU32,
    height: AtomicU32,
    delay: AtomicU32,
    hw: AtomicBool,
}

struct Job {
    ts: u32,
    codec: VideoCodec,
    data: Vec<u8>,
}

struct Watch {
    device: PublicKey,
    name: String,
    slot: Arc<FrameSlot>,
    tracker: Mutex<ReceiverTracker>,
    start: Instant,
    state: Mutex<WatchState>,
    closed: AtomicBool,
    last_key: Mutex<Option<Instant>>,
    min_transit: AtomicI64,
    counters: WatchCounters,
    tx: Mutex<Option<std::sync::mpsc::SyncSender<Job>>>,
}

impl Watch {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn set_state(&self, s: WatchState) {
        *self.state.lock().unwrap() = s;
    }
}

/// Alles, was Capture-Rueckruf und Takt-Aufgabe des Senders gemeinsam brauchen.
struct OutShared {
    sender: VideoSender,
    stop: AtomicBool,
    /// Keyframe gewuenscht, je Stufe.
    key: [AtomicBool; 2],
    /// Neue Zielbitrate (kbit/s) je Stufe, 0 = keine.
    bitrate: [AtomicU32; 2],
    codec: VideoCodec,
    layers: Vec<LayerInfo>,
    error: Mutex<Option<String>>,
    frames: AtomicU64,
    /// Laeuft Stream-Ton? (Announce.audio und Budget)
    audio: AtomicBool,
    /// Hinweis zum Ton, z.B. "Spielsound benoetigt Windows 11".
    note: Mutex<Option<String>>,
    /// Anzeigenamen der Zuschauer (aus dem Abo).
    names: Mutex<HashMap<PublicKey, String>>,
}

struct Outgoing {
    shared: Arc<OutShared>,
    capturer: Capturer,
    #[cfg(feature = "video")]
    audio: Option<app_video::loopback::LoopbackCapture>,
}

#[derive(Default)]
struct State {
    out: Option<Outgoing>,
    available: HashMap<PublicKey, Announced>,
    watch: Option<Arc<Watch>>,
}

struct Inner {
    net: Net,
    voice: VoiceService,
    rt: tokio::runtime::Handle,
    notify: Arc<dyn Fn() + Send + Sync>,
    state: Mutex<State>,
    /// Lautstaerke des Stream-Tons, getrennt vom Voice (0..=1). Der Ton selbst kommt mit M3-07.
    volume: Mutex<f32>,
}

#[derive(Clone)]
pub struct StreamService {
    inner: Arc<Inner>,
}

impl StreamService {
    /// Muss innerhalb einer tokio-Laufzeit aufgerufen werden.
    pub fn new(net: Net, voice: VoiceService, notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            inner: Arc::new(Inner {
                net,
                voice,
                rt: tokio::runtime::Handle::current(),
                notify,
                state: Mutex::new(State::default()),
                volume: Mutex::new(1.0),
            }),
        }
    }

    fn changed(&self) {
        (self.inner.notify)();
    }

    /// Geraete der anderen Teilnehmer im Voice-Channel.
    fn peers(&self) -> Vec<PublicKey> {
        let me = self.inner.net.device_id();
        self.inner
            .voice
            .participants()
            .into_iter()
            .map(|p| p.device)
            .filter(|d| *d != me)
            .collect()
    }

    fn is_peer(&self, device: &PublicKey) -> bool {
        self.peers().contains(device)
    }

    fn send(&self, to: PublicKey, msg: VideoMsg) {
        let net = self.inner.net.clone();
        self.inner.rt.spawn(async move {
            let _ = net.send_control(&to, ControlMessage::Video(msg)).await;
        });
    }

    // ---------------------------------------------------------------- Lautstaerke

    pub fn volume(&self) -> f32 {
        *self.inner.volume.lock().unwrap()
    }

    pub fn set_volume(&self, v: f32) {
        let v = v.clamp(0.0, 1.0);
        *self.inner.volume.lock().unwrap() = v;
        self.inner.voice.core().lock().unwrap().set_stream_volume(v);
    }

    // ---------------------------------------------------------------- Zuschauer

    /// Streams, die gerade angeboten werden (ohne den, den man schon ansieht).
    pub fn available(&self) -> Vec<AvailableStream> {
        let mut st = self.inner.state.lock().unwrap();
        st.available.retain(|_, a| a.seen.elapsed() < ANNOUNCE_TTL);
        let mut v: Vec<AvailableStream> = st
            .available
            .iter()
            .map(|(d, a)| AvailableStream {
                device: *d,
                name: a.name.clone(),
                quality: a
                    .layers
                    .first()
                    .map(describe)
                    .unwrap_or_else(|| "?".to_owned()),
                audio: a.audio,
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Streamt dieses Geraet gerade (laut frischer Ankuendigung)?
    pub fn is_live(&self, device: &PublicKey) -> bool {
        self.inner
            .state
            .lock()
            .unwrap()
            .available
            .get(device)
            .is_some_and(|a| a.seen.elapsed() < ANNOUNCE_TTL)
    }

    /// Fremden Stream ansehen. Ein bereits laufender Stream wird beendet.
    pub fn watch(&self, device: PublicKey) {
        self.unwatch();
        let name = {
            let st = self.inner.state.lock().unwrap();
            st.available
                .get(&device)
                .map(|a| a.name.clone())
                .unwrap_or_default()
        };
        let slot = Arc::new(FrameSlot::default());
        let w = Arc::new(Watch {
            device,
            name,
            slot,
            tracker: Mutex::new(ReceiverTracker::new(STREAM_ID, 0)),
            start: Instant::now(),
            state: Mutex::new(WatchState::Connecting),
            closed: AtomicBool::new(false),
            last_key: Mutex::new(None),
            min_transit: AtomicI64::new(i64::MAX),
            counters: WatchCounters::default(),
            tx: Mutex::new(None),
        });
        #[cfg(feature = "video")]
        {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(8);
            *w.tx.lock().unwrap() = Some(tx);
            let (w2, svc) = (w.clone(), self.clone());
            std::thread::Builder::new()
                .name("stream-decode".into())
                .spawn(move || decode_thread(&w2, &svc, &rx))
                .ok();
        }
        #[cfg(not(feature = "video"))]
        w.set_state(WatchState::Unsupported);

        self.inner.state.lock().unwrap().watch = Some(w.clone());
        {
            let core = self.inner.voice.core();
            let mut core = core.lock().unwrap();
            core.set_stream_volume(self.volume());
            core.set_stream_source(Some(device));
        }
        if !cfg!(feature = "video") {
            self.changed();
            return;
        }
        self.send(
            device,
            VideoMsg::Subscribe {
                stream_id: STREAM_ID,
                layer: 0,
                decoders: decoders(),
            },
        );
        // Bericht jede Sekunde, bis der Stream endet.
        let svc = self.clone();
        self.inner.rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            let mut last = 0u64;
            loop {
                tick.tick().await;
                if w.closed.load(Ordering::Acquire) {
                    break;
                }
                let r = w.tracker.lock().unwrap().report(w.now_ms());
                let decoded = w.counters.decoded.load(Ordering::Relaxed);
                let c = &w.counters;
                c.fps.store(
                    ((decoded - last) * 1000 / u64::from(r.interval_ms.max(1))) as u32,
                    Ordering::Relaxed,
                );
                last = decoded;
                c.kbps.store(r.kbps, Ordering::Relaxed);
                c.jitter.store(r.jitter_ms, Ordering::Relaxed);
                let total = r.received + r.lost + r.late;
                c.lost_pct.store(
                    if total == 0 {
                        0
                    } else {
                        (r.lost + r.late) * 100 / total
                    },
                    Ordering::Relaxed,
                );
                svc.send(w.device, VideoMsg::Report(r));
                svc.changed();
            }
        });
        self.changed();
    }

    pub fn unwatch(&self) {
        let w = self.inner.state.lock().unwrap().watch.take();
        if let Some(w) = w {
            self.inner
                .voice
                .core()
                .lock()
                .unwrap()
                .set_stream_source(None);
            w.closed.store(true, Ordering::Release);
            w.tx.lock().unwrap().take();
            if matches!(
                *w.state.lock().unwrap(),
                WatchState::Connecting | WatchState::Playing
            ) {
                self.send(
                    w.device,
                    VideoMsg::Unsubscribe {
                        stream_id: STREAM_ID,
                    },
                );
            }
            self.changed();
        }
    }

    /// Der Stream, den man gerade ansieht.
    pub fn watching(&self) -> Option<WatchInfo> {
        let w = self.inner.state.lock().unwrap().watch.clone()?;
        // Bild nach dem Ton richten: die Anzeige zeigt das Bild zur Abspielposition des Tons
        // (abzueglich der Ausgabeverzoegerung der Soundkarte).
        let pos = self.inner.voice.core().lock().unwrap().stream_position();
        w.slot
            .set_audio_pos(pos.map(|p| p.wrapping_sub(OUTPUT_LATENCY_MS)));
        let state = w.state.lock().unwrap().clone();
        let c = &w.counters;
        let size = match (
            c.width.load(Ordering::Relaxed),
            c.height.load(Ordering::Relaxed),
        ) {
            (0, _) | (_, 0) => None,
            (a, b) => Some((a, b)),
        };
        let stats = ViewStats {
            fps: c.fps.load(Ordering::Relaxed),
            kbps: c.kbps.load(Ordering::Relaxed),
            codec: VideoCodec::from_byte(c.codec.load(Ordering::Relaxed) as u8),
            size,
            layer: c.layer.load(Ordering::Relaxed) as u8,
            path: self.inner.net.path_kind(&w.device),
            delay_ms: c.delay.load(Ordering::Relaxed),
            jitter_ms: c.jitter.load(Ordering::Relaxed),
            lost_percent: c.lost_pct.load(Ordering::Relaxed),
            hardware_decode: c.hw.load(Ordering::Relaxed),
        };
        Some(WatchInfo {
            device: w.device,
            name: w.name.clone(),
            state,
            slot: w.slot.clone(),
            stats,
        })
    }

    fn request_key(&self, w: &Arc<Watch>) {
        let mut last = w.last_key.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < KEY_GAP) {
            return;
        }
        *last = Some(Instant::now());
        self.send(
            w.device,
            VideoMsg::KeyframeRequest {
                stream_id: STREAM_ID,
            },
        );
    }

    fn on_incoming_stream(&self, from: PublicKey, mut recv: iroh::endpoint::RecvStream) {
        let Some(w) = self.inner.state.lock().unwrap().watch.clone() else {
            return;
        };
        if w.device != from || w.closed.load(Ordering::Acquire) {
            return;
        }
        let svc = self.clone();
        self.inner.rt.spawn(async move {
            match read_video_frame(&mut recv, Duration::from_secs(2)).await {
                Ok((h, body)) => svc.on_frame(&w, h, body),
                Err(_) => {
                    w.tracker.lock().unwrap().on_aborted();
                    svc.request_key(&w);
                }
            }
        });
    }

    fn on_frame(&self, w: &Arc<Watch>, h: FrameHeader, body: Vec<u8>) {
        if h.stream_id != STREAM_ID {
            return;
        }
        let now = w.now_ms();
        let arrival = w.tracker.lock().unwrap().on_frame(&h, now);
        match arrival {
            Arrival::Drop => {}
            Arrival::DropAndRequestKey => self.request_key(w),
            Arrival::Decode => {
                let transit = now as i64 - i64::from(h.ts);
                let best = w
                    .min_transit
                    .fetch_min(transit, Ordering::Relaxed)
                    .min(transit);
                let c = &w.counters;
                c.delay
                    .store((transit - best).max(0) as u32, Ordering::Relaxed);
                c.layer.store(u32::from(h.layer), Ordering::Relaxed);
                c.codec
                    .store(u32::from(h.codec.to_byte()), Ordering::Relaxed);
                let tx = w.tx.lock().unwrap().clone();
                if let Some(tx) = tx {
                    let job = Job {
                        ts: h.ts,
                        codec: h.codec,
                        data: body,
                    };
                    if tx.try_send(job).is_err() {
                        // Decoder kommt nicht nach: Folgebilder waeren unbrauchbar.
                        w.tracker.lock().unwrap().on_aborted();
                        self.request_key(w);
                    }
                }
            }
        }
    }

    // ---------------------------------------------------------------- Sender

    pub fn is_streaming(&self) -> bool {
        self.inner.state.lock().unwrap().out.is_some()
    }

    /// Anzahl der Zuschauer und letzter Fehler des Encoders.
    pub fn outgoing_info(&self) -> Option<OutInfo> {
        let st = self.inner.state.lock().unwrap();
        let o = st.out.as_ref()?;
        let names = o.shared.names.lock().unwrap();
        let mut watchers: Vec<String> = o
            .shared
            .sender
            .viewers()
            .iter()
            .map(|d| {
                names
                    .get(d)
                    .cloned()
                    .unwrap_or_else(|| "Unbekannt".to_owned())
            })
            .collect();
        watchers.sort();
        let info = OutInfo {
            watchers,
            viewers: o.shared.sender.viewers().len(),
            kbps: o.shared.sender.target_kbps(),
            layers: o.shared.sender.layers(),
            error: o.shared.error.lock().unwrap().clone(),
            note: o.shared.note.lock().unwrap().clone(),
        };
        Some(info)
    }

    /// Beginnt zu senden. Ohne ffmpeg im Build gibt es einen Fehler.
    pub fn start(
        &self,
        source: app_video::capture::SourceId,
        cursor: bool,
        opts: StreamOptions,
    ) -> Result<(), String> {
        #[cfg(not(feature = "video"))]
        {
            let _ = (source, cursor, opts);
            Err("Dieser Build enthält kein ffmpeg und kann nicht streamen.".to_owned())
        }
        #[cfg(feature = "video")]
        {
            use app_video::encode::{probe, Bgra, Simulcast};
            use app_video::select::{choose, DecoderCaps};
            self.stop();
            let opts = opts.sanitized();
            // Zuschauer sind noch unbekannt: bei "Auto" nur H.264, das alle dekodieren koennen.
            let sel = choose(&opts, probe(), &[DecoderCaps::H264_ONLY]);
            let info = sel.best().ok_or("Kein Encoder gefunden")?;
            let codec = to_wire(info.codec);
            let layer_opts = opts.layers();
            let layers: Vec<LayerInfo> = layer_opts
                .iter()
                .enumerate()
                .map(|(i, o)| LayerInfo {
                    layer: i as u8,
                    codec,
                    width: 0,
                    height: o.resolution.lines().unwrap_or(0) as u16,
                    fps: o.fps as u8,
                    kbps: o.max_mbps * 1000,
                })
                .collect();
            let sender = VideoSender::new(self.inner.net.clone(), STREAM_ID, codec);
            sender.set_budget(Budget::default(), self.peers().len() as u32);
            sender.set_layers(layers.len() as u8);
            let shared = Arc::new(OutShared {
                sender,
                stop: AtomicBool::new(false),
                key: Default::default(),
                bitrate: Default::default(),
                codec,
                layers,
                error: Mutex::new(None),
                frames: AtomicU64::new(0),
                audio: AtomicBool::new(false),
                note: Mutex::new(None),
                names: Mutex::new(HashMap::new()),
            });
            let mut sim = Simulcast::new(info, &opts);
            let (sh, handle, t0) = (shared.clone(), self.inner.rt.clone(), Instant::now());
            let mut capturer = Capturer::new(move |f| {
                if sh.stop.load(Ordering::Relaxed) {
                    return;
                }
                let _g = handle.enter();
                for l in 0..sim.layer_count() {
                    if sh.key[usize::from(l)].swap(false, Ordering::AcqRel) {
                        sim.request_keyframe(l);
                    }
                    let b = sh.bitrate[usize::from(l)].swap(0, Ordering::AcqRel);
                    if b > 0 {
                        sim.set_bitrate_kbps(l, b);
                    }
                }
                let ts = t0.elapsed().as_millis() as i64;
                let img = Bgra {
                    data: f.data,
                    width: f.width,
                    height: f.height,
                    pitch: f.pitch,
                };
                match sim.encode(&img, ts) {
                    Ok(pkts) => {
                        for (layer, p) in pkts {
                            let out = sh.sender.send(&app_net::video::OutFrame {
                                layer,
                                key: p.key,
                                codec: sh.codec,
                                ts: ts as u32,
                                data: bytes::Bytes::from(p.data),
                            });
                            sh.frames.fetch_add(1, Ordering::Relaxed);
                            if out.want_key {
                                sh.key[usize::from(layer)].store(true, Ordering::Release);
                            }
                        }
                    }
                    Err(e) => *sh.error.lock().unwrap() = Some(e),
                }
            });
            let _ = capturer.set_cursor(cursor);
            capturer.start(source)?;
            let audio = self.start_audio(&shared, source, opts.audio, t0);
            self.inner.state.lock().unwrap().out = Some(Outgoing {
                shared: shared.clone(),
                capturer,
                audio,
            });
            self.spawn_tick(shared);
            self.changed();
            Ok(())
        }
    }

    /// Spielsound aufnehmen, als Opus-Stereo kodieren und an alle Zuschauer schicken.
    /// Fehlt die Funktion (Windows 10), laeuft der Stream ohne Ton weiter.
    #[cfg(feature = "video")]
    fn start_audio(
        &self,
        shared: &Arc<OutShared>,
        source: app_video::capture::SourceId,
        mode: app_video::select::AudioMode,
        t0: Instant,
    ) -> Option<app_video::loopback::LoopbackCapture> {
        use app_audio::stream_audio::{StreamEncoder, DEFAULT_KBPS};
        use app_video::loopback::{window_pid, LoopbackCapture, Mode};
        use app_video::select::AudioMode;
        let lb = match (mode, source) {
            (AudioMode::Off, _) => return None,
            (AudioMode::OnlyProgram, app_video::capture::SourceId::Window(h)) => {
                match window_pid(h) {
                    Some(pid) => Mode::IncludeTree(pid),
                    None => return None,
                }
            }
            // "Nur das Programm" ohne Fenster ergibt keinen Sinn: dann alles ausser Campfire.
            _ => Mode::ExcludeTree(std::process::id()),
        };
        let mut enc = StreamEncoder::new(DEFAULT_KBPS).ok()?;
        let (sh, net) = (shared.clone(), self.inner.net.clone());
        let res = LoopbackCapture::start(lb, move |block| {
            if sh.stop.load(Ordering::Relaxed) {
                return;
            }
            let Some(pkt) = enc.encode(block, t0.elapsed().as_millis() as u32) else {
                return;
            };
            let pkt = bytes::Bytes::from(pkt);
            for d in sh.sender.viewers() {
                let _ = net.send_datagram(&d, pkt.clone());
            }
        });
        match res {
            Ok(c) => {
                shared.audio.store(true, Ordering::Release);
                Some(c)
            }
            Err(_) => {
                *shared.note.lock().unwrap() = Some("Spielsound benötigt Windows 11".to_owned());
                None
            }
        }
    }

    pub fn stop(&self) {
        let out = self.inner.state.lock().unwrap().out.take();
        if let Some(mut o) = out {
            o.shared.stop.store(true, Ordering::Release);
            #[cfg(feature = "video")]
            if let Some(a) = o.audio.as_mut() {
                a.stop();
            }
            o.capturer.stop();
            for d in self.peers() {
                self.send(
                    d,
                    VideoMsg::Stop {
                        stream_id: STREAM_ID,
                    },
                );
            }
            self.changed();
        }
    }

    #[cfg(feature = "video")]
    fn spawn_tick(&self, shared: Arc<OutShared>) {
        let svc = self.clone();
        self.inner.rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            let mut n = 0u32;
            loop {
                tick.tick().await;
                if shared.stop.load(Ordering::Acquire) {
                    break;
                }
                // Quelle verschwunden (Fenster geschlossen)?
                let alive = {
                    let st = svc.inner.state.lock().unwrap();
                    st.out.as_ref().is_some_and(|o| o.capturer.is_running())
                };
                if !alive {
                    svc.stop();
                    break;
                }
                let ad = shared.sender.adapt();
                for (l, k) in ad.bitrates {
                    shared.bitrate[usize::from(l)].store(k, Ordering::Release);
                }
                for l in ad.key_layers {
                    shared.key[usize::from(l)].store(true, Ordering::Release);
                }
                let peers = svc.peers();
                // Der Ton (160 kbit/s je Zuschauer) kommt vom selben Upload: er zaehlt wie
                // drei Voice-Partner (3 x 64 kbit/s) je Zuschauer.
                let audio_load = if shared.audio.load(Ordering::Acquire) {
                    3 * shared.sender.viewers().len() as u32
                } else {
                    0
                };
                shared
                    .sender
                    .set_budget(Budget::default(), peers.len() as u32 + audio_load);
                if n.is_multiple_of(2) {
                    for d in peers {
                        svc.send(
                            d,
                            VideoMsg::Announce {
                                stream_id: STREAM_ID,
                                layers: shared.layers.clone(),
                                audio: shared.audio.load(Ordering::Acquire),
                            },
                        );
                    }
                }
                n = n.wrapping_add(1);
                svc.changed();
            }
        });
    }

    // ---------------------------------------------------------------- Netzwerk

    pub fn on_net_event(&self, ev: &NetEvent) {
        match ev {
            NetEvent::PeerDown { device } => {
                if let Some(o) = &self.inner.state.lock().unwrap().out {
                    o.shared.sender.remove_viewer(device);
                }
                let mut ended = false;
                {
                    let mut st = self.inner.state.lock().unwrap();
                    st.available.remove(device);
                    if let Some(w) = &st.watch {
                        if w.device == *device {
                            w.set_state(WatchState::Ended);
                            ended = true;
                        }
                    }
                }
                if ended {
                    self.changed();
                }
            }
            NetEvent::Control {
                from,
                msg: ControlMessage::Video(m),
            } => self.on_video_msg(from.device, &from.display_name, m),
            _ => {}
        }
    }

    /// Eingehender Frame-Stream (Netz-Ereignis `VideoStream`; der Core reicht ihn hierher).
    pub fn on_video_stream(&self, from: PublicKey, recv: iroh::endpoint::RecvStream) {
        self.on_incoming_stream(from, recv);
    }

    fn on_video_msg(&self, from: PublicKey, name: &str, m: &VideoMsg) {
        match m {
            VideoMsg::Announce {
                stream_id,
                layers,
                audio,
            } if *stream_id == STREAM_ID => {
                if !self.is_peer(&from) {
                    return;
                }
                self.inner.state.lock().unwrap().available.insert(
                    from,
                    Announced {
                        name: name.to_owned(),
                        layers: layers.clone(),
                        audio: *audio,
                        seen: Instant::now(),
                    },
                );
                self.changed();
            }
            VideoMsg::Stop { stream_id } if *stream_id == STREAM_ID => {
                let mut st = self.inner.state.lock().unwrap();
                st.available.remove(&from);
                if let Some(w) = &st.watch {
                    if w.device == from {
                        w.set_state(WatchState::Ended);
                    }
                }
                drop(st);
                self.changed();
            }
            VideoMsg::Refused { reason, .. } => {
                let st = self.inner.state.lock().unwrap();
                if let Some(w) = &st.watch {
                    if w.device == from {
                        w.set_state(WatchState::Refused(*reason));
                    }
                }
                drop(st);
                self.changed();
            }
            VideoMsg::Subscribe { .. }
            | VideoMsg::Unsubscribe { .. }
            | VideoMsg::KeyframeRequest { .. }
            | VideoMsg::Report(_) => {
                let shared = self
                    .inner
                    .state
                    .lock()
                    .unwrap()
                    .out
                    .as_ref()
                    .map(|o| o.shared.clone());
                let Some(shared) = shared else {
                    if matches!(m, VideoMsg::Subscribe { stream_id, .. } if *stream_id == STREAM_ID)
                    {
                        self.refuse(from, RefuseReason::NoSuchStream);
                    }
                    return;
                };
                if !self.is_peer(&from) {
                    if matches!(m, VideoMsg::Subscribe { .. }) {
                        self.refuse(from, RefuseReason::NotAllowed);
                    }
                    return;
                }
                if let VideoMsg::Subscribe { .. } = m {
                    shared.names.lock().unwrap().insert(from, name.to_owned());
                }
                match shared.sender.on_message(&from, m) {
                    Ok(true) => {
                        let layer = shared.sender.viewer_layer(&from).unwrap_or(0);
                        shared.key[usize::from(layer.min(1))].store(true, Ordering::Release);
                        self.changed();
                    }
                    Ok(false) => {}
                    Err(reason) => self.refuse(from, reason),
                }
            }
            _ => {}
        }
    }

    fn refuse(&self, to: PublicKey, reason: RefuseReason) {
        self.send(
            to,
            VideoMsg::Refused {
                stream_id: STREAM_ID,
                reason,
            },
        );
    }
}

/// Was die Oberflaeche ueber den angesehenen Stream wissen muss.
pub struct WatchInfo {
    pub device: PublicKey,
    pub name: String,
    pub state: WatchState,
    pub slot: Arc<FrameSlot>,
    pub stats: ViewStats,
}

pub struct OutInfo {
    pub viewers: usize,
    pub kbps: u32,
    pub layers: u8,
    pub error: Option<String>,
    /// Namen der aktuellen Zuschauer.
    pub watchers: Vec<String>,
    /// Hinweis, der kein Fehler ist (z.B. kein Spielsound unter Windows 10).
    pub note: Option<String>,
}

fn describe(l: &LayerInfo) -> String {
    if l.height > 0 {
        format!("{}p{}", l.height, l.fps)
    } else {
        format!("{} fps", l.fps)
    }
}

/// Welche Codecs dieser Rechner dekodieren kann.
fn decoders() -> Vec<VideoCodec> {
    #[cfg(feature = "video")]
    {
        app_video::decode::supported()
            .into_iter()
            .map(to_wire)
            .collect()
    }
    #[cfg(not(feature = "video"))]
    {
        vec![VideoCodec::H264]
    }
}

/// Dekodiert die ankommenden Frames; bei Fehlern wird ein Keyframe angefordert.
#[cfg(feature = "video")]
fn decode_thread(w: &Arc<Watch>, svc: &StreamService, rx: &std::sync::mpsc::Receiver<Job>) {
    use app_video::decode::Decoder;
    let mut dec: Option<Decoder> = None;
    while let Ok(job) = rx.recv() {
        if w.closed.load(Ordering::Acquire) {
            break;
        }
        let codec = from_wire(job.codec);
        if dec.as_ref().is_none_or(|d| d.codec() != codec) {
            match Decoder::new(codec, true) {
                Ok(d) => {
                    w.counters.hw.store(d.is_hardware(), Ordering::Relaxed);
                    dec = Some(d);
                }
                Err(_) => {
                    w.set_state(WatchState::Unsupported);
                    svc.changed();
                    break;
                }
            }
        }
        let Some(d) = dec.as_mut() else { continue };
        match d.decode(&job.data) {
            Ok(frames) => {
                if let Some(mut last) = frames.into_iter().last() {
                    last.ts = job.ts;
                    w.counters.width.store(last.w, Ordering::Relaxed);
                    w.counters.height.store(last.h, Ordering::Relaxed);
                    w.counters.decoded.fetch_add(1, Ordering::Relaxed);
                    w.slot.set(last);
                    let mut s = w.state.lock().unwrap();
                    if *s == WatchState::Connecting {
                        *s = WatchState::Playing;
                        drop(s);
                        svc.changed();
                    }
                }
            }
            Err(_) => {
                w.tracker.lock().unwrap().on_aborted();
                svc.request_key(w);
            }
        }
    }
}

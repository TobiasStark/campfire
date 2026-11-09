//! Video-Transport (M3-03): Fan-out zu mehreren Zuschauern und Empfangsmessung.
//!
//! * [`VideoSender`] verteilt Frames. Jeder Frame geht auf einem eigenen uni-Stream; kommt er
//!   nicht rechtzeitig an, wird der Stream zurueckgesetzt. Ein Zuschauer, bei dem zu viele Frames
//!   offen sind, bekommt keine weiteren, bis ein Keyframe kommt. Die anderen Zuschauer merken
//!   davon nichts, weil jeder Frame in einer eigenen Task pro Zuschauer gesendet wird.
//! * [`ReceiverTracker`] sortiert verspaetete Frames aus, erkennt Luecken (dann wird ein Keyframe
//!   angefordert) und misst Verlust, Jitter und Durchsatz fuer den `ReceiverReport`.

use crate::rate::{Budget, LayerPicker, RateController, LAYER1_KBPS};
use crate::{Net, NetError};
use app_proto::control::ControlMessage;
use app_proto::video::{
    FrameHeader, ReceiverReport, RefuseReason, VideoCodec, VideoMsg, FLAG_KEY, HEADER_LEN,
};
use app_proto::PublicKey;
use bytes::Bytes;
use iroh::endpoint::RecvStream;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Wie viele Frames pro Zuschauer gleichzeitig unterwegs sein duerfen.
pub const MAX_INFLIGHT: u32 = 4;
/// Nach dieser Zeit gilt ein Frame als verloren und der Stream wird zurueckgesetzt.
pub const FRAME_DEADLINE: Duration = Duration::from_millis(500);
/// Mindestabstand zwischen zwei automatischen Keyframe-Anforderungen.
pub const KEY_REQUEST_GAP: Duration = Duration::from_millis(300);

/// Ein kodierter Frame vom Encoder.
#[derive(Debug, Clone)]
pub struct OutFrame {
    pub layer: u8,
    pub key: bool,
    pub codec: VideoCodec,
    /// Millisekunden auf der Medienuhr des Senders.
    pub ts: u32,
    pub data: Bytes,
}

/// Was mit einem Frame fuer einen Zuschauer geschieht.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Send,
    Skip,
}

/// Zustand eines Zuschauers beim Sender (reine Logik, ohne Netz).
#[derive(Debug, Clone)]
pub struct Policy {
    needs_key: bool,
}

impl Policy {
    /// Ein neuer Zuschauer steigt immer mit einem Keyframe ein.
    pub fn new() -> Self {
        Self { needs_key: true }
    }

    pub fn needs_key(&self) -> bool {
        self.needs_key
    }

    pub fn want_key(&mut self) {
        self.needs_key = true;
    }

    /// `inflight`: Frames, die dieser Zuschauer noch nicht bestaetigt hat.
    pub fn decide(&mut self, key: bool, inflight: u32) -> Decision {
        if self.needs_key && !key {
            return Decision::Skip;
        }
        if inflight >= MAX_INFLIGHT {
            // Ein ausgelassener Frame macht alle folgenden unbrauchbar, bis ein Keyframe kommt.
            self.needs_key = true;
            return Decision::Skip;
        }
        if key {
            self.needs_key = false;
        }
        Decision::Send
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self::new()
    }
}

/// Hoechstzahl der Qualitaetsstufen (0 = Original, 1 = 720p30).
pub const MAX_LAYERS: u8 = 2;

struct Sub {
    picker: LayerPicker,
    codecs: Vec<VideoCodec>,
    policy: Policy,
    inflight: Arc<AtomicU32>,
    /// Wird von der Sende-Task gesetzt, wenn ein Frame nicht ankam.
    failed: Arc<AtomicBool>,
}

/// Ergebnis von [`VideoSender::send`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SendOutcome {
    pub sent: usize,
    pub skipped: usize,
    /// Der Encoder soll einen Keyframe erzeugen.
    pub want_key: bool,
}

struct SenderState {
    subs: HashMap<PublicKey, Sub>,
    /// Zaehler je Stufe: ein Zuschauer sieht nur die Frames seiner Stufe, die Nummern muessen
    /// bei ihm lueckenlos sein.
    frame_no: [u32; MAX_LAYERS as usize],
    last_key_request: Option<Instant>,
    budget: Budget,
    /// Gespraechspartner im Voice (haben Vorrang vor Video).
    voice_peers: u32,
    /// Angebotene Stufen (1 = nur Original, 2 = Simulcast).
    layers: u8,
    /// Ein Regler pro Stufe.
    rate: [RateController; MAX_LAYERS as usize],
    reports: HashMap<PublicKey, ReceiverReport>,
}

impl SenderState {
    fn update_cap(&mut self) {
        let total = self.subs.len() as u32;
        if self.layers < 2 {
            let cap = self.budget.cap_kbps(self.voice_peers, total);
            self.rate[0].set_cap(cap);
            return;
        }
        // Jeder Zuschauer bekommt seine Stufe einzeln: Upload = n0 * r0 + n1 * r1.
        let low = self.subs.values().filter(|s| s.picker.layer() > 0).count() as u32;
        let video = self.budget.video_kbps(self.voice_peers);
        let cap1 = LAYER1_KBPS.min(video / total.max(1));
        let cap0 = video.saturating_sub(low * cap1) / (total - low).max(1);
        self.rate[0].set_cap(cap0);
        self.rate[1].set_cap(cap1);
    }
}

/// Was `VideoSender::adapt` dem Encoder-Teil auftraegt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Adaptation {
    /// Neue Zielbitraten `(Stufe, kbit/s)`.
    pub bitrates: Vec<(u8, u32)>,
    /// Stufen, die sofort einen Keyframe brauchen (Zuschauer ist hierher gewechselt).
    pub key_layers: Vec<u8>,
}

/// Verteilt die Frames eines Streams an dessen Zuschauer.
#[derive(Clone)]
pub struct VideoSender {
    net: Net,
    stream_id: u8,
    /// Codecs, in denen dieser Stream kodiert wird.
    codec: VideoCodec,
    state: Arc<Mutex<SenderState>>,
}

impl VideoSender {
    pub fn new(net: Net, stream_id: u8, codec: VideoCodec) -> Self {
        Self {
            net,
            stream_id,
            codec,
            state: Arc::new(Mutex::new(SenderState {
                subs: HashMap::new(),
                frame_no: [0; MAX_LAYERS as usize],
                last_key_request: None,
                budget: Budget::default(),
                voice_peers: 0,
                layers: 1,
                rate: [
                    RateController::new(Budget::default().cap_kbps(0, 1)),
                    RateController::new(LAYER1_KBPS),
                ],
                reports: HashMap::new(),
            })),
        }
    }

    pub fn viewers(&self) -> Vec<PublicKey> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .subs
            .keys()
            .copied()
            .collect()
    }

    /// Alle Decoder-Faehigkeiten der Zuschauer (fuer die Codec-Wahl, M3-02).
    pub fn viewer_codecs(&self) -> Vec<Vec<VideoCodec>> {
        let st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.subs.values().map(|s| s.codecs.clone()).collect()
    }

    pub fn remove_viewer(&self, device: &PublicKey) {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.subs.remove(device);
        st.reports.remove(device);
        st.update_cap();
    }

    /// Upload und Zahl der Voice-Partner aendern (Einstellung, Messung, Voice-Beitritt).
    pub fn set_budget(&self, budget: Budget, voice_peers: u32) {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.budget = budget;
        st.voice_peers = voice_peers;
        st.update_cap();
    }

    /// Aktuelle Zielbitrate der Originalstufe fuer den Encoder (kbit/s).
    pub fn target_kbps(&self) -> u32 {
        self.layer_target_kbps(0)
    }

    pub fn layer_target_kbps(&self, layer: u8) -> u32 {
        let st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.rate[usize::from(layer.min(MAX_LAYERS - 1))].target_kbps()
    }

    /// Simulcast an- oder abschalten: `layers` = 1 (nur Original) oder 2 (zusaetzlich 720p30).
    /// Zuschauer auf einer nicht mehr angebotenen Stufe wechseln nach unten bzw. oben.
    /// Gibt die Stufen zurueck, die jetzt einen Keyframe brauchen.
    pub fn set_layers(&self, layers: u8) -> Vec<u8> {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.layers = layers.clamp(1, MAX_LAYERS);
        let top = st.layers - 1;
        let mut keys = Vec::new();
        for s in st.subs.values_mut() {
            if s.picker.layer() > top {
                s.picker.set_layer(top);
                s.policy.want_key();
                if !keys.contains(&top) {
                    keys.push(top);
                }
            }
        }
        st.update_cap();
        keys
    }

    pub fn layers(&self) -> u8 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .layers
    }

    /// Stufe eines Zuschauers (None, wenn er nicht abonniert hat).
    pub fn viewer_layer(&self, device: &PublicKey) -> Option<u8> {
        let st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.subs.get(device).map(|s| s.picker.layer())
    }

    /// Einmal pro Sekunde aufrufen: wertet die Berichte aus. Jeder Zuschauer wird, wenn noetig,
    /// auf eine andere Stufe gesetzt; die Bitrate jeder Stufe folgt ihren eigenen Zuschauern.
    pub fn adapt(&self) -> Adaptation {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let reports: Vec<(PublicKey, ReceiverReport)> = st.reports.drain().collect();
        let layers = st.layers;
        let mut per_layer: [Vec<ReceiverReport>; MAX_LAYERS as usize] = Default::default();
        let mut out = Adaptation::default();
        let mut moved = false;
        for (device, r) in reports {
            let Some(sub) = st.subs.get_mut(&device) else {
                continue;
            };
            per_layer[usize::from(sub.picker.layer().min(MAX_LAYERS - 1))].push(r);
            if let Some(new) = sub.picker.on_report(&r, layers) {
                sub.policy.want_key();
                if !out.key_layers.contains(&new) {
                    out.key_layers.push(new);
                }
                moved = true;
            }
        }
        if moved {
            st.update_cap();
        }
        for (layer, reports) in per_layer.iter().enumerate().take(usize::from(layers)) {
            if let Some(k) = st.rate[layer].on_reports(reports) {
                out.bitrates.push((layer as u8, k));
            }
        }
        out
    }

    /// Verarbeitet eine Video-Nachricht eines Zuschauers. `true` = sofort einen Keyframe erzeugen.
    /// Ist ein Abo nicht moeglich, wird `Refused` zurueckgeschickt (siehe `refuse`).
    pub fn on_message(&self, from: &PublicKey, msg: &VideoMsg) -> Result<bool, RefuseReason> {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match msg {
            VideoMsg::Subscribe {
                stream_id,
                layer,
                decoders,
            } if *stream_id == self.stream_id => {
                if !decoders.contains(&self.codec) {
                    return Err(RefuseReason::NoCommonCodec);
                }
                if !st.subs.contains_key(from)
                    && !st.budget.admit(st.voice_peers, st.subs.len() as u32)
                {
                    return Err(RefuseReason::Capacity);
                }
                let layer = (*layer).min(st.layers - 1);
                st.subs.insert(
                    *from,
                    Sub {
                        picker: LayerPicker::new(layer),
                        codecs: decoders.clone(),
                        policy: Policy::new(),
                        inflight: Arc::new(AtomicU32::new(0)),
                        failed: Arc::new(AtomicBool::new(false)),
                    },
                );
                st.last_key_request = Some(Instant::now());
                st.update_cap();
                Ok(true)
            }
            VideoMsg::Report(r) if r.stream_id == self.stream_id => {
                if st.subs.contains_key(from) {
                    st.reports.insert(*from, *r);
                }
                Ok(false)
            }
            VideoMsg::Subscribe { .. } => Err(RefuseReason::NoSuchStream),
            VideoMsg::Unsubscribe { stream_id } if *stream_id == self.stream_id => {
                st.subs.remove(from);
                st.reports.remove(from);
                st.update_cap();
                Ok(false)
            }
            VideoMsg::KeyframeRequest { stream_id } if *stream_id == self.stream_id => {
                if let Some(s) = st.subs.get_mut(from) {
                    s.policy.want_key();
                    let ok = st
                        .last_key_request
                        .is_none_or(|t| t.elapsed() >= KEY_REQUEST_GAP);
                    if ok {
                        st.last_key_request = Some(Instant::now());
                    }
                    return Ok(ok);
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    /// Sendet `frame` an alle Zuschauer dieser Stufe. Blockiert nie auf langsame Zuschauer.
    pub fn send(&self, frame: &OutFrame) -> SendOutcome {
        let mut out = SendOutcome::default();
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = usize::from(frame.layer.min(MAX_LAYERS - 1));
        let frame_no = st.frame_no[slot];
        st.frame_no[slot] = frame_no.wrapping_add(1);
        let header = FrameHeader {
            stream_id: self.stream_id,
            layer: frame.layer,
            flags: if frame.key { FLAG_KEY } else { 0 },
            codec: frame.codec,
            frame_no,
            ts: frame.ts,
            len: frame.data.len() as u32,
        };
        let mut wants_key = false;
        for (device, sub) in &mut st.subs {
            if sub.picker.layer() != frame.layer {
                continue;
            }
            if sub.failed.swap(false, Ordering::AcqRel) {
                sub.policy.want_key();
            }
            let inflight = sub.inflight.load(Ordering::Acquire);
            match sub.policy.decide(frame.key, inflight) {
                Decision::Skip => {
                    out.skipped += 1;
                    wants_key |= sub.policy.needs_key();
                }
                Decision::Send => {
                    out.sent += 1;
                    sub.inflight.fetch_add(1, Ordering::AcqRel);
                    let net = self.net.clone();
                    let device = *device;
                    let (inflight, failed) = (sub.inflight.clone(), sub.failed.clone());
                    let data = frame.data.clone();
                    tokio::spawn(async move {
                        let ok = tokio::time::timeout(
                            FRAME_DEADLINE,
                            send_one(&net, &device, header, data),
                        )
                        .await
                        .is_ok_and(|r| r.is_ok());
                        if !ok {
                            failed.store(true, Ordering::Release);
                        }
                        inflight.fetch_sub(1, Ordering::AcqRel);
                    });
                }
            }
        }
        if wants_key
            && st
                .last_key_request
                .is_none_or(|t| t.elapsed() >= KEY_REQUEST_GAP)
        {
            st.last_key_request = Some(Instant::now());
            out.want_key = true;
        }
        out
    }

    /// Kuendigt den Stream bei einem Zuschauer an oder beendet ihn.
    pub async fn notify(&self, device: &PublicKey, msg: VideoMsg) -> Result<(), NetError> {
        self.net
            .send_control(device, ControlMessage::Video(msg))
            .await
    }
}

/// Sendet einen Frame und wartet, bis der Empfaenger ihn vollstaendig bestaetigt hat.
/// Bei Fehler oder Abbruch (Zeitlimit des Aufrufers) wird der Stream zurueckgesetzt.
async fn send_one(
    net: &Net,
    device: &PublicKey,
    header: FrameHeader,
    data: Bytes,
) -> Result<(), NetError> {
    let mut s = net.open_video(device).await?;
    let res = async {
        s.write_all(&header.write())
            .await
            .map_err(|e| NetError::Send(e.to_string()))?;
        s.write_all(&data)
            .await
            .map_err(|e| NetError::Send(e.to_string()))?;
        s.finish().map_err(|e| NetError::Send(e.to_string()))?;
        s.stopped()
            .await
            .map_err(|e| NetError::Send(e.to_string()))?;
        Ok(())
    }
    .await;
    if res.is_err() {
        let _ = s.reset(0u32.into());
    }
    res
}

#[derive(Debug, thiserror::Error)]
pub enum VideoReadError {
    #[error("Stream-Fehler: {0}")]
    Io(String),
    #[error("Ungueltiger Frame-Kopf")]
    BadHeader,
    #[error("Zeitueberschreitung")]
    Timeout,
}

/// Schrittweite, in der der Frame-Puffer beim Lesen waechst.
const READ_STEP: usize = 256 * 1024;

/// Liest einen Frame von einem eingehenden Video-Stream.
pub async fn read_video_frame(
    recv: &mut RecvStream,
    timeout: Duration,
) -> Result<(FrameHeader, Vec<u8>), VideoReadError> {
    let work = async {
        let mut head = [0u8; HEADER_LEN];
        recv.read_exact(&mut head)
            .await
            .map_err(|e| VideoReadError::Io(e.to_string()))?;
        let h = FrameHeader::parse(&head).ok_or(VideoReadError::BadHeader)?;
        // Der Puffer waechst erst mit den angekommenen Daten: ein Kopf, der 8 MiB verspricht,
        // belegt nicht vorab 8 MiB (mehrere gleichzeitige Streams wuerden sich addieren).
        let len = h.len as usize;
        let mut body = Vec::with_capacity(len.min(READ_STEP));
        while body.len() < len {
            let start = body.len();
            body.resize((start + READ_STEP).min(len), 0);
            recv.read_exact(&mut body[start..])
                .await
                .map_err(|e| VideoReadError::Io(e.to_string()))?;
        }
        Ok((h, body))
    };
    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| VideoReadError::Timeout)?
}

/// Was der Empfaenger mit einem angekommenen Frame tun soll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    /// An den Decoder geben.
    Decode,
    /// Verwerfen (zu spaet oder Einstieg nur mit Keyframe moeglich).
    Drop,
    /// Verwerfen und einen Keyframe anfordern.
    DropAndRequestKey,
}

/// Misst einen Video-Stream beim Zuschauer.
#[derive(Debug)]
pub struct ReceiverTracker {
    stream_id: u8,
    layer: Option<u8>,
    next_no: Option<u32>,
    need_key: bool,
    window_start_ms: u64,
    received: u32,
    lost: u32,
    late: u32,
    bytes: u64,
    jitter: f64,
    last_transit: Option<f64>,
}

impl ReceiverTracker {
    pub fn new(stream_id: u8, now_ms: u64) -> Self {
        Self {
            stream_id,
            layer: None,
            next_no: None,
            need_key: true,
            window_start_ms: now_ms,
            received: 0,
            lost: 0,
            late: 0,
            bytes: 0,
            jitter: 0.0,
            last_transit: None,
        }
    }

    /// `now_ms`: lokale Uhr des Empfaengers (nur Differenzen zaehlen).
    pub fn on_frame(&mut self, h: &FrameHeader, now_ms: u64) -> Arrival {
        // Jitter nach RFC 3550: Schwankung von (Ankunft - Zeitstempel).
        let transit = now_ms as f64 - f64::from(h.ts);
        if let Some(prev) = self.last_transit {
            self.jitter += ((transit - prev).abs() - self.jitter) / 16.0;
        }
        self.last_transit = Some(transit);

        // Stufenwechsel: erst mit dem ersten Keyframe der neuen Stufe; Nachzuegler der alten
        // Stufe werden verworfen. Die Nummern der neuen Stufe beginnen unabhaengig.
        match self.layer {
            Some(l) if l != h.layer => {
                if !h.is_key() {
                    self.late += 1;
                    return Arrival::Drop;
                }
                self.layer = Some(h.layer);
                self.next_no = None;
            }
            None => self.layer = Some(h.layer),
            _ => {}
        }

        if let Some(next) = self.next_no {
            // Vergleich mit Umlauf: negativ = alt.
            let diff = h.frame_no.wrapping_sub(next) as i32;
            if diff < 0 {
                self.late += 1;
                return Arrival::Drop;
            }
            if diff > 0 {
                self.lost += diff as u32;
                self.need_key = true;
            }
        }
        self.next_no = Some(h.frame_no.wrapping_add(1));
        self.received += 1;
        self.bytes += u64::from(h.len);
        if h.is_key() {
            self.need_key = false;
            return Arrival::Decode;
        }
        if self.need_key {
            return Arrival::DropAndRequestKey;
        }
        Arrival::Decode
    }

    /// Ein Stream ist abgebrochen, ohne dass ein Frame ankam.
    pub fn on_aborted(&mut self) {
        self.lost += 1;
        self.need_key = true;
    }

    /// Schliesst das Messfenster ab und beginnt ein neues.
    pub fn report(&mut self, now_ms: u64) -> ReceiverReport {
        let interval = now_ms.saturating_sub(self.window_start_ms).max(1);
        let r = ReceiverReport {
            stream_id: self.stream_id,
            interval_ms: interval as u32,
            received: self.received,
            lost: self.lost,
            late: self.late,
            jitter_ms: self.jitter.round() as u32,
            kbps: (self.bytes * 8 / interval) as u32,
        };
        self.window_start_ms = now_ms;
        self.received = 0;
        self.lost = 0;
        self.late = 0;
        self.bytes = 0;
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(no: u32, key: bool, ts: u32, len: u32) -> FrameHeader {
        FrameHeader {
            stream_id: 1,
            layer: 0,
            flags: if key { FLAG_KEY } else { 0 },
            codec: VideoCodec::H264,
            frame_no: no,
            ts,
            len,
        }
    }

    #[test]
    fn policy_new_viewer_waits_for_key() {
        let mut p = Policy::new();
        assert_eq!(p.decide(false, 0), Decision::Skip);
        assert_eq!(p.decide(true, 0), Decision::Send);
        assert_eq!(p.decide(false, 0), Decision::Send);
    }

    #[test]
    fn policy_slow_viewer_is_cut_off_until_next_key() {
        let mut p = Policy::new();
        assert_eq!(p.decide(true, 0), Decision::Send);
        assert_eq!(p.decide(false, MAX_INFLIGHT), Decision::Skip);
        assert!(p.needs_key());
        // Auch wenn der Stau weg ist: erst ein Keyframe beendet die Sperre.
        assert_eq!(p.decide(false, 0), Decision::Skip);
        // Ein Keyframe bei vollem Stau wird ebenfalls ausgelassen.
        assert_eq!(p.decide(true, MAX_INFLIGHT), Decision::Skip);
        assert_eq!(p.decide(true, 1), Decision::Send);
        assert_eq!(p.decide(false, 1), Decision::Send);
    }

    #[test]
    fn tracker_drops_late_and_requests_key_after_gap() {
        let mut t = ReceiverTracker::new(1, 0);
        // Einstieg ohne Keyframe: verwerfen und Keyframe anfordern.
        assert_eq!(
            t.on_frame(&hdr(5, false, 0, 100), 0),
            Arrival::DropAndRequestKey
        );
        assert_eq!(t.on_frame(&hdr(6, true, 16, 100), 16), Arrival::Decode);
        assert_eq!(t.on_frame(&hdr(7, false, 33, 100), 33), Arrival::Decode);
        // Frame 8 fehlt.
        assert_eq!(
            t.on_frame(&hdr(9, false, 66, 100), 66),
            Arrival::DropAndRequestKey
        );
        // Nachzuegler.
        assert_eq!(t.on_frame(&hdr(8, false, 50, 100), 70), Arrival::Drop);
        assert_eq!(t.on_frame(&hdr(10, true, 83, 100), 83), Arrival::Decode);
        let r = t.report(1000);
        assert_eq!((r.lost, r.late), (1, 1));
        assert_eq!(r.interval_ms, 1000);
        assert_eq!(r.kbps, 5 * 100 * 8 / 1000);
        // Neues Fenster beginnt bei null.
        assert_eq!(t.report(2000).received, 0);
    }

    #[test]
    fn tracker_jitter_is_zero_for_steady_stream_and_grows_with_variation() {
        let mut steady = ReceiverTracker::new(1, 0);
        for i in 0..60u32 {
            steady.on_frame(&hdr(i, i == 0, i * 16, 10), u64::from(i * 16 + 40));
        }
        assert_eq!(steady.report(1000).jitter_ms, 0);
        let mut bumpy = ReceiverTracker::new(1, 0);
        for i in 0..60u32 {
            let extra = if i % 2 == 0 { 0 } else { 30 };
            bumpy.on_frame(&hdr(i, i == 0, i * 16, 10), u64::from(i * 16 + 40 + extra));
        }
        assert!(bumpy.report(1000).jitter_ms >= 10);
    }

    #[test]
    fn tracker_switches_layers_only_on_a_keyframe() {
        let mut t = ReceiverTracker::new(1, 0);
        let on = |layer, no, key| FrameHeader {
            layer,
            ..hdr(no, key, 0, 10)
        };
        assert_eq!(t.on_frame(&on(0, 0, true), 0), Arrival::Decode);
        assert_eq!(t.on_frame(&on(0, 1, false), 16), Arrival::Decode);
        // Erste Frames der neuen Stufe ohne Keyframe: verwerfen, nicht als Verlust zaehlen.
        assert_eq!(t.on_frame(&on(1, 0, false), 20), Arrival::Drop);
        assert_eq!(t.on_frame(&on(1, 7, true), 30), Arrival::Decode);
        assert_eq!(t.on_frame(&on(1, 8, false), 46), Arrival::Decode);
        // Nachzuegler der alten Stufe stoeren nicht.
        assert_eq!(t.on_frame(&on(0, 2, false), 50), Arrival::Drop);
        assert_eq!(t.on_frame(&on(1, 9, false), 62), Arrival::Decode);
        assert_eq!(t.report(1000).lost, 0);
    }

    #[test]
    fn tracker_handles_frame_number_wraparound() {
        let mut t = ReceiverTracker::new(1, 0);
        assert_eq!(t.on_frame(&hdr(u32::MAX, true, 0, 1), 0), Arrival::Decode);
        assert_eq!(t.on_frame(&hdr(0, false, 16, 1), 16), Arrival::Decode);
        assert_eq!(t.report(100).lost, 0);
    }
}

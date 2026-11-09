//! Dekodiertes Bild im NV12-Layout (M3-06): eine Y-Ebene und eine verschraenkte UV-Ebene.
//! Ohne ffmpeg nutzbar, damit die Anzeige auch in Builds ohne Decoder uebersetzt.

#[derive(Debug)]
pub struct YuvFrame {
    /// Laufende Nummer (fuer die Anzeige: neues Bild hochladen?).
    pub id: u64,
    /// Medienzeit des Senders in ms (gleiche Uhr wie der Stream-Ton).
    pub ts: u32,
    pub w: u32,
    pub h: u32,
    /// `w * h` Bytes.
    pub y: Vec<u8>,
    /// `w * h / 2` Bytes (U, V abwechselnd, halbe Aufloesung).
    pub uv: Vec<u8>,
}

/// So viele Bilder werden hoechstens vorgehalten (etwa eine Sekunde bei 60 fps).
const QUEUE_MAX: usize = 64;

#[derive(Debug, Default)]
struct SlotInner {
    queue: std::collections::VecDeque<std::sync::Arc<YuvFrame>>,
    shown: Option<std::sync::Arc<YuvFrame>>,
    /// Medienzeit des Tons, der gerade zu hoeren ist (None = kein Ton: neuestes Bild zeigen).
    audio_pos: Option<u32>,
}

/// Bilder eines Streams: der Decoder schreibt, die Anzeige liest. Laeuft Stream-Ton, zeigt die
/// Anzeige das Bild, dessen Zeitstempel zur Abspielposition des Tons passt (Lippensynchronitaet);
/// sonst das neueste.
#[derive(Debug, Default)]
pub struct FrameSlot(std::sync::Mutex<SlotInner>);

/// `a` liegt nicht nach `b` (mit Umlauf der 32-Bit-Zeit).
fn not_after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

impl FrameSlot {
    pub fn set(&self, f: YuvFrame) {
        if let Ok(mut g) = self.0.lock() {
            g.queue.push_back(std::sync::Arc::new(f));
            while g.queue.len() > QUEUE_MAX {
                g.queue.pop_front();
            }
        }
    }

    /// Abspielposition des Tons setzen (oder `None`, wenn keiner laeuft).
    pub fn set_audio_pos(&self, pos: Option<u32>) {
        if let Ok(mut g) = self.0.lock() {
            g.audio_pos = pos;
        }
    }

    /// Das Bild, das jetzt zu zeigen ist.
    pub fn get(&self) -> Option<std::sync::Arc<YuvFrame>> {
        let mut g = self.0.lock().ok()?;
        match g.audio_pos {
            None => {
                if let Some(last) = g.queue.pop_back() {
                    g.queue.clear();
                    g.shown = Some(last);
                }
            }
            Some(pos) => {
                // Alles, was nicht nach der Tonposition liegt, ist faellig; das neueste davon zaehlt.
                let mut due = None;
                while g.queue.front().is_some_and(|f| not_after(f.ts, pos)) {
                    due = g.queue.pop_front();
                }
                if due.is_some() {
                    g.shown = due;
                }
            }
        }
        g.shown.clone()
    }

    pub fn clear(&self) {
        if let Ok(mut g) = self.0.lock() {
            g.queue.clear();
            g.shown = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ts: u32) -> YuvFrame {
        YuvFrame {
            id: u64::from(ts),
            ts,
            w: 2,
            h: 2,
            y: vec![0; 4],
            uv: vec![0; 2],
        }
    }

    #[test]
    fn without_audio_the_newest_frame_is_shown() {
        let s = FrameSlot::default();
        assert!(s.get().is_none());
        for t in [0, 16, 33] {
            s.set(frame(t));
        }
        assert_eq!(s.get().unwrap().ts, 33);
    }

    #[test]
    fn with_audio_frames_wait_for_the_sound_and_stay_within_one_frame_of_it() {
        let s = FrameSlot::default();
        for i in 0..30u32 {
            s.set(frame(1000 + i * 16));
        }
        // Der Ton ist 100 ms hinter dem Bild: erst das Bild zu seiner Position zeigen.
        s.set_audio_pos(Some(1000));
        assert_eq!(s.get().unwrap().ts, 1000);
        let mut worst = 0u32;
        for pos in (1000..1400).step_by(5) {
            s.set_audio_pos(Some(pos));
            let shown = s.get().unwrap().ts;
            assert!(shown <= pos);
            worst = worst.max(pos - shown);
        }
        assert!(worst < 16, "Bild hinkt {worst} ms hinter dem Ton");
        // Ein Bild aus der Zukunft wird nicht vorzeitig gezeigt.
        s.set_audio_pos(Some(1100));
        let before = s.get().unwrap().ts;
        s.set(frame(5000));
        assert_eq!(s.get().unwrap().ts, before);
        // Ton weg: sofort das neueste.
        s.set_audio_pos(None);
        assert_eq!(s.get().unwrap().ts, 5000);
    }

    #[test]
    fn time_wraparound_is_handled() {
        assert!(not_after(u32::MAX - 5, 3));
        assert!(!not_after(3, u32::MAX - 5));
    }
}

//! Upload-Budget und Bitraten-Anpassung (M3-04). Reine Logik ohne Netz.
//!
//! * [`Budget`] teilt den Upload auf: Voice hat immer Vorrang, vom Rest bleibt ein Sicherheitsabstand
//!   frei, der Video-Anteil wird auf die Zuschauer verteilt. Ist nichts mehr uebrig, wird ein
//!   weiterer Zuschauer abgelehnt (`RefuseReason::Capacity`).
//! * [`RateController`] passt die Bitrate anhand der `ReceiverReport`s an: bei Verlust sofort
//!   herunter (auf das, was tatsaechlich ankam), bei sauberen Berichten langsam wieder hinauf.

use app_proto::video::ReceiverReport;

/// Bandbreite, die ein Voice-Partner braucht (Opus + Paket-Overhead), mit Reserve.
pub const VOICE_KBPS_PER_PEER: u32 = 64;
/// Kleinste sinnvolle Video-Bitrate je Zuschauer; darunter wird abgelehnt.
pub const MIN_VIDEO_KBPS: u32 = 1000;
/// Anteil des Uploads, der hoechstens verplant wird (Rest = Luft fuer Schwankungen).
pub const USABLE_PERCENT: u32 = 80;
/// Angenommener Upload, solange nichts gemessen oder eingetragen ist.
pub const DEFAULT_UPLOAD_KBPS: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Upload der Leitung in kbit/s (gemessen oder eingetragen).
    pub upload_kbps: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            upload_kbps: DEFAULT_UPLOAD_KBPS,
        }
    }
}

impl Budget {
    /// Was fuer Video bleibt, wenn `voice_peers` Gespraechspartner Voice bekommen.
    pub fn video_kbps(&self, voice_peers: u32) -> u32 {
        (self.upload_kbps / 100 * USABLE_PERCENT)
            .saturating_sub(voice_peers.saturating_mul(VOICE_KBPS_PER_PEER))
    }

    /// Bitratenobergrenze des Streams bei `viewers` Zuschauern (jeder bekommt den Stream einzeln).
    pub fn cap_kbps(&self, voice_peers: u32, viewers: u32) -> u32 {
        self.video_kbps(voice_peers) / viewers.max(1)
    }

    /// Darf ein weiterer Zuschauer dazukommen, ohne dass alle unter das Minimum fallen?
    pub fn admit(&self, voice_peers: u32, current_viewers: u32) -> bool {
        self.cap_kbps(voice_peers, current_viewers + 1) >= MIN_VIDEO_KBPS
    }
}

/// Verlustanteil eines Berichts in Prozent (verloren und zu spaete Frames).
fn bad_percent(r: &ReceiverReport) -> u32 {
    let total = r.received + r.lost + r.late;
    if total == 0 {
        return 0;
    }
    (r.lost + r.late) * 100 / total
}

/// Ab diesem Anteil verlorener/verspaeteter Frames gilt ein Bericht als schlecht.
const BAD_PERCENT: u32 = 3;
/// So viele saubere Berichte in Folge, bevor die Bitrate wieder steigt.
const CLEAN_BEFORE_RAISE: u32 = 3;

#[derive(Debug, Clone)]
pub struct RateController {
    cap: u32,
    min: u32,
    target: u32,
    clean: u32,
}

impl RateController {
    pub fn new(cap_kbps: u32) -> Self {
        let cap = cap_kbps.max(MIN_VIDEO_KBPS);
        Self {
            cap,
            min: MIN_VIDEO_KBPS.min(cap),
            target: cap,
            clean: 0,
        }
    }

    pub fn target_kbps(&self) -> u32 {
        self.target
    }

    /// Neue Obergrenze (Zuschauer kamen oder gingen, Budget geaendert).
    pub fn set_cap(&mut self, cap_kbps: u32) {
        self.cap = cap_kbps.max(MIN_VIDEO_KBPS);
        self.min = MIN_VIDEO_KBPS.min(self.cap);
        self.target = self.target.clamp(self.min, self.cap);
    }

    /// Wertet die neuesten Berichte aller Zuschauer aus (etwa einmal pro Sekunde).
    /// Gibt die neue Zielbitrate zurueck, wenn sie sich geaendert hat.
    pub fn on_reports(&mut self, reports: &[ReceiverReport]) -> Option<u32> {
        if reports.is_empty() {
            return None;
        }
        let before = self.target;
        let worst = reports
            .iter()
            .filter(|r| bad_percent(r) >= BAD_PERCENT)
            .map(|r| r.kbps)
            .min();
        if let Some(delivered) = worst {
            self.clean = 0;
            // Mehr als die Leitung schafft ist sinnlos: auf das Angekommene (mit Luft) gehen,
            // aber mindestens einen Schritt nach unten.
            let by_delivery = delivered / 100 * 90;
            let step = self.target / 100 * 85;
            self.target = by_delivery.min(step).clamp(self.min, self.cap);
        } else {
            self.clean += 1;
            if self.clean >= CLEAN_BEFORE_RAISE {
                self.clean = 0;
                self.target = (self.target + self.target / 12).clamp(self.min, self.cap);
            }
        }
        (self.target != before).then_some(self.target)
    }
}

/// Bitrate der zweiten Stufe (720p30) in kbit/s.
pub const LAYER1_KBPS: u32 = 2500;
/// So viele Berichte in Folge muessen schlecht sein, bevor ein Zuschauer eine Stufe tiefer geht.
const DOWN_AFTER_BAD: u32 = 2;
/// So viele saubere Berichte auf der unteren Stufe, bevor der erste Aufstiegsversuch kommt.
const UP_AFTER_CLEAN: u32 = 15;
/// Laengste Wartezeit (in Berichten) zwischen zwei Aufstiegsversuchen.
const UP_WAIT_MAX: u32 = 120;
/// Faellt ein Zuschauer so kurz nach einem Aufstieg wieder zurueck, war der Versuch erfolglos.
const UP_FAILED_WITHIN: u32 = 5;

/// Waehlt die Qualitaetsstufe eines einzelnen Zuschauers anhand seiner Berichte.
/// Stufe 0 ist die hoechste. Abstieg schnell, Aufstieg als vorsichtiger Versuch; schlaegt er fehl,
/// wird die Wartezeit bis zum naechsten Versuch verdoppelt.
#[derive(Debug, Clone)]
pub struct LayerPicker {
    layer: u8,
    bad: u32,
    clean: u32,
    wait: u32,
    since_up: Option<u32>,
}

impl LayerPicker {
    pub fn new(layer: u8) -> Self {
        Self {
            layer,
            bad: 0,
            clean: 0,
            wait: UP_AFTER_CLEAN,
            since_up: None,
        }
    }

    pub fn layer(&self) -> u8 {
        self.layer
    }

    /// Erzwingt eine Stufe (Abo, Stufe verschwindet).
    pub fn set_layer(&mut self, layer: u8) {
        self.layer = layer;
        self.bad = 0;
        self.clean = 0;
        self.since_up = None;
    }

    /// `layers`: Zahl der angebotenen Stufen. `Some(neu)`, wenn die Stufe wechselt.
    pub fn on_report(&mut self, r: &ReceiverReport, layers: u8) -> Option<u8> {
        if self.layer >= layers {
            self.set_layer(layers.saturating_sub(1));
            return Some(self.layer);
        }
        if let Some(n) = self.since_up.as_mut() {
            *n += 1;
        }
        if bad_percent(r) >= BAD_PERCENT {
            self.clean = 0;
            self.bad += 1;
            if self.bad >= DOWN_AFTER_BAD && self.layer + 1 < layers {
                if self.since_up.is_some_and(|n| n <= UP_FAILED_WITHIN) {
                    self.wait = (self.wait * 2).min(UP_WAIT_MAX);
                } else {
                    self.wait = UP_AFTER_CLEAN;
                }
                self.layer += 1;
                self.bad = 0;
                self.since_up = None;
                return Some(self.layer);
            }
            return None;
        }
        self.bad = 0;
        if self.layer > 0 {
            self.clean += 1;
            if self.clean >= self.wait {
                self.clean = 0;
                self.layer -= 1;
                self.since_up = Some(0);
                return Some(self.layer);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(received: u32, lost: u32, kbps: u32) -> ReceiverReport {
        ReceiverReport {
            stream_id: 1,
            interval_ms: 1000,
            received,
            lost,
            late: 0,
            jitter_ms: 0,
            kbps,
        }
    }

    /// Simulierte Leitung: bis `link_kbps` kommt alles an, darueber geht der Rest verloren.
    fn through(link_kbps: u32, sent_kbps: u32) -> ReceiverReport {
        if sent_kbps <= link_kbps {
            report(60, 0, sent_kbps)
        } else {
            let ok = 60 * link_kbps / sent_kbps;
            report(ok, 60 - ok, link_kbps)
        }
    }

    #[test]
    fn budget_gives_voice_priority_and_limits_viewers() {
        let b = Budget {
            upload_kbps: 10_000,
        };
        // 80 % von 10 Mbit = 8000, davon gehen 2 Voice-Partner mit je 64 ab.
        assert_eq!(b.video_kbps(2), 8000 - 128);
        assert_eq!(b.cap_kbps(2, 2), (8000 - 128) / 2);
        // Voice wird nie vom Video verdraengt: mehr Voice-Partner = weniger Video.
        assert!(b.video_kbps(10) < b.video_kbps(2));
        // Mit 8 Mbit Video passen 7 Zuschauer zu je >= 1 Mbit, der achte nicht.
        assert!(b.admit(0, 6));
        assert!(!b.admit(0, 8));
        // Ein winziger Upload nimmt keinen Zuschauer an.
        assert!(!Budget { upload_kbps: 1000 }.admit(0, 0));
    }

    #[test]
    fn halved_bandwidth_is_followed_within_three_seconds() {
        let mut c = RateController::new(12_000);
        let mut link = 12_000;
        // Eingeschwungen bei 6 Mbit/s: Zielbitrate wurde schon einmal gesenkt.
        link = link.min(6000);
        for _ in 0..10 {
            let r = through(link, c.target_kbps());
            c.on_reports(&[r]);
        }
        assert!(c.target_kbps() <= 6000);
        // Jetzt halbiert sich die Leitung.
        link = 3000;
        let mut seconds = 0;
        while c.target_kbps() > link {
            let r = through(link, c.target_kbps());
            c.on_reports(&[r]);
            seconds += 1;
            assert!(
                seconds <= 3,
                "nach {seconds} s noch {} kbit/s",
                c.target_kbps()
            );
        }
        // Und friert nicht ein: danach kommt wieder (fast) alles an.
        let r = through(link, c.target_kbps());
        assert!(r.lost * 100 / 60 < BAD_PERCENT);
    }

    #[test]
    fn recovers_slowly_and_respects_cap_and_floor() {
        let mut c = RateController::new(8000);
        // Dauerhaft schlecht: nie unter dem Minimum.
        for _ in 0..30 {
            c.on_reports(&[report(10, 50, 200)]);
        }
        assert_eq!(c.target_kbps(), MIN_VIDEO_KBPS);
        // Sauber: erst nach drei Berichten ein Schritt hinauf, nie ueber die Obergrenze.
        assert_eq!(c.on_reports(&[report(60, 0, 1000)]), None);
        assert_eq!(c.on_reports(&[report(60, 0, 1000)]), None);
        assert!(c.on_reports(&[report(60, 0, 1000)]).is_some());
        for _ in 0..300 {
            c.on_reports(&[report(60, 0, c.target_kbps())]);
        }
        assert_eq!(c.target_kbps(), 8000);
        // Neue Obergrenze (mehr Zuschauer) drueckt das Ziel sofort nach unten.
        c.set_cap(2500);
        assert_eq!(c.target_kbps(), 2500);
    }

    #[test]
    fn picker_drops_fast_and_climbs_back_carefully() {
        let good = report(60, 0, 2000);
        let bad = report(30, 30, 1000);
        let mut p = LayerPicker::new(0);
        // Ein einzelner schlechter Bericht genuegt nicht.
        assert_eq!(p.on_report(&bad, 2), None);
        assert_eq!(p.on_report(&good, 2), None);
        assert_eq!(p.on_report(&bad, 2), None);
        assert_eq!(p.on_report(&bad, 2), Some(1));
        // Unten ist Schluss.
        assert_eq!(p.on_report(&bad, 2), None);
        assert_eq!(p.on_report(&bad, 2), None);
        assert_eq!(p.layer(), 1);
        // Aufstieg erst nach UP_AFTER_CLEAN sauberen Berichten.
        for _ in 0..UP_AFTER_CLEAN - 1 {
            assert_eq!(p.on_report(&good, 2), None);
        }
        assert_eq!(p.on_report(&good, 2), Some(0));
        // Der Versuch scheitert sofort: Rueckfall, und der naechste Versuch dauert doppelt so lange.
        assert_eq!(p.on_report(&bad, 2), None);
        assert_eq!(p.on_report(&bad, 2), Some(1));
        for _ in 0..UP_AFTER_CLEAN * 2 - 1 {
            assert_eq!(p.on_report(&good, 2), None);
        }
        assert_eq!(p.on_report(&good, 2), Some(0));
        // Ein gelungener Aufstieg setzt die Wartezeit zurueck.
        for _ in 0..UP_FAILED_WITHIN + 1 {
            assert_eq!(p.on_report(&good, 2), None);
        }
        assert_eq!(p.on_report(&bad, 2), None);
        assert_eq!(p.on_report(&bad, 2), Some(1));
        for _ in 0..UP_AFTER_CLEAN - 1 {
            assert_eq!(p.on_report(&good, 2), None);
        }
        assert_eq!(p.on_report(&good, 2), Some(0));
    }

    #[test]
    fn picker_follows_a_vanishing_layer_and_never_moves_with_one_layer() {
        let bad = report(10, 50, 100);
        let mut p = LayerPicker::new(0);
        for _ in 0..10 {
            assert_eq!(p.on_report(&bad, 1), None);
        }
        let mut p = LayerPicker::new(1);
        assert_eq!(p.on_report(&report(60, 0, 1000), 1), Some(0));
        assert_eq!(p.layer(), 0);
    }

    #[test]
    fn worst_viewer_decides_and_no_reports_change_nothing() {
        let mut c = RateController::new(8000);
        assert_eq!(c.on_reports(&[]), None);
        let new = c
            .on_reports(&[report(60, 0, 8000), report(30, 30, 3000)])
            .unwrap();
        assert!(new <= 2700);
    }
}

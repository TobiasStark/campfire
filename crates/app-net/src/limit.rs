//! Mengengrenzen je Peer (N-3).
//!
//! Alle Netz-Ereignisse teilen sich einen Kanal. Damit ein Freund, der Nachrichten flutet, nicht
//! die Verarbeitung fuer alle verzoegert, bekommt jeder Peer und jede Art einen eigenen
//! Token-Bucket. Ist er leer, liest die Verbindung dieses Peers einfach langsamer (QUIC bremst
//! dann den Sender), Datagramme werden verworfen. Die Grenzen liegen weit ueber dem, was ein
//! normaler Client braucht.

use std::time::{Duration, Instant};

/// Kontrollnachrichten je Peer: dauerhaft und kurzzeitig.
pub(crate) const CONTROL: (f64, f64) = (100.0, 400.0);
/// Neue Sync- und Push-Streams je Peer (ein Push je veroeffentlichtem Event).
pub(crate) const SYNC_STREAMS: (f64, f64) = (20.0, 100.0);
/// Neue Video-Streams je Peer: ein Uni-Stream je Frame (60 fps, mehrere Ebenen); die Zahl
/// gleichzeitig offener Streams begrenzt QUIC zusaetzlich.
pub(crate) const VIDEO_STREAMS: (f64, f64) = (240.0, 480.0);
/// Datagramme je Peer (Voice: 50 Pakete/s je Sprecher).
pub(crate) const DATAGRAMS: (f64, f64) = (400.0, 800.0);

#[derive(Debug)]
pub(crate) struct Bucket {
    tokens: f64,
    rate: f64,
    burst: f64,
    last: Instant,
}

impl Bucket {
    pub(crate) fn new((rate, burst): (f64, f64)) -> Self {
        Self {
            tokens: burst,
            rate,
            burst,
            last: Instant::now(),
        }
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * self.rate).min(self.burst);
        self.last = now;
    }

    /// Nimmt ein Token, falls eines frei ist.
    pub(crate) fn try_take(&mut self) -> bool {
        self.refill(Instant::now());
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Wartezeit bis zum naechsten Token (0, wenn eines frei ist; nimmt es dann).
    fn reserve(&mut self) -> Duration {
        self.refill(Instant::now());
        self.tokens -= 1.0;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / self.rate)
        }
    }

    /// Wartet, bis ein Token frei ist. Bremst nur den Peer, dem der Bucket gehoert.
    pub(crate) async fn take(&mut self) {
        let wait = self.reserve();
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_allows_a_burst_then_the_rate() {
        let mut b = Bucket::new((10.0, 5.0));
        assert!((0..5).all(|_| b.try_take()));
        assert!(!b.try_take());
        // Reservieren ueber den Bestand hinaus ergibt eine Wartezeit von 1/rate je Token.
        let w = b.reserve();
        assert!(
            w > Duration::from_millis(50) && w <= Duration::from_millis(110),
            "{w:?}"
        );
        b.last -= Duration::from_secs(10);
        b.refill(Instant::now());
        assert!(b.tokens <= 5.0);
    }

    #[tokio::test]
    async fn take_waits_instead_of_failing() {
        let mut b = Bucket::new((100.0, 1.0));
        let t = Instant::now();
        b.take().await;
        b.take().await;
        b.take().await;
        assert!(t.elapsed() >= Duration::from_millis(15));
    }
}

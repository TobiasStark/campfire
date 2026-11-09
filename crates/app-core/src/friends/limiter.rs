//! Rate-Limit fuer `app/hello/1` (Token-Bucket pro Node-ID plus global).

use app_proto::PublicKey;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub per_node_burst: f64,
    pub per_node_refill: Duration,
    pub global_burst: f64,
    /// Neue Tokens pro Sekunde global.
    pub global_per_sec: f64,
    /// Obergrenze fuer gemerkte Node-IDs (Speicherschutz).
    pub max_tracked: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_node_burst: 3.0,
            per_node_refill: Duration::from_secs(10),
            global_burst: 20.0,
            global_per_sec: 5.0,
            max_tracked: 4096,
        }
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

pub struct RateLimiter {
    limits: Limits,
    nodes: HashMap<PublicKey, Bucket>,
    global: Bucket,
}

impl RateLimiter {
    pub fn new(limits: Limits, now: Instant) -> Self {
        Self {
            limits,
            nodes: HashMap::new(),
            global: Bucket {
                tokens: limits.global_burst,
                last: now,
            },
        }
    }

    fn refill(b: &mut Bucket, now: Instant, burst: f64, per_sec: f64) {
        let dt = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + dt * per_sec).min(burst);
        b.last = now;
    }

    /// `true`, wenn die Anfrage bearbeitet werden darf. Verbraucht je ein Token.
    pub fn allow(&mut self, node: &PublicKey, now: Instant) -> bool {
        let l = self.limits;
        Self::refill(&mut self.global, now, l.global_burst, l.global_per_sec);
        if self.global.tokens < 1.0 {
            return false;
        }
        if self.nodes.len() >= l.max_tracked && !self.nodes.contains_key(node) {
            // Voll: abgelaufene (wieder volle) Eintraege verwerfen, sonst ablehnen.
            let per_sec = 1.0 / l.per_node_refill.as_secs_f64();
            self.nodes.retain(|_, b| {
                let dt = now.saturating_duration_since(b.last).as_secs_f64();
                b.tokens + dt * per_sec < l.per_node_burst
            });
            if self.nodes.len() >= l.max_tracked {
                return false;
            }
        }
        let per_sec = 1.0 / l.per_node_refill.as_secs_f64();
        let b = self.nodes.entry(*node).or_insert(Bucket {
            tokens: l.per_node_burst,
            last: now,
        });
        Self::refill(b, now, l.per_node_burst, per_sec);
        if b.tokens < 1.0 {
            return false;
        }
        b.tokens -= 1.0;
        self.global.tokens -= 1.0;
        true
    }
}

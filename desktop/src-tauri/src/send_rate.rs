//! Silent ceiling on how often Kotodama sends messages to the providers' pages.
//!
//! Every send starts from a user action, but a burst (a script, a stuck key, a retry loop gone wrong) must never
//! turn into a flood towards a provider. The ceiling is far above any human pace: at most
//! `PER_PROVIDER_PER_MIN` sends a minute to the same provider and `TOTAL_PER_MIN` a minute overall. Past it a send
//! is not refused and nothing is shown: it is held until a slot frees up (see `kotodama_broadcast`).

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Sends a minute to one provider.
pub const PER_PROVIDER_PER_MIN: usize = 20;
/// Sends a minute to all providers together.
pub const TOTAL_PER_MIN: usize = 60;
const WINDOW: Duration = Duration::from_secs(60);

/// Booked send times (past and future), sorted, per provider and overall.
pub struct SendRate {
    per_cap: usize,
    all_cap: usize,
    per: HashMap<String, VecDeque<Instant>>,
    all: VecDeque<Instant>,
}

/// Inserts `t` keeping the queue sorted.
fn insert_sorted(q: &mut VecDeque<Instant>, t: Instant) {
    let at = q.partition_point(|x| *x <= t);
    q.insert(at, t);
}

/// Earliest instant at or after `now` that keeps every 60 s window of `q` within `cap` sends: the `cap`-th
/// most recent booking plus the window.
fn earliest(q: &VecDeque<Instant>, cap: usize, now: Instant) -> Instant {
    if cap == 0 || q.len() < cap {
        return now;
    }
    (q[q.len() - cap] + WINDOW).max(now)
}

impl SendRate {
    pub fn new(per_cap: usize, all_cap: usize) -> Self {
        SendRate { per_cap, all_cap, per: HashMap::new(), all: VecDeque::new() }
    }

    /// Books the next send to `key` at or after `now` and returns how long it has to wait (zero almost always).
    pub fn reserve(&mut self, key: &str, now: Instant) -> Duration {
        let old = now.checked_sub(WINDOW);
        let prune = |q: &mut VecDeque<Instant>| {
            if let Some(old) = old {
                while q.front().map(|t| *t <= old).unwrap_or(false) {
                    q.pop_front();
                }
            }
        };
        prune(&mut self.all);
        self.per.values_mut().for_each(|q| prune(q));
        self.per.retain(|_, q| !q.is_empty());
        let per = self.per.entry(key.to_string()).or_default();
        let at = earliest(per, self.per_cap, now).max(earliest(&self.all, self.all_cap, now));
        insert_sorted(per, at);
        insert_sorted(&mut self.all, at);
        at - now
    }
}

/// Process-wide booking for the sends to the providers (see `SendRate::reserve`).
pub fn reserve(key: &str) -> Duration {
    static R: OnceLock<Mutex<SendRate>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(SendRate::new(PER_PROVIDER_PER_MIN, TOTAL_PER_MIN)))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .reserve(key, Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Within the per-provider ceiling nothing waits; the next one waits for the oldest to leave the window, and
    /// another provider is not affected.
    #[test]
    fn per_provider_ceiling_holds_the_extra_send() {
        let t0 = Instant::now();
        let mut r = SendRate::new(20, 60);
        for _ in 0..20 {
            assert_eq!(r.reserve("qwen", t0), Duration::ZERO);
        }
        assert_eq!(r.reserve("qwen", t0), WINDOW);
        assert_eq!(r.reserve("openai", t0), Duration::ZERO);
        // Another one 30 s later: the next free slot is the one after the booking at t0 + 60 s.
        assert_eq!(r.reserve("qwen", t0 + Duration::from_secs(30)), Duration::from_secs(30));
    }

    /// The overall ceiling holds even when every provider is under its own.
    #[test]
    fn total_ceiling_holds_across_providers() {
        let t0 = Instant::now();
        let mut r = SendRate::new(20, 60);
        for i in 0..60 {
            assert_eq!(r.reserve(&format!("p{}", i % 4), t0), Duration::ZERO);
        }
        assert_eq!(r.reserve("p9", t0), WINDOW);
    }

    /// A human pace (one send every few seconds) never waits, and old bookings stop counting.
    #[test]
    fn human_pace_never_waits() {
        let t0 = Instant::now();
        let mut r = SendRate::new(20, 60);
        for i in 0..200u64 {
            assert_eq!(r.reserve("gemini", t0 + Duration::from_secs(i * 4)), Duration::ZERO);
        }
        assert!(r.all.len() <= 16);
    }
}

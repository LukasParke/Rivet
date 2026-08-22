//! In-memory fixed-window rate limiter (single-node), with bounded key space.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    window: Duration,
    limit: u32,
    /// Key -> (window reset instant, count).
    entries: Mutex<HashMap<String, (Instant, u32)>>,
    /// Hard cap on tracked keys to bound memory under key-flooding.
    max_keys: usize,
}

impl RateLimiter {
    pub fn new(limit: u32, window: Duration) -> Self {
        Self {
            window,
            limit,
            entries: Mutex::new(HashMap::new()),
            max_keys: 100_000,
        }
    }

    /// Returns true if the action is allowed for `key` at time `now`.
    pub fn allow(&self, key: &str, now: Instant) -> bool {
        let mut map = self.entries.lock().expect("rate limiter mutex");
        match map.get_mut(key) {
            Some((reset_at, count)) if now < *reset_at => {
                if *count >= self.limit {
                    false
                } else {
                    *count += 1;
                    true
                }
            }
            _ => {
                // Evict expired entries when the map grows past the cap.
                if map.len() >= self.max_keys {
                    map.retain(|_, (reset_at, _)| now < *reset_at);
                }
                map.insert(key.to_string(), (now + self.window, 1));
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_limit_then_resets() {
        let rl = RateLimiter::new(3, Duration::from_millis(50));
        let t0 = Instant::now();
        assert!(rl.allow("ip1", t0));
        assert!(rl.allow("ip1", t0));
        assert!(rl.allow("ip1", t0));
        assert!(
            !rl.allow("ip1", t0),
            "4th attempt in window must be blocked"
        );
        // Other keys unaffected.
        assert!(rl.allow("ip2", t0));
        // After the window, allowed again.
        let later = t0 + Duration::from_millis(60);
        assert!(rl.allow("ip1", later));
    }
}

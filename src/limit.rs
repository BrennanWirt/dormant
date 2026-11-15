//! Wake rate limits. On a public port anyone can send a login and so run `start_cmd`; these limits
//! cap how often that can happen: per source address and for the server as a whole.
//!
//! Only wakes that are let through are counted (a start, a resume from freeze, or a wake queued while
//! the server is stopping). Logins that arrive while the server is already starting or up cost nothing.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Denied {
    /// This source address has used up `wake_limit_per_ip`.
    Source,
    /// The server as a whole has used up `wake_limit_global`.
    Global,
}

/// Sliding-window limiter: at most `per_ip` wakes per source and `global` wakes in total within
/// `window`. A limit of 0 switches that limit off.
pub struct WakeLimiter {
    per_ip: usize,
    global: usize,
    window: Duration,
    seen: Mutex<Seen>,
}

#[derive(Default)]
struct Seen {
    all: VecDeque<Instant>,
    by_source: HashMap<u128, VecDeque<Instant>>,
}

/// One key per IPv4 address and per IPv6 /64 (a single host usually owns a whole /64).
/// IPv4-mapped IPv6 addresses count as the IPv4 address they carry.
fn source_key(ip: IpAddr) -> u128 {
    match ip.to_canonical() {
        IpAddr::V4(a) => u128::from(u32::from(a)),
        IpAddr::V6(a) => u128::from(a) | 0xffff_ffff_ffff_ffff,
    }
}

fn expire(q: &mut VecDeque<Instant>, now: Instant, window: Duration) {
    while q.front().is_some_and(|t| now.saturating_duration_since(*t) >= window) {
        q.pop_front();
    }
}

impl WakeLimiter {
    pub fn new(per_ip: usize, global: usize, window_secs: u64) -> Self {
        WakeLimiter { per_ip, global, window: Duration::from_secs(window_secs), seen: Mutex::default() }
    }

    /// May `ip` wake the server at `now`? Records the wake when the answer is yes.
    pub fn check(&self, ip: IpAddr, now: Instant) -> Result<(), Denied> {
        let mut seen = self.seen.lock().unwrap();
        let Seen { all, by_source } = &mut *seen;
        expire(all, now, self.window);
        by_source.retain(|_, q| {
            expire(q, now, self.window);
            !q.is_empty()
        });
        if self.global > 0 && all.len() >= self.global {
            return Err(Denied::Global);
        }
        let key = source_key(ip);
        if self.per_ip > 0 && by_source.get(&key).is_some_and(|q| q.len() >= self.per_ip) {
            return Err(Denied::Source);
        }
        // Memory stays small: an entry is added only for a wake that was let through, and entries
        // older than the window are dropped on every call.
        if self.global > 0 {
            all.push_back(now);
        }
        if self.per_ip > 0 {
            by_source.entry(key).or_default().push_back(now);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn per_source_limit_and_window() {
        let l = WakeLimiter::new(2, 0, 60);
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        assert_eq!(l.check(ip("203.0.113.1"), at(0)), Ok(()));
        assert_eq!(l.check(ip("203.0.113.1"), at(10)), Ok(()));
        assert_eq!(l.check(ip("203.0.113.1"), at(20)), Err(Denied::Source));
        // another address is not affected
        assert_eq!(l.check(ip("203.0.113.2"), at(20)), Ok(()));
        // a refused attempt is not counted, so the first slot frees up exactly one window after it was used
        assert_eq!(l.check(ip("203.0.113.1"), at(59)), Err(Denied::Source));
        assert_eq!(l.check(ip("203.0.113.1"), at(60)), Ok(()));
        assert_eq!(l.check(ip("203.0.113.1"), at(61)), Err(Denied::Source));
        assert_eq!(l.check(ip("203.0.113.1"), at(70)), Ok(()));
    }

    #[test]
    fn global_limit_counts_every_source() {
        let l = WakeLimiter::new(1, 3, 60);
        let t0 = Instant::now();
        for i in 1..=3 {
            assert_eq!(l.check(ip(&format!("198.51.100.{i}")), t0), Ok(()));
        }
        assert_eq!(l.check(ip("198.51.100.4"), t0), Err(Denied::Global));
        // the global limit is reported first, also for a source that is over its own limit
        assert_eq!(l.check(ip("198.51.100.1"), t0), Err(Denied::Global));
        assert_eq!(l.check(ip("198.51.100.4"), t0 + Duration::from_secs(60)), Ok(()));
    }

    #[test]
    fn zero_switches_a_limit_off() {
        let l = WakeLimiter::new(0, 0, 60);
        let t0 = Instant::now();
        for _ in 0..1000 {
            assert_eq!(l.check(ip("203.0.113.1"), t0), Ok(()));
        }
        assert!(l.seen.lock().unwrap().by_source.is_empty()); // nothing is stored when nothing is limited
    }

    #[test]
    fn ipv6_is_limited_per_slash_64_and_mapped_ipv4_as_ipv4() {
        let l = WakeLimiter::new(1, 0, 60);
        let t0 = Instant::now();
        assert_eq!(l.check(ip("2001:db8:1:2::1"), t0), Ok(()));
        assert_eq!(l.check(ip("2001:db8:1:2:ffff::9"), t0), Err(Denied::Source));
        assert_eq!(l.check(ip("2001:db8:1:3::1"), t0), Ok(()));
        assert_eq!(l.check(ip("192.0.2.7"), t0), Ok(()));
        assert_eq!(l.check(ip("::ffff:192.0.2.7"), t0), Err(Denied::Source));
    }

    #[test]
    fn old_entries_are_dropped() {
        let l = WakeLimiter::new(1, 0, 60);
        let t0 = Instant::now();
        for i in 0..50u8 {
            assert_eq!(l.check(IpAddr::from([10, 0, 0, i]), t0), Ok(()));
        }
        assert_eq!(l.seen.lock().unwrap().by_source.len(), 50);
        assert_eq!(l.check(ip("10.0.1.1"), t0 + Duration::from_secs(61)), Ok(()));
        assert_eq!(l.seen.lock().unwrap().by_source.len(), 1);
    }
}

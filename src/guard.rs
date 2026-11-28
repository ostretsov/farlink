//! Brakes on password guessing at the relay (`/api/login`).
//!
//! Failed sign-ins are counted per client address and per sign-in name in a
//! sliding window. Once either has too many, further attempts are refused
//! without even checking the password, until old failures fall out of the
//! window. Pure logic: the caller passes the time.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;

/// Failures counted over this many seconds.
pub const WINDOW_SECS: u64 = 15 * 60;
/// Failed attempts one address may make per window.
pub const PER_ADDRESS: usize = 10;
/// Failed attempts on one sign-in name per window, from anywhere (slows
/// guessing spread over many addresses).
pub const PER_LOGIN: usize = 30;
/// Most keys remembered; the oldest are forgotten first.
const MAX_KEYS: usize = 50_000;

#[derive(Default)]
pub struct LoginGuard {
    failures: HashMap<String, VecDeque<u64>>,
}

impl LoginGuard {
    /// Seconds to wait before this address may try this name again, or
    /// `None` when the attempt may go ahead.
    pub fn blocked(&mut self, addr: &str, login: &str, now: u64) -> Option<u64> {
        let a = self.wait(&addr_key(addr), PER_ADDRESS, now);
        let l = self.wait(&login_key(login), PER_LOGIN, now);
        a.max(l)
    }

    /// Remember a wrong password.
    pub fn failed(&mut self, addr: &str, login: &str, now: u64) {
        if self.failures.len() >= MAX_KEYS {
            self.forget_old(now);
        }
        for key in [addr_key(addr), login_key(login)] {
            self.failures.entry(key).or_default().push_back(now);
        }
    }

    /// A right password clears that address's record (not the name's: an
    /// attacker signing in to their own account must not reset the count
    /// on someone else's name).
    pub fn succeeded(&mut self, addr: &str) {
        self.failures.remove(&addr_key(addr));
    }

    fn wait(&mut self, key: &str, limit: usize, now: u64) -> Option<u64> {
        let q = self.failures.get_mut(key)?;
        while q.front().is_some_and(|&t| t + WINDOW_SECS <= now) {
            q.pop_front();
        }
        if q.is_empty() {
            self.failures.remove(key);
            return None;
        }
        (q.len() >= limit).then(|| q[q.len() - limit] + WINDOW_SECS - now)
    }

    fn forget_old(&mut self, now: u64) {
        self.failures
            .retain(|_, q| q.back().is_some_and(|&t| t + WINDOW_SECS > now));
        if self.failures.len() >= MAX_KEYS {
            self.failures.clear();
        }
    }
}

fn addr_key(addr: &str) -> String {
    format!("a:{addr}")
}

fn login_key(login: &str) -> String {
    let l = login.trim().to_lowercase();
    format!("l:{}", if l.is_empty() { "admin" } else { &l })
}

/// The address used for counting: IPv6 clients are grouped by their /64
/// network (one machine usually has a whole /64 to pick addresses from).
pub fn client_key(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_is_blocked_after_too_many_failures_then_released() {
        let mut g = LoginGuard::default();
        for i in 0..PER_ADDRESS as u64 {
            assert_eq!(g.blocked("1.2.3.4", "anna", 100 + i), None);
            g.failed("1.2.3.4", "anna", 100 + i);
        }
        let wait = g.blocked("1.2.3.4", "anna", 120).expect("blocked");
        assert!(wait > 0 && wait <= WINDOW_SECS);
        assert!(
            g.blocked("1.2.3.4", "bob", 120).is_some(),
            "the address is blocked for every name"
        );
        assert_eq!(
            g.blocked("5.6.7.8", "bob", 120),
            None,
            "other addresses are not"
        );
        assert_eq!(
            g.blocked("1.2.3.4", "anna", 100 + WINDOW_SECS + 1),
            None,
            "old failures expire"
        );
    }

    #[test]
    fn one_name_guessed_from_many_addresses_is_slowed_down() {
        let mut g = LoginGuard::default();
        for i in 0..PER_LOGIN {
            g.failed(&format!("10.0.0.{i}"), "", 50);
        }
        assert!(
            g.blocked("10.9.9.9", "admin", 60).is_some(),
            "empty name = admin"
        );
        assert_eq!(g.blocked("10.9.9.9", "anna", 60), None);
    }

    #[test]
    fn success_clears_the_address_but_not_the_name() {
        let mut g = LoginGuard::default();
        for _ in 0..PER_ADDRESS {
            g.failed("1.1.1.1", "x", 10);
        }
        g.succeeded("1.1.1.1");
        assert_eq!(g.blocked("1.1.1.1", "y", 11), None);
    }

    #[test]
    fn ipv6_grouped_by_network() {
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        assert_eq!(client_key(a), client_key(b));
        assert_eq!(client_key("::ffff:1.2.3.4".parse().unwrap()), "1.2.3.4");
    }
}

//! Per-client request limits. Request rates are in-memory token buckets; registration
//! quotas live in SQLite (see `Store::register_from`) so a restart cannot reset them.
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
pub struct Rate {
    pub burst: f64,
    pub per_sec: f64,
}

/// Registration caps over rolling windows. Counted per client and across all clients,
/// so a botnet spread over many addresses still hits the global ceiling.
#[derive(Clone, Copy, Debug)]
pub struct Quota {
    pub per_client_hour: i64,
    pub per_client_day: i64,
    pub global_hour: i64,
}
impl Quota {
    pub const NONE: Self = Self {
        per_client_hour: i64::MAX,
        per_client_day: i64::MAX,
        global_hour: i64::MAX,
    };
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Every `/api/*` request.
    pub api: Rate,
    /// `/api/auth` login and registration attempts, on top of `api`. Registration is
    /// further bounded by `register`.
    pub auth: Rate,
    /// WebSocket handshakes.
    pub ws: Rate,
    /// Concurrently open WebSockets.
    pub ws_per_client: usize,
    pub register: Quota,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            // 50 requests/s sustained per IP, with a burst for page loads and rapid
            // trading; only scripted floods notice it.
            api: Rate {
                burst: 300.0,
                per_sec: 50.0,
            },
            // Login brute force guard; registration has its own hard quota below.
            auth: Rate {
                burst: 30.0,
                per_sec: 1.0,
            },
            ws: Rate {
                burst: 30.0,
                per_sec: 5.0,
            },
            ws_per_client: 100,
            register: Quota {
                per_client_hour: 2,
                per_client_day: 5,
                global_hour: 30,
            },
        }
    }
}
impl Limits {
    /// No effective limits, for offline fixtures and tests that exercise other behavior.
    pub fn unlimited() -> Self {
        let free = Rate {
            burst: 1e9,
            per_sec: 1e9,
        };
        Self {
            api: free,
            auth: free,
            ws: free,
            ws_per_client: usize::MAX,
            register: Quota::NONE,
        }
    }
}

/// An address or CIDR block, e.g. `172.18.0.2` or `172.16.0.0/12`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Net {
    addr: IpAddr,
    prefix: u8,
}
impl std::str::FromStr for Net {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (ip, prefix) = s.split_once('/').unwrap_or((s, ""));
        let addr = ip
            .trim()
            .parse::<IpAddr>()
            .map_err(|_| format!("无效地址: {s}"))?
            .to_canonical();
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = if prefix.is_empty() {
            max
        } else {
            prefix
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("无效网段: {s}"))?
        };
        Ok(Self { addr, prefix })
    }
}
impl Net {
    pub fn contains(&self, ip: IpAddr) -> bool {
        let bits = |ip: IpAddr| match ip.to_canonical() {
            IpAddr::V4(v4) => (u32::from(v4) as u128, 32),
            IpAddr::V6(v6) => (u128::from(v6), 128),
        };
        let ((net, width), (host, host_width)) = (bits(self.addr), bits(ip));
        if width != host_width {
            return false;
        }
        let shift = width - self.prefix as u32;
        shift >= width || (net >> shift) == (host >> shift)
    }
}

/// Clients are keyed by IPv4 address or IPv6 /64, the smallest block one user usually
/// controls; rotating addresses inside a /64 does not buy a fresh quota.
pub fn client_key(ip: Option<IpAddr>) -> String {
    match ip {
        None => "unknown".into(),
        Some(IpAddr::V4(v4)) => v4.to_string(),
        Some(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
    }
}

type Buckets = HashMap<(String, &'static str), (f64, Instant)>;

#[derive(Default)]
pub struct Limiter {
    buckets: Mutex<Buckets>,
    sockets: Arc<Mutex<HashMap<String, usize>>>,
}
impl Limiter {
    /// Take one token, or return the whole seconds until one is available.
    pub fn check(
        &self,
        client: &str,
        kind: &'static str,
        rate: Rate,
        now: Instant,
    ) -> Result<(), u64> {
        let mut map = self.buckets.lock().unwrap();
        if map.len() > 50_000 {
            // Every configured bucket refills within ten minutes, so idle ones equal fresh ones.
            map.retain(|_, (_, at)| now.duration_since(*at) < Duration::from_secs(600));
        }
        let entry = map
            .entry((client.to_owned(), kind))
            .or_insert((rate.burst, now));
        let tokens =
            (entry.0 + now.duration_since(entry.1).as_secs_f64() * rate.per_sec).min(rate.burst);
        if tokens >= 1.0 {
            *entry = (tokens - 1.0, now);
            Ok(())
        } else {
            *entry = (tokens, now);
            Err(((1.0 - tokens) / rate.per_sec).ceil().max(1.0) as u64)
        }
    }
    /// Reserve one open-socket slot for the client; released when the slot drops.
    pub fn socket(&self, client: &str, max: usize) -> Option<SocketSlot> {
        let mut open = self.sockets.lock().unwrap();
        let count = open.entry(client.to_owned()).or_default();
        if *count >= max {
            return None;
        }
        *count += 1;
        Some(SocketSlot {
            client: client.to_owned(),
            open: self.sockets.clone(),
        })
    }
}

pub struct SocketSlot {
    client: String,
    open: Arc<Mutex<HashMap<String, usize>>>,
}
impl Drop for SocketSlot {
    fn drop(&mut self) {
        let mut open = self.open.lock().unwrap();
        if let Some(count) = open.get_mut(&self.client) {
            *count -= 1;
            if *count == 0 {
                open.remove(&self.client);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bucket_allows_burst_then_refills_at_rate() {
        let limiter = Limiter::default();
        let rate = Rate {
            burst: 3.0,
            per_sec: 0.5,
        };
        let t = Instant::now();
        for _ in 0..3 {
            assert!(limiter.check("a", "x", rate, t).is_ok());
        }
        assert_eq!(limiter.check("a", "x", rate, t), Err(2));
        assert!(
            limiter.check("b", "x", rate, t).is_ok(),
            "clients are independent"
        );
        assert!(
            limiter.check("a", "y", rate, t).is_ok(),
            "kinds are independent"
        );
        assert!(
            limiter
                .check("a", "x", rate, t + Duration::from_secs(2))
                .is_ok()
        );
        assert!(
            limiter
                .check("a", "x", rate, t + Duration::from_secs(2))
                .is_err()
        );
    }
    #[test]
    fn ipv6_is_keyed_by_64_and_mapped_v4_by_address() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(client_key(Some(a)), client_key(Some(b)));
        assert_ne!(client_key(Some(a)), client_key(Some(c)));
        let mapped: IpAddr = "::ffff:203.0.113.7".parse().unwrap();
        assert_eq!(client_key(Some(mapped)), "203.0.113.7");
    }
    #[test]
    fn trusted_networks_match_addresses_and_cidrs() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let net = |s: &str| s.parse::<Net>().unwrap();
        assert!(net("172.16.0.0/12").contains(ip("172.18.0.5")));
        assert!(!net("172.16.0.0/12").contains(ip("172.32.0.1")));
        assert!(net("172.18.0.2").contains(ip("::ffff:172.18.0.2")));
        assert!(!net("172.18.0.2").contains(ip("172.18.0.3")));
        assert!(net("0.0.0.0/0").contains(ip("8.8.8.8")));
        assert!(!net("0.0.0.0/0").contains(ip("2001:db8::1")));
        assert!(net("fd00::/8").contains(ip("fd12::1")));
        for bad in ["172.16.0.0/33", "nope", "10.0.0.0/x"] {
            assert!(bad.parse::<Net>().is_err(), "{bad}");
        }
    }
    #[test]
    fn socket_slots_cap_and_release() {
        let limiter = Limiter::default();
        let one = limiter.socket("a", 2).unwrap();
        let _two = limiter.socket("a", 2).unwrap();
        assert!(limiter.socket("a", 2).is_none());
        assert!(limiter.socket("b", 2).is_some());
        drop(one);
        assert!(limiter.socket("a", 2).is_some());
    }
}

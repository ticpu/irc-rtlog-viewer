//! Per-client budgets for AI ask sessions and the plain-text search.
//!
//! A client is its IP address: the TCP peer, or the X-Real-IP header when the
//! peer is listed in `trusted_proxies`. IPv6 clients are keyed by their /64,
//! since one host routinely holds a whole prefix and could rotate through it;
//! an IPv4-mapped address counts as the IPv4 address.
//!
//! The ask budget counts Anthropic API requests and is charged to both the
//! address and an `ask_id` cookie handed out by the ask page. Dropping the
//! cookie gets a fresh one, so the cookie budget only shares an address's
//! allowance between its users; the address budget is the one that holds.
//!
//! Budgets are governor token buckets: a full bucket of N per day refills one
//! token every 24h/N, a rolling allowance rather than a midnight reset. State
//! is in memory and resets on restart; `prune` drops buckets that are full
//! again, which keeps the maps bounded by recent clients.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::http::{HeaderMap, header};
use governor::clock::{Clock, DefaultClock};
use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};
use tokio::sync::Semaphore;

use crate::Config;

/// Name of the cookie carrying the ask client id.
pub const ASK_COOKIE: &str = "ask_id";

const DAY: Duration = Duration::from_secs(86_400);

/// Why a request was refused, with the wait until the next token.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Client(Duration),
    Address(Duration),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (who, wait) = match self {
            Refusal::Client(wait) => ("this browser", wait),
            Refusal::Address(wait) => ("your address", wait),
        };
        let minutes = wait
            .as_secs()
            .div_ceil(60)
            .max(1);
        write!(
            f,
            "AI search budget used up for {who}, next request allowed in {minutes} min"
        )
    }
}

struct AskBudget {
    per_client: DefaultKeyedRateLimiter<String>,
    per_address: DefaultKeyedRateLimiter<IpAddr>,
}

pub struct Limits {
    ask: Option<AskBudget>,
    search_per_address: DefaultKeyedRateLimiter<IpAddr>,
    /// Bounds concurrent plain-text searches.
    pub search_slots: Arc<Semaphore>,
    trusted_proxies: Vec<IpAddr>,
}

fn per_day(n: NonZeroU32) -> Quota {
    Quota::with_period(DAY / n.get())
        .expect("a day divided by a u32 is never zero")
        .allow_burst(n)
}

impl Limits {
    pub fn new(config: &Config) -> Self {
        let ask = config
            .ai
            .as_ref()
            .map(|ai| AskBudget {
                per_client: RateLimiter::keyed(per_day(ai.requests_per_day_per_client)),
                per_address: RateLimiter::keyed(per_day(ai.requests_per_day_per_ip)),
            });
        Self {
            ask,
            search_per_address: RateLimiter::keyed(Quota::per_minute(
                config
                    .search
                    .per_ip_per_minute,
            )),
            search_slots: Arc::new(Semaphore::new(
                config
                    .search
                    .max_concurrent
                    .get(),
            )),
            trusted_proxies: config
                .trusted_proxies
                .iter()
                .map(IpAddr::to_canonical)
                .collect(),
        }
    }

    /// Takes one Anthropic request from the client's and the address's budget.
    ///
    /// The client is charged first, so a refusal on the address costs the
    /// client one token. Always succeeds when AI is not configured.
    pub fn charge_ask(&self, client: &str, addr: IpAddr) -> Result<(), Refusal> {
        let Some(ask) = &self.ask else {
            return Ok(());
        };
        let now = DefaultClock::default().now();
        ask.per_client
            .check_key(&client.to_owned())
            .map_err(|e| Refusal::Client(e.wait_time_from(now)))?;
        ask.per_address
            .check_key(&bucket_key(addr))
            .map_err(|e| Refusal::Address(e.wait_time_from(now)))
    }

    /// Takes one search from the address's budget, or returns the wait.
    pub fn charge_search(&self, addr: IpAddr) -> Result<(), Duration> {
        let now = DefaultClock::default().now();
        self.search_per_address
            .check_key(&bucket_key(addr))
            .map_err(|e| e.wait_time_from(now))
    }

    /// Drops buckets that have refilled completely.
    pub fn prune(&self) {
        if let Some(ask) = &self.ask {
            ask.per_client
                .retain_recent();
            ask.per_client
                .shrink_to_fit();
            ask.per_address
                .retain_recent();
            ask.per_address
                .shrink_to_fit();
        }
        self.search_per_address
            .retain_recent();
        self.search_per_address
            .shrink_to_fit();
    }

    /// The client address: X-Real-IP when the peer is a trusted proxy, else the peer.
    pub fn client_addr(&self, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
        let peer = peer
            .ip()
            .to_canonical();
        if !self
            .trusted_proxies
            .contains(&peer)
        {
            return peer;
        }
        let Some(value) = headers.get("x-real-ip") else {
            return peer;
        };
        match value
            .to_str()
            .ok()
            .and_then(|v| {
                v.trim()
                    .parse::<IpAddr>()
                    .ok()
            }) {
            Some(addr) => addr.to_canonical(),
            None => {
                static WARNED: AtomicBool = AtomicBool::new(false);
                if !WARNED.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "limits: X-Real-IP from trusted proxy {peer} is not an IP address, \
                         keying its requests by the proxy (reported once)"
                    );
                }
                peer
            }
        }
    }
}

/// The bucket an address is charged to: IPv6 by /64, IPv4 as is.
fn bucket_key(addr: IpAddr) -> IpAddr {
    match addr.to_canonical() {
        IpAddr::V6(v6) => {
            let prefix = u128::from(v6) & !((1u128 << 64) - 1);
            IpAddr::V6(Ipv6Addr::from(prefix))
        }
        v4 => v4,
    }
}

/// A fresh random ask client id, 32 hex digits.
pub fn new_client_id() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)?;
    Ok(bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The ask client id from the request's cookies, if present and well formed.
pub fn client_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| {
            v.to_str()
                .ok()
        })
        .flat_map(cookie::Cookie::split_parse)
        .filter_map(Result::ok)
        .find(|c| c.name() == ASK_COOKIE)
        .map(|c| {
            c.value()
                .to_owned()
        })
        .filter(|v| {
            v.len() == 32
                && v.bytes()
                    .all(|b| b.is_ascii_hexdigit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(yaml: &str) -> Limits {
        Limits::new(&serde_yaml::from_str(yaml).unwrap())
    }

    fn ip(addr: &str) -> IpAddr {
        addr.parse()
            .unwrap()
    }

    fn peer(addr: &str) -> SocketAddr {
        SocketAddr::new(ip(addr), 1234)
    }

    fn real_ip(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-real-ip",
            value
                .parse()
                .unwrap(),
        );
        headers
    }

    #[test]
    fn real_ip_is_trusted_only_from_listed_proxies() {
        let l = limits("logs_dirs: []");
        let h = real_ip("2001:db8::7");
        let expected = ip("2001:db8::7");
        assert_eq!(l.client_addr(peer("127.0.0.1"), &h), expected);
        assert_eq!(l.client_addr(peer("::1"), &h), expected);
        assert_eq!(l.client_addr(peer("::ffff:127.0.0.1"), &h), expected);
        let stranger = ip("192.0.2.9");
        assert_eq!(l.client_addr(peer("192.0.2.9"), &h), stranger);
        assert_eq!(
            l.client_addr(peer("127.0.0.1"), &real_ip("garbage")),
            ip("127.0.0.1")
        );
    }

    #[test]
    fn ipv6_addresses_share_their_64_bucket() {
        let l = limits("logs_dirs: []\nsearch: {per_ip_per_minute: 2}");
        assert!(
            l.charge_search(ip("2001:db8:1:2::1"))
                .is_ok()
        );
        assert!(
            l.charge_search(ip("2001:db8:1:2:ffff::9"))
                .is_ok()
        );
        assert!(
            l.charge_search(ip("2001:db8:1:2::3"))
                .is_err()
        );
        assert!(
            l.charge_search(ip("2001:db8:1:3::1"))
                .is_ok()
        );
        assert!(
            l.charge_search(ip("::ffff:192.0.2.1"))
                .is_ok()
        );
        assert!(
            l.charge_search(ip("192.0.2.1"))
                .is_ok()
        );
        assert!(
            l.charge_search(ip("192.0.2.1"))
                .is_err()
        );
    }

    #[test]
    fn ask_budget_charges_client_then_address() {
        let l = limits(
            "logs_dirs: []\nai: {api_key: k, output_dir: /x, requests_per_day_per_client: 2, \
             requests_per_day_per_ip: 3}",
        );
        let addr = ip("192.0.2.1");
        assert_eq!(l.charge_ask("a", addr), Ok(()));
        assert_eq!(l.charge_ask("a", addr), Ok(()));
        assert!(matches!(l.charge_ask("a", addr), Err(Refusal::Client(_))));
        assert_eq!(l.charge_ask("b", addr), Ok(()));
        assert!(matches!(l.charge_ask("b", addr), Err(Refusal::Address(_))));
        let other = ip("192.0.2.2");
        assert!(matches!(l.charge_ask("a", other), Err(Refusal::Client(_))));
        assert_eq!(l.charge_ask("c", other), Ok(()));
    }

    #[test]
    fn client_id_accepts_only_well_formed_values() {
        let id = new_client_id().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("x=1; {ASK_COOKIE}={id}")
                .parse()
                .unwrap(),
        );
        assert_eq!(client_id(&headers), Some(id));
        headers.insert(
            header::COOKIE,
            format!("{ASK_COOKIE}=../../etc")
                .parse()
                .unwrap(),
        );
        assert_eq!(client_id(&headers), None);
        assert_eq!(client_id(&HeaderMap::new()), None);
    }
}

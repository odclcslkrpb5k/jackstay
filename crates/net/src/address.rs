//! A peer's address as the user gives it: `host`, `host:port`, `[v6]:port`, or a bare IPv6
//! address. Resolved again before every dial, so a changed address is followed.

use std::fmt;
use std::net::{IpAddr, SocketAddr};

/// The longest host name accepted (RFC 1035's 253, with room for a trailing dot).
const MAX_HOST_LEN: usize = 254;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    host: String,
    port: Option<u16>,
}

impl Address {
    /// The address in `text`, or `None` if it is not one: empty, too long, holding a space or a
    /// character a host name cannot have, or with a port of 0 or out of range.
    #[must_use]
    pub fn parse(text: &str) -> Option<Address> {
        let (host, port) = if let Some(rest) = text.strip_prefix('[') {
            let (host, after) = rest.split_once(']')?;
            host.parse::<std::net::Ipv6Addr>().ok()?;
            let port = match after {
                "" => None,
                p => Some(port(p.strip_prefix(':')?)?),
            };
            (host, port)
        } else if text.parse::<IpAddr>().is_ok() {
            (text, None)
        } else if let Some((host, p)) = text.rsplit_once(':') {
            (host, Some(port(p)?))
        } else {
            (text, None)
        };
        let host_ok = !host.is_empty()
            && host.len() <= MAX_HOST_LEN
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'));
        host_ok.then(|| Address { host: host.to_owned(), port })
    }

    /// The port to dial: the one given, or `default`.
    #[must_use]
    pub fn port_or(&self, default: u16) -> u16 {
        self.port.unwrap_or(default)
    }

    /// Every address the host resolves to, at the given port, IPv6 and IPv4 as the resolver orders
    /// them. Runs the system resolver on tokio's blocking pool: it can stall.
    pub(crate) async fn resolve(&self, port: u16) -> Vec<SocketAddr> {
        match tokio::net::lookup_host((self.host.as_str(), port)).await {
            Ok(addrs) => addrs.collect(),
            Err(_) => Vec::new(),
        }
    }
}

fn port(text: &str) -> Option<u16> {
    text.parse::<u16>().ok().filter(|&p| p != 0 && text.bytes().all(|b| b.is_ascii_digit()))
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v6 = self.host.contains(':');
        match (self.port, v6) {
            (None, _) => write!(f, "{}", self.host),
            (Some(p), true) => write!(f, "[{}]:{p}", self.host),
            (Some(p), false) => write!(f, "{}:{p}", self.host),
        }
    }
}

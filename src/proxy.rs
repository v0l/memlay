//! Client address resolution behind a reverse proxy.
//!
//! When memlay sits behind an L7 proxy (nginx ingress, Cloudflare, …) every
//! connection arrives from the proxy, so logs and per-connection accounting all
//! show one address. The proxy passes the real client in `X-Forwarded-For` /
//! `X-Real-IP`, but those headers are attacker-controlled on a direct
//! connection, so they are only believed when the peer address itself matches a
//! configured trusted proxy range.

use axum::http::HeaderMap;
use ipnet::IpNet;
use std::net::{IpAddr, SocketAddr};

/// Trusted reverse proxies, parsed from config at startup.
#[derive(Debug, Default, Clone)]
pub struct TrustedProxies {
    nets: Vec<IpNet>,
}

impl TrustedProxies {
    /// Parse CIDRs (`10.244.0.0/16`) or bare IPs (`10.244.0.93`). Unparsable
    /// entries are logged and skipped rather than failing startup.
    pub fn parse(entries: &[String]) -> Self {
        let nets = entries
            .iter()
            .filter_map(|entry| match entry.parse::<IpNet>() {
                Ok(net) => Some(net),
                Err(_) => match entry.parse::<IpAddr>() {
                    Ok(ip) => Some(IpNet::from(ip)),
                    Err(e) => {
                        tracing::warn!(entry = %entry, error = %e, "ignoring invalid trusted_proxies entry");
                        None
                    }
                },
            })
            .collect();
        Self { nets }
    }

    pub fn is_empty(&self) -> bool {
        self.nets.is_empty()
    }

    fn trusts(&self, ip: IpAddr) -> bool {
        // An IPv4 peer arriving on a dual-stack listener shows up as
        // ::ffff:a.b.c.d, which never matches an IPv4 CIDR unless unmapped.
        let ip = unmap(ip);
        self.nets.iter().any(|net| net.contains(&ip))
    }

    /// The address to attribute this request to: the forwarded client when the
    /// peer is a trusted proxy and sent a usable header, otherwise the peer.
    ///
    /// The port is kept from the peer connection. It is the proxy's ephemeral
    /// port, not the client's, but it is what distinguishes two connections
    /// from the same address in logs.
    pub fn client_addr(&self, peer: SocketAddr, headers: &HeaderMap) -> SocketAddr {
        if self.nets.is_empty() || !self.trusts(peer.ip()) {
            return peer;
        }
        match forwarded_ip(headers) {
            Some(ip) => SocketAddr::new(ip, peer.port()),
            None => peer,
        }
    }
}

/// `X-Real-IP` first (nginx sets it to the immediate client), else the
/// left-most `X-Forwarded-For` entry.
fn forwarded_ip(headers: &HeaderMap) -> Option<IpAddr> {
    let real_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<IpAddr>().ok());
    if real_ip.is_some() {
        return real_ip;
    }

    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())?
        .split(',')
        .find_map(|part| part.trim().parse::<IpAddr>().ok())
}

fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn untrusted_peer_headers_are_ignored() {
        let trusted = TrustedProxies::parse(&["10.244.0.0/16".to_string()]);
        let peer = addr("203.0.113.7:4444");
        let got = trusted.client_addr(peer, &headers(&[("x-forwarded-for", "1.2.3.4")]));
        assert_eq!(got, peer);
    }

    #[test]
    fn trusted_peer_headers_are_used() {
        let trusted = TrustedProxies::parse(&["10.244.0.0/16".to_string()]);
        let got = trusted.client_addr(
            addr("10.244.0.93:4444"),
            &headers(&[("x-forwarded-for", "1.2.3.4, 10.244.0.93")]),
        );
        assert_eq!(got, addr("1.2.3.4:4444"));
    }

    #[test]
    fn real_ip_wins_over_forwarded_for() {
        let trusted = TrustedProxies::parse(&["10.244.0.93".to_string()]);
        let got = trusted.client_addr(
            addr("10.244.0.93:4444"),
            &headers(&[("x-real-ip", "9.9.9.9"), ("x-forwarded-for", "1.2.3.4")]),
        );
        assert_eq!(got, addr("9.9.9.9:4444"));
    }

    #[test]
    fn ipv4_mapped_peer_matches_ipv4_cidr() {
        let trusted = TrustedProxies::parse(&["10.244.0.0/16".to_string()]);
        let got = trusted.client_addr(
            addr("[::ffff:10.244.0.93]:4444"),
            &headers(&[("x-real-ip", "1.2.3.4")]),
        );
        assert_eq!(got, addr("1.2.3.4:4444"));
    }

    #[test]
    fn garbage_header_falls_back_to_peer() {
        let trusted = TrustedProxies::parse(&["10.244.0.0/16".to_string()]);
        let peer = addr("10.244.0.93:4444");
        assert_eq!(
            trusted.client_addr(peer, &headers(&[("x-forwarded-for", "not-an-ip")])),
            peer
        );
        assert_eq!(trusted.client_addr(peer, &HeaderMap::new()), peer);
    }

    #[test]
    fn no_config_means_peer_only() {
        let trusted = TrustedProxies::parse(&[]);
        assert!(trusted.is_empty());
        let peer = addr("10.244.0.93:4444");
        assert_eq!(
            trusted.client_addr(peer, &headers(&[("x-real-ip", "1.2.3.4")])),
            peer
        );
    }
}

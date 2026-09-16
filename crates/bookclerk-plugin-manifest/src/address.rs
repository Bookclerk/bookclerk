//! Address-space policy independent of hostname matching.
//!
//! Default reachability is the public Internet. Loopback, RFC1918, link-local,
//! unique-local, metadata, and IPv4-mapped forms of those ranges stay denied
//! unless the operator grants an explicit CIDR. A public-redirect capability
//! never implies those ranges.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// One operator- or manifest-granted CIDR (`10.0.60.100/32`, `::1/128`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CidrGrant {
    /// Network address (host bits masked off).
    pub network: IpAddr,
    /// Prefix length.
    pub prefix: u8,
}

impl CidrGrant {
    /// Parses `addr/prefix`. Bare addresses are treated as `/32` or `/128`.
    ///
    /// # Errors
    ///
    /// Returns a message when the CIDR is malformed or the prefix is too large.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("empty CIDR".into());
        }
        let (addr_s, prefix) = match raw.split_once('/') {
            Some((addr, p)) => {
                let prefix: u8 = p
                    .parse()
                    .map_err(|_| format!("invalid CIDR prefix `{p}`"))?;
                (addr, prefix)
            }
            None => {
                let addr: IpAddr = raw.parse().map_err(|_| format!("invalid IP `{raw}`"))?;
                let prefix = if addr.is_ipv4() { 32 } else { 128 };
                return Self::masked(addr, prefix);
            }
        };
        let addr: IpAddr = addr_s
            .parse()
            .map_err(|_| format!("invalid CIDR address `{addr_s}`"))?;
        Self::masked(addr, prefix)
    }

    /// Masks host bits so `network` is the canonical subnet address.
    ///
    /// # Errors
    ///
    /// Returns a message when `prefix` exceeds the address family's width.
    fn masked(addr: IpAddr, prefix: u8) -> Result<Self, String> {
        let max = if addr.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return Err(format!("CIDR prefix {prefix} exceeds {max}"));
        }
        Ok(Self {
            network: mask_ip(addr, prefix),
            prefix,
        })
    }

    /// True when `ip` (after IPv4-mapped unwrapping) falls in this range.
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical_ip(ip);
        let net = canonical_ip(self.network);
        match (ip, net) {
            (IpAddr::V4(ip), IpAddr::V4(net)) => ipv4_prefix_eq(ip, net, self.prefix.min(32)),
            (IpAddr::V6(ip), IpAddr::V6(net)) => ipv6_prefix_eq(ip, net, self.prefix.min(128)),
            _ => false,
        }
    }
}

/// Unwraps IPv4-mapped IPv6 so `:ffff:127.0.0.1` is treated as IPv4.
#[must_use]
pub fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        other => other,
    }
}

/// True when `ip` is reachable on the public Internet (not a special range).
#[must_use]
pub fn is_public_internet(ip: IpAddr) -> bool {
    let ip = canonical_ip(ip);
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

/// True when `v4` is not a special-use / private range.
fn is_public_v4(v4: Ipv4Addr) -> bool {
    !(v4.is_unspecified()
        || v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
        || v4.octets()[0] == 0
        || is_cgnat(v4)
        || is_benchmark_v4(v4))
}

/// True when `v6` is not loopback, ULA, link-local, multicast, or documentation.
fn is_public_v6(v6: Ipv6Addr) -> bool {
    !(v6.is_unspecified()
        || v6.is_loopback()
        || v6.is_multicast()
        || v6.is_unique_local()
        || v6.is_unicast_link_local()
        || v6.segments()[0] == 0x2001 && v6.segments()[1] == 0xdb8)
}

/// RFC 6598 shared address space (`100.64.0.0/10`).
fn is_cgnat(v4: Ipv4Addr) -> bool {
    v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 64
}

/// RFC 2544 benchmark range (`198.18.0.0/15`).
fn is_benchmark_v4(v4: Ipv4Addr) -> bool {
    v4.octets()[0] == 198 && (v4.octets()[1] == 18 || v4.octets()[1] == 19)
}

/// Special hostnames that resolve into denied address space without a CIDR grant.
#[must_use]
pub fn is_restricted_hostname(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    h == "localhost"
        || h == "localhost.localdomain"
        || h.ends_with(".localhost")
        || h == "metadata.google.internal"
}

/// True when `ip` is allowed: public Internet, or an explicit CIDR grant.
#[must_use]
pub fn address_allowed(ip: IpAddr, grants: &[CidrGrant]) -> bool {
    if is_public_internet(ip) {
        return true;
    }
    grants.iter().any(|g| g.contains(ip))
}

/// Workerd `network.allow` tokens for this policy: `public` plus granted CIDRs.
///
/// Never emits the blanket `private` / `local` / `network` tokens.
#[must_use]
pub fn workerd_network_allow(cidrs: &[CidrGrant]) -> Vec<String> {
    let mut allow = vec!["public".to_string()];
    for c in cidrs {
        allow.push(format!("{}/{}", c.network, c.prefix));
    }
    allow.sort();
    allow.dedup();
    allow
}

/// Clears host bits for `addr`/`prefix`.
fn mask_ip(addr: IpAddr, prefix: u8) -> IpAddr {
    match addr {
        IpAddr::V4(v4) => {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            IpAddr::V4(Ipv4Addr::from(u32::from(v4) & mask))
        }
        IpAddr::V6(v6) => {
            let bits = u128::from(v6);
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            IpAddr::V6(Ipv6Addr::from(bits & mask))
        }
    }
}

/// Prefix match for IPv4.
fn ipv4_prefix_eq(ip: Ipv4Addr, net: Ipv4Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    (u32::from(ip) & mask) == (u32::from(net) & mask)
}

/// Prefix match for IPv6.
fn ipv6_prefix_eq(ip: Ipv6Addr, net: Ipv6Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    };
    (u128::from(ip) & mask) == (u128::from(net) & mask)
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("ip")
    }

    #[test]
    fn default_denies_special_ranges() {
        for s in [
            "127.0.0.1",
            "::1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.0.1",
            "169.254.169.254",
            "169.254.0.1",
            "100.64.0.1",
            "0.0.0.0",
            "fc00::1",
            "fd12:3456:789a::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:192.168.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(
                !is_public_internet(ip(s)),
                "{s} must not be public Internet"
            );
            assert!(
                !address_allowed(ip(s), &[]),
                "{s} denied without CIDR grant"
            );
        }
    }

    #[test]
    fn public_unicast_is_allowed() {
        assert!(is_public_internet(ip("1.1.1.1")));
        assert!(is_public_internet(ip("8.8.8.8")));
        assert!(is_public_internet(ip("2001:4860:4860::8888")));
        assert!(address_allowed(ip("1.1.1.1"), &[]));
    }

    #[test]
    fn explicit_cidr_allows_private() {
        let g = CidrGrant::parse("10.0.0.0/8").unwrap();
        assert!(address_allowed(ip("10.0.0.1"), std::slice::from_ref(&g)));
        assert!(!address_allowed(
            ip("192.168.0.1"),
            std::slice::from_ref(&g)
        ));
        assert!(address_allowed(ip("::ffff:10.1.2.3"), &[g]));
    }

    #[test]
    fn loopback_and_link_local_need_exact_grants() {
        let loop4 = CidrGrant::parse("127.0.0.1/32").unwrap();
        let loop6 = CidrGrant::parse("::1/128").unwrap();
        let lla = CidrGrant::parse("169.254.0.0/16").unwrap();
        let ula = CidrGrant::parse("fc00::/7").unwrap();
        let fe80 = CidrGrant::parse("fe80::/10").unwrap();
        assert!(address_allowed(ip("127.0.0.1"), &[loop4]));
        assert!(address_allowed(ip("::1"), &[loop6]));
        assert!(address_allowed(ip("169.254.169.254"), &[lla]));
        assert!(address_allowed(ip("fd00::1"), &[ula]));
        assert!(address_allowed(ip("fe80::1"), &[fe80]));
        assert!(!address_allowed(ip("127.0.0.1"), &[]));
    }

    #[test]
    fn workerd_allow_never_uses_private_blanket() {
        let g = CidrGrant::parse("192.168.50.0/24").unwrap();
        let allow = workerd_network_allow(&[g]);
        assert!(allow.contains(&"public".into()));
        assert!(allow.iter().any(|t| t == "192.168.50.0/24"));
        assert!(!allow.iter().any(|t| t == "private" || t == "local"));
    }

    #[test]
    fn restricted_hostnames() {
        assert!(is_restricted_hostname("localhost"));
        assert!(is_restricted_hostname("Foo.Localhost."));
        assert!(!is_restricted_hostname("api.example.com"));
    }

    #[test]
    fn rfc1918_classifier() {
        assert!(Ipv4Addr::new(10, 0, 0, 1).is_private());
        assert!(Ipv4Addr::new(172, 16, 0, 1).is_private());
        assert!(Ipv4Addr::new(192, 168, 0, 1).is_private());
        assert!(!Ipv4Addr::new(172, 15, 0, 1).is_private());
    }
}

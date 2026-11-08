//! The network allowlist. A connection from outside it is closed before the WebSocket handshake,
//! so an unlisted peer gets no bytes from the protocol at all.

use std::net::{IpAddr, Ipv4Addr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: u32,
    mask: u32,
}

impl Cidr {
    /// Parse `a.b.c.d/n`. Host bits are masked away, so `10.0.0.3/24` is `10.0.0.0/24`.
    pub fn parse(text: &str) -> Result<Cidr, String> {
        let (addr, bits) = text.split_once('/')
            .ok_or_else(|| format!("{text}: expected a CIDR such as 10.0.0.0/24"))?;
        let ip: Ipv4Addr = addr.parse().map_err(|_| format!("{text}: not an IPv4 address"))?;
        let bits: u32 = bits.parse().map_err(|_| format!("{text}: prefix length is not a number"))?;
        if bits > 32 {
            return Err(format!("{text}: prefix length must be 0 to 32"));
        }
        let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
        Ok(Cidr { network: u32::from(ip) & mask, mask })
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask == self.network
    }
}

/// A comma-separated list of CIDRs, as the `--allow` flag takes it.
pub fn parse_list(text: &str) -> Result<Vec<Cidr>, String> {
    let list: Result<Vec<Cidr>, String> = text.split(',').map(str::trim).filter(|s| !s.is_empty())
        .map(Cidr::parse).collect();
    let list = list?;
    if list.is_empty() {
        return Err("the allowlist is empty: nothing could connect".into());
    }
    Ok(list)
}

/// Whether `ip` is inside any allowed network. An IPv6 peer is allowed only when it is an
/// IPv4-mapped address inside the list.
pub fn allowed(list: &[Cidr], ip: IpAddr) -> bool {
    let v4 = match ip {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(v6) => v6.to_ipv4_mapped(),
    };
    v4.is_some_and(|v4| list.iter().any(|c| c.contains(v4)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cidr_contains_its_own_block_and_nothing_else() {
        let c = Cidr::parse("10.0.0.0/24").unwrap();
        assert!(c.contains("10.0.0.3".parse().unwrap()));
        assert!(c.contains("10.0.0.255".parse().unwrap()));
        assert!(!c.contains("10.10.21.1".parse().unwrap()));
        assert!(!c.contains("192.168.1.1".parse().unwrap()));
        assert_eq!(Cidr::parse("10.0.0.3/24").unwrap(), c, "host bits are masked");
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn bad_cidrs_are_refused() {
        assert!(Cidr::parse("10.0.0.0").is_err());
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("10.10.20/24").is_err());
        assert!(parse_list("").is_err(), "an empty allowlist would silently refuse everything");
    }

    #[test]
    fn the_default_list_admits_loopback_private_and_shared_vpn_addresses_only() {
        let list = parse_list(super::super::DEFAULT_ALLOW).unwrap();
        for inside in ["127.0.0.1", "10.0.0.7", "172.20.1.2", "192.168.1.188", "100.101.102.103", "::ffff:10.0.0.7"] {
            assert!(allowed(&list, inside.parse().unwrap()), "{inside}");
        }
        for outside in ["203.0.113.9", "8.8.8.8", "172.32.0.1", "100.128.0.1", "2001:db8::1"] {
            assert!(!allowed(&list, outside.parse().unwrap()), "{outside}");
        }
    }
}

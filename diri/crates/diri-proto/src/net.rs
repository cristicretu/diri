//! Where a Dirijor listener is allowed to bind.
//!
//! Every TCP surface in this workspace — the node management port, the web
//! frontend — is a *private* surface: the transport is expected to be a
//! tailnet, a LAN, or loopback, and the app-layer token is what authenticates
//! the caller. Neither of those assumptions survives a public bind, so the
//! check lives here once rather than once per listener.

use std::net::{IpAddr, SocketAddr};

/// True when `address` is on loopback, a private LAN, the Tailscale/CGNAT
/// range (`100.64.0.0/10`), or link-local.
///
/// `0.0.0.0` is deliberately rejected: a wildcard bind reaches the public
/// interface on every VPS this project runs on.
#[must_use]
pub fn is_private_bind_address(address: SocketAddr) -> bool {
    match address.ip() {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            ip.is_loopback()
                || ip.is_private()
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || ip.is_link_local()
        }
        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_lan_and_tailscale_are_private() {
        assert!(is_private_bind_address("127.0.0.1:7337".parse().unwrap()));
        assert!(is_private_bind_address("192.168.1.2:7337".parse().unwrap()));
        assert!(is_private_bind_address("10.0.0.4:7337".parse().unwrap()));
        assert!(is_private_bind_address("100.64.12.2:7337".parse().unwrap()));
        assert!(is_private_bind_address(
            "100.66.149.100:7337".parse().unwrap()
        ));
        assert!(is_private_bind_address("[::1]:7337".parse().unwrap()));
    }

    #[test]
    fn wildcard_and_public_addresses_are_not() {
        assert!(!is_private_bind_address("0.0.0.0:7337".parse().unwrap()));
        assert!(!is_private_bind_address("8.8.8.8:7337".parse().unwrap()));
        assert!(!is_private_bind_address("[::]:7337".parse().unwrap()));
    }

    /// `100.64.0.0/10` ends at `100.127.255.255`; `100.128.x.x` is public
    /// space and must not be mistaken for a tailnet.
    #[test]
    fn the_cgnat_range_stops_at_its_boundary() {
        assert!(is_private_bind_address(
            "100.127.255.255:1".parse().unwrap()
        ));
        assert!(!is_private_bind_address("100.128.0.1:1".parse().unwrap()));
        assert!(!is_private_bind_address(
            "100.63.255.255:1".parse().unwrap()
        ));
    }
}

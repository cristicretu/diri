//! Where a Dirijor listener is allowed to bind.
//!
//! Every TCP surface in this workspace carries a bearer credential without
//! adding TLS. It may therefore bind only to loopback or a Tailscale address,
//! where the transport is encrypted before it leaves the machine.

use std::net::{IpAddr, SocketAddr};

/// True when `address` is on loopback or a Tailscale address.
///
/// Tailscale uses `100.64.0.0/10` for IPv4 and `fd7a:115c:a1e0::/48` for IPv6.
/// Other private, unique-local, link-local, and wildcard addresses are rejected
/// because they do not prove the bearer is crossing an encrypted transport.
#[must_use]
pub fn is_safe_plaintext_address(address: SocketAddr) -> bool {
    match address.ip() {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            ip.is_loopback() || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            ip.is_loopback()
                || (segments[0] == 0xfd7a && segments[1] == 0x115c && segments[2] == 0xa1e0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_and_tailscale_are_secure() {
        assert!(is_safe_plaintext_address("127.0.0.1:7337".parse().unwrap()));
        assert!(is_safe_plaintext_address(
            "100.64.12.2:7337".parse().unwrap()
        ));
        assert!(is_safe_plaintext_address(
            "100.66.149.100:7337".parse().unwrap()
        ));
        assert!(is_safe_plaintext_address("[::1]:7337".parse().unwrap()));
        assert!(is_safe_plaintext_address(
            "[fd7a:115c:a1e0::1]:7337".parse().unwrap()
        ));
    }

    #[test]
    fn lan_link_local_wildcard_and_public_addresses_are_not() {
        for address in [
            "192.168.1.2:7337",
            "10.0.0.4:7337",
            "169.254.1.2:7337",
            "0.0.0.0:7337",
            "8.8.8.8:7337",
            "[fd12::1]:7337",
            "[fe80::1]:7337",
            "[::]:7337",
        ] {
            assert!(
                !is_safe_plaintext_address(address.parse().unwrap()),
                "{address}"
            );
        }
    }

    /// `100.64.0.0/10` ends at `100.127.255.255`; `100.128.x.x` is public
    /// space and must not be mistaken for a tailnet.
    #[test]
    fn the_cgnat_range_stops_at_its_boundary() {
        assert!(is_safe_plaintext_address(
            "100.127.255.255:1".parse().unwrap()
        ));
        assert!(!is_safe_plaintext_address("100.128.0.1:1".parse().unwrap()));
        assert!(!is_safe_plaintext_address(
            "100.63.255.255:1".parse().unwrap()
        ));
    }
}

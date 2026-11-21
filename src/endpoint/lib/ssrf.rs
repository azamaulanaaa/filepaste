use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The largest response body that the endpoint stores, in bytes.
pub const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

/// The largest number of response headers that the endpoint accepts.
pub const MAX_RESPONSE_HEADERS: usize = 64;

/// The largest total size of response headers, in bytes.
pub const MAX_RESPONSE_HEADER_BYTES: usize = 16 * 1024;

/// The time allowed to connect to the target, in seconds.
pub const CONNECT_TIMEOUT_SECS: u64 = 5;

/// The total time allowed for a fetch, in seconds.
pub const DEFAULT_TOTAL_TIMEOUT_SECS: u64 = 30;

/// The largest total time allowed for a fetch, in seconds.
pub const MAX_TOTAL_TIMEOUT_SECS: u64 = 60;

/// Return true when the endpoint must not connect to this address.
pub fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => is_forbidden_ipv4(address),
        IpAddr::V6(address) => is_forbidden_ipv6(address),
    }
}

fn is_forbidden_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4() {
        return is_forbidden_ipv4(mapped);
    }

    let octets = address.octets();
    if address.is_loopback()
        || address.is_unspecified()
        || address.is_unicast_link_local()
        || address.is_unique_local()
        || address.is_multicast()
    {
        return true;
    }

    // NAT64 embeds an IPv4 address in the last four octets.
    if octets[0..4] == [0, 0x64, 0xff, 0x9b] && octets[4..12].iter().all(|byte| *byte == 0) {
        let embedded = Ipv4Addr::new(octets[12], octets[13], octets[14], octets[15]);
        return is_forbidden_ipv4(embedded);
    }

    // 6to4 embeds an IPv4 address in octets two through five.
    if octets[0] == 0x20 && octets[1] == 0x02 {
        let embedded = Ipv4Addr::new(octets[2], octets[3], octets[4], octets[5]);
        return is_forbidden_ipv4(embedded);
    }

    false
}

fn is_forbidden_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_multicast()
        || address.is_broadcast()
        || address.is_documentation()
        || is_reserved_ipv4(octets)
        // 100.64.0.0/10 is a shared address range.
        || (octets[0] == 100 && (octets[1] & 0xc0) == 0x40)
}

fn is_reserved_ipv4(octets: [u8; 4]) -> bool {
    octets[0] == 0
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 198 && (octets[1] & 0xfe) == 18)
        || octets[0] >= 240
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_loopback_and_private_ipv4_addresses() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "0.0.0.0",
        ] {
            let ip: IpAddr = address.parse().expect("parse IPv4 address");
            assert!(is_forbidden_ip(ip), "address must be blocked: {address}");
        }
    }

    #[test]
    fn rejects_cloud_metadata_addresses() {
        for address in ["169.254.169.254", "100.100.100.200"] {
            let ip: IpAddr = address.parse().expect("parse IPv4 address");
            assert!(is_forbidden_ip(ip), "address must be blocked: {address}");
        }
        let aws_ipv6: IpAddr = "fd00:ec2::254".parse().expect("parse IPv6 address");
        assert!(is_forbidden_ip(aws_ipv6));
    }

    #[test]
    fn rejects_loopback_and_unique_local_ipv6_addresses() {
        for address in ["::1", "fc00::1", "fd12::1", "fe80::1", "::", "ff02::1"] {
            let ip: IpAddr = address.parse().expect("parse IPv6 address");
            assert!(is_forbidden_ip(ip), "address must be blocked: {address}");
        }
    }

    #[test]
    fn rejects_embedded_ipv4_addresses() {
        for address in ["::ffff:127.0.0.1", "64:ff9b::7f00:1", "2002:7f00:1::"] {
            let ip: IpAddr = address.parse().expect("parse IPv6 address");
            assert!(is_forbidden_ip(ip), "address must be blocked: {address}");
        }
    }

    #[test]
    fn accepts_public_addresses() {
        for address in ["1.1.1.1", "8.8.8.8", "2001:4860:4860::8888"] {
            let ip: IpAddr = address.parse().expect("parse IP address");
            assert!(!is_forbidden_ip(ip), "address must be allowed: {address}");
        }
    }
}

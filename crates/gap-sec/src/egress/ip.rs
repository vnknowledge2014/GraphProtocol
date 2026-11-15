//! IPv4/IPv6 CIDR matching and SSRF range classification (`SEC-075`, `SEC-076`).

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// Reason an IP address was blocked by [`classify_blocked_ip`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockedIpReason {
    /// Unspecified / "this network" range (`0.0.0.0/8`, `::`).
    Unspecified,
    /// Host loopback range (`127.0.0.0/8`, `::1`).
    Loopback,
    /// RFC 1918 private IPv4 ranges (`10/8`, `172.16/12`, `192.168/16`).
    PrivateRfc1918,
    /// RFC 4193 IPv6 Unique Local Addresses (`fc00::/7`) or deprecated site-local (`fec0::/10`).
    UniqueLocalIpv6,
    /// Link-local addresses (`169.254.0.0/16`, `fe80::/10`).
    LinkLocal,
    /// Well-known cloud instance metadata endpoints (`169.254.169.254`, `169.254.170.2`, `100.100.100.200`, `fd00:ec2::254`).
    CloudMetadata,
    /// Shared CGNAT range (`100.64.0.0/10`, RFC 6598).
    Cgnat,
    /// Multicast ranges (`224.0.0.0/4`, `ff00::/8`).
    Multicast,
    /// Benchmarking, documentation, IETF protocol assignments, reserved, or broadcast ranges.
    ReservedOrDocumentation,
    /// IPv6 transition/tunnel address embedding a blocked IPv4 target (IPv4-mapped, NAT64, 6to4, Teredo, compatible).
    EmbeddedTunnelAddress,
}

impl fmt::Display for BlockedIpReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::Unspecified => "unspecified address range",
            Self::Loopback => "loopback address range",
            Self::PrivateRfc1918 => "RFC 1918 private IPv4 range",
            Self::UniqueLocalIpv6 => "RFC 4193 unique-local IPv6 range",
            Self::LinkLocal => "link-local address range",
            Self::CloudMetadata => "cloud instance metadata endpoint",
            Self::Cgnat => "RFC 6598 CGNAT shared address space",
            Self::Multicast => "multicast address range",
            Self::ReservedOrDocumentation => "reserved or documentation address range",
            Self::EmbeddedTunnelAddress => "IPv6 transition/tunnel address wrapping blocked IPv4",
        };
        f.write_str(msg)
    }
}

/// Parsed IPv4 or IPv6 CIDR network prefix used in branch `egress.allow[]` rules (`SEC-075`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CidrBlock {
    /// IPv4 network prefix (`network` already masked).
    V4 {
        /// Network address in host byte order with host bits zeroed.
        network: u32,
        /// Prefix length (`0..=32`).
        prefix: u8,
    },
    /// IPv6 network prefix (`network` already masked).
    V6 {
        /// Network address in host byte order with host bits zeroed.
        network: u128,
        /// Prefix length (`0..=128`).
        prefix: u8,
    },
}

impl CidrBlock {
    /// Constructs a canonical IPv4 CIDR block, masking host bits.
    #[must_use]
    pub const fn from_v4(addr: Ipv4Addr, prefix: u8) -> Option<Self> {
        if prefix > 32 {
            return None;
        }
        let bits = u32::from_be_bytes(addr.octets());
        let mask = v4_mask(prefix);
        Some(Self::V4 {
            network: bits & mask,
            prefix,
        })
    }

    /// Constructs a canonical IPv6 CIDR block, masking host bits.
    #[must_use]
    pub const fn from_v6(addr: Ipv6Addr, prefix: u8) -> Option<Self> {
        if prefix > 128 {
            return None;
        }
        let bits = u128::from_be_bytes(addr.octets());
        let mask = v6_mask(prefix);
        Some(Self::V6 {
            network: bits & mask,
            prefix,
        })
    }

    /// Parses a CIDR literal (or bare host IP, treated as `/32` or `/128`).
    ///
    /// # Errors
    ///
    /// Returns an error description when the address or prefix length is invalid.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if let Some((ip_part, prefix_part)) = trimmed.split_once('/') {
            let prefix: u8 = prefix_part
                .parse()
                .map_err(|_| format!("invalid CIDR prefix `{prefix_part}`"))?;
            let ip = IpAddr::from_str(ip_part)
                .map_err(|_| format!("invalid CIDR address `{ip_part}`"))?;
            match ip {
                IpAddr::V4(v4) => Self::from_v4(v4, prefix)
                    .ok_or_else(|| format!("IPv4 CIDR prefix `{prefix}` out of range 0..=32")),
                IpAddr::V6(v6) => Self::from_v6(v6, prefix)
                    .ok_or_else(|| format!("IPv6 CIDR prefix `{prefix}` out of range 0..=128")),
            }
        } else {
            let ip = IpAddr::from_str(trimmed)
                .map_err(|_| format!("invalid IP/CIDR literal `{trimmed}`"))?;
            Ok(match ip {
                IpAddr::V4(v4) => Self::V4 {
                    network: u32::from_be_bytes(v4.octets()),
                    prefix: 32,
                },
                IpAddr::V6(v6) => Self::V6 {
                    network: u128::from_be_bytes(v6.octets()),
                    prefix: 128,
                },
            })
        }
    }

    /// Returns `true` when `ip` falls inside this CIDR prefix.
    ///
    /// Note: IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) are also checked against IPv4 CIDRs.
    #[must_use]
    pub const fn contains(&self, ip: IpAddr) -> bool {
        match (*self, ip) {
            (Self::V4 { network, prefix }, IpAddr::V4(v4)) => {
                let bits = u32::from_be_bytes(v4.octets());
                (bits & v4_mask(prefix)) == network
            }
            (Self::V6 { network, prefix }, IpAddr::V6(v6)) => {
                let bits = u128::from_be_bytes(v6.octets());
                (bits & v6_mask(prefix)) == network
            }
            (Self::V4 { network, prefix }, IpAddr::V6(v6)) => match extract_ipv4_mapped(v6) {
                Some(v4) => {
                    let bits = u32::from_be_bytes(v4.octets());
                    (bits & v4_mask(prefix)) == network
                }
                None => false,
            },
            (Self::V6 { .. }, IpAddr::V4(_)) => false,
        }
    }
}

impl fmt::Display for CidrBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V4 { network, prefix } => {
                let addr = Ipv4Addr::from(*network);
                write!(f, "{addr}/{prefix}")
            }
            Self::V6 { network, prefix } => {
                let addr = Ipv6Addr::from(*network);
                write!(f, "{addr}/{prefix}")
            }
        }
    }
}

const fn v4_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

const fn v6_mask(prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    }
}

const fn v4_in_cidr(addr: u32, net: [u8; 4], prefix: u8) -> bool {
    let net_bits = u32::from_be_bytes(net);
    let mask = v4_mask(prefix);
    (addr & mask) == (net_bits & mask)
}

const fn v6_in_cidr(addr: u128, net: [u16; 8], prefix: u8) -> bool {
    let mut bytes = [0_u8; 16];
    let mut i = 0;
    while i < 8 {
        let seg = net[i].to_be_bytes();
        bytes[i * 2] = seg[0];
        bytes[i * 2 + 1] = seg[1];
        i += 1;
    }
    let net_bits = u128::from_be_bytes(bytes);
    let mask = v6_mask(prefix);
    (addr & mask) == (net_bits & mask)
}

const fn extract_ipv4_mapped(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = v6.segments();
    if s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0xffff {
        let hi = s[6].to_be_bytes();
        let lo = s[7].to_be_bytes();
        Some(Ipv4Addr::new(hi[0], hi[1], lo[0], lo[1]))
    } else {
        None
    }
}

const fn v4_from_bits(bits: u32) -> Ipv4Addr {
    let b = bits.to_be_bytes();
    Ipv4Addr::new(b[0], b[1], b[2], b[3])
}

/// Extracts an embedded IPv4 address from IPv6 transition mechanisms (`SEC-075`):
/// - IPv4-mapped (`::ffff:0:0/96`)
/// - IPv4-compatible (`::/96`, excluding `::` and `::1`)
/// - NAT64 well-known & local-use (`64:ff9b::/96`, `64:ff9b:1::/48`)
/// - 6to4 (`2002::/16`)
/// - Teredo (`2001:0000::/32`)
const fn extract_embedded_ipv4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(mapped) = extract_ipv4_mapped(v6) {
        return Some(mapped);
    }
    let s = v6.segments();
    // IPv4-compatible (`::a.b.c.d`) excluding `::` (`0.0.0.0`) and `::1` (`0.0.0.1`)
    if s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0 {
        let low32 = ((s[6] as u32) << 16) | (s[7] as u32);
        if low32 > 1 {
            return Some(v4_from_bits(low32));
        }
    }
    // NAT64 well-known prefix `64:ff9b::/96`
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0 {
        let low32 = ((s[6] as u32) << 16) | (s[7] as u32);
        return Some(v4_from_bits(low32));
    }
    // NAT64 local-use prefix `64:ff9b:1::/48` (IPv4 in low 32 bits)
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0x0001 {
        let low32 = ((s[6] as u32) << 16) | (s[7] as u32);
        return Some(v4_from_bits(low32));
    }
    // 6to4 prefix `2002::/16` -> IPv4 in segments 1..=2
    if s[0] == 0x2002 {
        let bits = ((s[1] as u32) << 16) | (s[2] as u32);
        return Some(v4_from_bits(bits));
    }
    // Teredo `2001:0000::/32` -> IPv4 client address bitwise-inverted in segments 6..=7
    if s[0] == 0x2001 && s[1] == 0x0000 {
        let obfuscated = ((s[6] as u32) << 16) | (s[7] as u32);
        return Some(v4_from_bits(!obfuscated));
    }
    None
}

/// Classifies whether `ip` belongs to a blocked SSRF destination range (`SEC-075`).
///
/// Returns `None` only for globally routable unicast addresses free of dangerous embedded IPv4
/// tunnels or metadata endpoints.
#[must_use]
pub const fn classify_blocked_ip(ip: IpAddr) -> Option<BlockedIpReason> {
    match ip {
        IpAddr::V4(v4) => classify_blocked_v4(v4),
        IpAddr::V6(v6) => classify_blocked_v6(v6),
    }
}

const fn classify_blocked_v4(v4: Ipv4Addr) -> Option<BlockedIpReason> {
    let bits = u32::from_be_bytes(v4.octets());

    // Exact cloud instance metadata endpoints (AWS/GCP/Azure `169.254.169.254`, ECS `169.254.170.2`, Alibaba `100.100.100.200`).
    if bits == u32::from_be_bytes([169, 254, 169, 254])
        || bits == u32::from_be_bytes([169, 254, 170, 2])
        || bits == u32::from_be_bytes([100, 100, 100, 200])
    {
        return Some(BlockedIpReason::CloudMetadata);
    }

    if v4_in_cidr(bits, [0, 0, 0, 0], 8) {
        return Some(BlockedIpReason::Unspecified);
    }
    if v4_in_cidr(bits, [127, 0, 0, 0], 8) {
        return Some(BlockedIpReason::Loopback);
    }
    if v4_in_cidr(bits, [10, 0, 0, 0], 8)
        || v4_in_cidr(bits, [172, 16, 0, 0], 12)
        || v4_in_cidr(bits, [192, 168, 0, 0], 16)
    {
        return Some(BlockedIpReason::PrivateRfc1918);
    }
    if v4_in_cidr(bits, [100, 64, 0, 0], 10) {
        return Some(BlockedIpReason::Cgnat);
    }
    if v4_in_cidr(bits, [169, 254, 0, 0], 16) {
        return Some(BlockedIpReason::LinkLocal);
    }
    if v4_in_cidr(bits, [224, 0, 0, 0], 4) {
        return Some(BlockedIpReason::Multicast);
    }
    if v4_in_cidr(bits, [192, 0, 0, 0], 24)
        || v4_in_cidr(bits, [192, 0, 2, 0], 24)
        || v4_in_cidr(bits, [198, 18, 0, 0], 15)
        || v4_in_cidr(bits, [198, 51, 100, 0], 24)
        || v4_in_cidr(bits, [203, 0, 113, 0], 24)
        || v4_in_cidr(bits, [240, 0, 0, 0], 4)
    {
        return Some(BlockedIpReason::ReservedOrDocumentation);
    }

    None
}

const fn classify_blocked_v6(v6: Ipv6Addr) -> Option<BlockedIpReason> {
    let bits = u128::from_be_bytes(v6.octets());

    // AWS EC2 IPv6 Instance Metadata Service `fd00:ec2::254`
    if v6_in_cidr(bits, [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254], 128) {
        return Some(BlockedIpReason::CloudMetadata);
    }

    if bits == 0 {
        return Some(BlockedIpReason::Unspecified);
    }
    if bits == 1 {
        return Some(BlockedIpReason::Loopback);
    }
    if v6_in_cidr(bits, [0xfe80, 0, 0, 0, 0, 0, 0, 0], 10) {
        return Some(BlockedIpReason::LinkLocal);
    }
    if v6_in_cidr(bits, [0xfc00, 0, 0, 0, 0, 0, 0, 0], 7)
        || v6_in_cidr(bits, [0xfec0, 0, 0, 0, 0, 0, 0, 0], 10)
    {
        return Some(BlockedIpReason::UniqueLocalIpv6);
    }
    if v6_in_cidr(bits, [0xff00, 0, 0, 0, 0, 0, 0, 0], 8) {
        return Some(BlockedIpReason::Multicast);
    }
    if v6_in_cidr(bits, [0x2001, 0x0db8, 0, 0, 0, 0, 0, 0], 32) {
        return Some(BlockedIpReason::ReservedOrDocumentation);
    }

    if let Some(embedded) = extract_embedded_ipv4(v6)
        && let Some(inner_reason) = classify_blocked_v4(embedded)
    {
        return match inner_reason {
            BlockedIpReason::CloudMetadata => Some(BlockedIpReason::CloudMetadata),
            _ => Some(BlockedIpReason::EmbeddedTunnelAddress),
        };
    }

    None
}

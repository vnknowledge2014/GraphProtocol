//! Egress validation and SSRF/metadata protections for [`super::HttpSidecar`] (`RTE-041`, `WP-M5-06`).

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use gap_core::error::{ErrorCode, GapError, GapResult};

const METADATA_HOSTS: &[&str] = &[
    "metadata.google.internal",
    "metadata",
    "instance-data",
    "169.254.169.254",
    "100.100.100.200",
    "fd00:ec2::254",
];

/// Parsed `http://` sidecar base endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEndpoint {
    /// Original sanitized endpoint URL without trailing slash.
    pub canonical_url: String,
    /// Hostname or IP literal (without IPv6 brackets).
    pub host: String,
    /// Explicit or default (`80`) TCP port.
    pub port: u16,
    /// Optional base path prefix (always starts with `'/'` or is empty; never has `/v1/systemone` suffix).
    pub base_path: String,
}

impl ParsedEndpoint {
    /// Parses and validates an `http://` sidecar URL (`RTE-041`).
    ///
    /// Rejects non-`http://` schemes, embedded userinfo (`@`), query strings (`?`), URL fragments
    /// (`#`), control characters, and empty hosts.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] when `raw` violates endpoint URL syntax rules.
    pub fn parse(raw: &str) -> GapResult<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty()
            || trimmed
                .bytes()
                .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
        {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "invalid sidecar endpoint: contains whitespace or control characters",
            ));
        }

        let rest = trimmed.strip_prefix("http://").ok_or_else(|| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                "invalid sidecar endpoint: scheme must be `http://`",
            )
        })?;

        if rest.contains('?') || rest.contains('#') {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "invalid sidecar endpoint: query strings and fragments are forbidden",
            ));
        }

        let (authority, raw_path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, ""),
        };

        if authority.is_empty() || authority.contains('@') {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "invalid sidecar endpoint: missing host or forbidden userinfo `@`",
            ));
        }

        let (host, port) = parse_authority(authority)?;
        if host.is_empty() {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "invalid sidecar endpoint: empty host",
            ));
        }

        let mut base_path = raw_path.trim_end_matches('/').to_owned();
        if base_path.contains("..") {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "invalid sidecar endpoint: path traversal `..` is forbidden",
            ));
        }
        if let Some(prefix) = base_path.strip_suffix("/v1/systemone/batch") {
            base_path = prefix.to_owned();
        } else if let Some(prefix) = base_path.strip_suffix("/v1/systemone") {
            base_path = prefix.to_owned();
        }

        let host_display = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.clone()
        };
        let canonical_url = if base_path.is_empty() {
            format!("http://{host_display}:{port}")
        } else {
            format!("http://{host_display}:{port}{base_path}")
        };

        Ok(Self {
            canonical_url,
            host,
            port,
            base_path,
        })
    }

    /// Formats single or batch path relative to `self.base_path`.
    #[must_use]
    pub fn request_path(&self, suffix: &str) -> String {
        if self.base_path.is_empty() {
            suffix.to_owned()
        } else {
            format!("{}{suffix}", self.base_path)
        }
    }

    /// Formats HTTP `Host` header value.
    #[must_use]
    pub fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn parse_authority(authority: &str) -> GapResult<(String, u16)> {
    if let Some(after_open) = authority.strip_prefix('[') {
        let close = after_open.find(']').ok_or_else(|| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                "invalid sidecar endpoint: unclosed IPv6 host bracket",
            )
        })?;
        let host = after_open[..close].to_ascii_lowercase();
        let tail = &after_open[close + 1..];
        let port = if tail.is_empty() {
            80
        } else if let Some(port_str) = tail.strip_prefix(':') {
            parse_port(port_str)?
        } else {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "invalid sidecar endpoint: malformed bracketed host/port",
            ));
        };
        return Ok((host, port));
    }

    match authority.rsplit_once(':') {
        Some((host, port_str)) if !host.contains(':') => {
            Ok((host.to_ascii_lowercase(), parse_port(port_str)?))
        }
        Some(_) => Err(GapError::new(
            ErrorCode::DiscoveryDegraded,
            "invalid sidecar endpoint: IPv6 literals must be bracketed `[::1]:port`",
        )),
        None => Ok((authority.to_ascii_lowercase(), 80)),
    }
}

fn parse_port(raw: &str) -> GapResult<u16> {
    let port: u16 = raw.parse().map_err(|_| {
        GapError::new(
            ErrorCode::DiscoveryDegraded,
            format!("invalid sidecar TCP port `{raw}`"),
        )
    })?;
    if port == 0 {
        return Err(GapError::new(
            ErrorCode::DiscoveryDegraded,
            "invalid sidecar TCP port `0`",
        ));
    }
    Ok(port)
}

/// Pluggable egress-guard hook invoked before any sidecar TCP connection is opened (`WP-M5-06`).
pub trait EgressGuardHook: Send + Sync + fmt::Debug {
    /// Validates `endpoint` and its resolved `SocketAddr` candidates prior to `connect`.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] when egress policy forbids the target host or IP address.
    fn check_endpoint(&self, endpoint: &ParsedEndpoint, resolved: &[SocketAddr]) -> GapResult<()>;
}

/// Fail-closed default egress guard blocking metadata IPs, unspecified/multicast ranges, private
/// RFC1918 subnets (by default), and loopback addresses unless explicitly enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultSidecarEgressGuard {
    /// Permit `127.0.0.0/8`, `::1`, and `localhost` for local sidecar daemons (`omni decide`).
    pub allow_loopback: bool,
    /// Permit private RFC1918 / ULA addresses (`10/8`, `172.16/12`, `192.168/16`, `fc00::/7`).
    pub allow_private: bool,
    /// Optional explicit host allow-list (lowercase exact match). Empty means allow non-blocked hosts.
    pub allowed_hosts: Vec<String>,
}

impl Default for DefaultSidecarEgressGuard {
    fn default() -> Self {
        Self::strict()
    }
}

impl DefaultSidecarEgressGuard {
    /// Strict production egress guard: blocks loopback, private subnets, link-local, and metadata IPs.
    #[must_use]
    pub const fn strict() -> Self {
        Self {
            allow_loopback: false,
            allow_private: false,
            allowed_hosts: Vec::new(),
        }
    }

    /// Guard permitting local loopback (`127.0.0.1` / `[::1]`) while blocking metadata and remote private CIDRs.
    #[must_use]
    pub const fn loopback_allowed() -> Self {
        Self {
            allow_loopback: true,
            allow_private: false,
            allowed_hosts: Vec::new(),
        }
    }

    /// Wraps this guard inside `Arc<dyn EgressGuardHook>`.
    #[must_use]
    pub fn into_hook(self) -> Arc<dyn EgressGuardHook> {
        Arc::new(self)
    }

    /// Checks whether `host` name itself violates static host policies before DNS lookup.
    pub fn check_host_name(&self, host: &str) -> GapResult<()> {
        let lower = host.trim_end_matches('.').to_ascii_lowercase();
        if !self.allowed_hosts.is_empty() && !self.allowed_hosts.iter().any(|h| h == &lower) {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar egress blocked: host `{lower}` not in allowed_hosts"),
            ));
        }

        if METADATA_HOSTS.iter().any(|blocked| *blocked == lower) || lower.ends_with(".internal") {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar egress blocked: metadata/internal host `{lower}` forbidden"),
            ));
        }

        if !self.allow_loopback && lower == "localhost" {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "sidecar egress blocked: loopback host `localhost` disallowed",
            ));
        }

        if let Ok(ip) = lower.parse::<IpAddr>() {
            return self.check_ip(ip);
        }

        Ok(())
    }

    /// Checks a resolved `IpAddr` against SSRF / metadata / private / loopback policies.
    pub fn check_ip(&self, ip: IpAddr) -> GapResult<()> {
        match ip {
            IpAddr::V4(v4) => self.check_ipv4(v4),
            IpAddr::V6(v6) => self.check_ipv6(v6),
        }
    }

    fn check_ipv4(&self, ip: Ipv4Addr) -> GapResult<()> {
        let [a, b, c, _d] = ip.octets();
        if ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar egress blocked: non-routable IPv4 `{ip}`"),
            ));
        }
        // Link-local 169.254.0.0/16 (AWS/GCP/Azure metadata 169.254.169.254) and CGNAT 100.64.0.0/10 (Alibaba 100.100.100.200).
        if (a == 169 && b == 254) || (a == 100 && (64..=127).contains(&b)) {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar egress blocked: link-local/metadata IPv4 `{ip}`"),
            ));
        }
        // Documentation / benchmark / reserved ranges.
        if (a == 192 && b == 0 && c == 2)
            || (a == 198 && b == 51 && c == 100)
            || (a == 203 && b == 0 && c == 113)
            || (a == 198 && (b == 18 || b == 19))
            || a >= 240
            || a == 0
        {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar egress blocked: reserved IPv4 `{ip}`"),
            ));
        }
        if ip.is_loopback() && !self.allow_loopback {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar egress blocked: loopback IPv4 `{ip}`"),
            ));
        }
        if ip.is_private() && !self.allow_private {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar egress blocked: private IPv4 `{ip}`"),
            ));
        }
        Ok(())
    }

    fn check_ipv6(&self, ip: Ipv6Addr) -> GapResult<()> {
        if let Some(mapped) = ip.to_ipv4_mapped() {
            return self.check_ipv4(mapped);
        }
        if ip.is_unspecified() || ip.is_multicast() {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar egress blocked: non-routable IPv6 `{ip}`"),
            ));
        }
        let segs = ip.segments();
        // Always block AWS IPv6 metadata `fd00:ec2::254` and link-local `fe80::/10`.
        if ip
            == "fd00:ec2::254"
                .parse::<Ipv6Addr>()
                .unwrap_or(Ipv6Addr::UNSPECIFIED)
            || (segs[0] & 0xffc0) == 0xfe80
        {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar egress blocked: link-local/metadata IPv6 `{ip}`"),
            ));
        }
        if ip.is_loopback() && !self.allow_loopback {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar egress blocked: loopback IPv6 `{ip}`"),
            ));
        }
        if ip.is_unique_local() && !self.allow_private {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar egress blocked: unique-local IPv6 `{ip}`"),
            ));
        }
        Ok(())
    }
}

impl EgressGuardHook for DefaultSidecarEgressGuard {
    fn check_endpoint(&self, endpoint: &ParsedEndpoint, resolved: &[SocketAddr]) -> GapResult<()> {
        self.check_host_name(&endpoint.host)?;
        if resolved.is_empty() {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!(
                    "sidecar egress blocked: host `{}` resolved zero addresses",
                    endpoint.host
                ),
            ));
        }
        for addr in resolved {
            self.check_ip(addr.ip())?;
        }
        Ok(())
    }
}

/// Resolves `endpoint` and runs `guard.check_endpoint`, returning pinned [`SocketAddr`]s ready to
/// dial (eliminating TOCTOU DNS rebinding between check and `connect`).
///
/// # Errors
///
/// Returns [`GapError`] on DNS failure, empty address set, or egress guard denial.
pub fn resolve_and_validate(
    endpoint: &ParsedEndpoint,
    guard: &dyn EgressGuardHook,
) -> GapResult<Vec<SocketAddr>> {
    if let Ok(ip) = endpoint.host.parse::<IpAddr>() {
        let resolved = vec![SocketAddr::new(ip, endpoint.port)];
        guard.check_endpoint(endpoint, &resolved)?;
        return Ok(resolved);
    }

    let lower_host = endpoint.host.trim_end_matches('.').to_ascii_lowercase();
    if METADATA_HOSTS.iter().any(|blocked| *blocked == lower_host)
        || lower_host.ends_with(".internal")
    {
        return Err(GapError::new(
            ErrorCode::DiscoveryDegraded,
            format!("sidecar egress blocked: metadata/internal host `{lower_host}` forbidden"),
        ));
    }

    let addrs: Vec<SocketAddr> = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!(
                    "failed to resolve sidecar host `{}:{}`: {err}",
                    endpoint.host, endpoint.port
                ),
            )
        })?
        .collect();
    guard.check_endpoint(endpoint, &addrs)?;
    Ok(addrs)
}

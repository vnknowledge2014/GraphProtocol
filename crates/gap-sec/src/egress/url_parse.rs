//! Strict outbound URL parser rejecting userinfo, IPv6 zones, odd IP encodings, and localhost/metadata hosts (`SEC-075`, `SEC-076`).

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// Transport scheme permitted by `EgressGuard`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scheme {
    /// Encrypted HTTPS (`443` default).
    Https,
    /// Cleartext HTTP (`80` default; permitted only for explicitly allow-listed local dev hosts).
    Http,
}

impl Scheme {
    /// Lowercase wire name (`"https"` or `"http"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Http => "http",
        }
    }

    /// Standard TCP port for the scheme.
    #[must_use]
    pub const fn default_port(self) -> u16 {
        match self {
            Self::Https => 443,
            Self::Http => 80,
        }
    }
}

/// Errors produced when validating an outbound URL before DNS resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlParseError {
    /// Input contains whitespace, ASCII control bytes, or backslash confusion.
    MalformedSyntax(String),
    /// Non-HTTP(S) scheme (`file:`, `gopher:`, `ftp:`, relative, etc.).
    UnsupportedScheme(String),
    /// Embedded credentials (`user@` or `user:pass@`) in authority (`SEC-075`).
    UserinfoDenied,
    /// IPv6 link-local or interface zone identifier (`%` or `%25`, `SEC-076`).
    Ipv6ZoneIdDenied,
    /// Decimal integer, octal, hex, or shorthand numeric IP literal (`SEC-076`).
    OddIpEncodingDenied(String),
    /// Prohibited host literal (`localhost`, `localhost.`, `metadata.google.internal`, `*.internal`).
    BlockedHostname(String),
    /// Invalid TCP port outside `1..=65535`.
    InvalidPort(String),
}

impl fmt::Display for UrlParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedSyntax(msg) => write!(f, "malformed outbound URL: {msg}"),
            Self::UnsupportedScheme(s) => write!(f, "unsupported outbound URL scheme `{s}`"),
            Self::UserinfoDenied => {
                f.write_str("userinfo (`user:pass@`) is forbidden in outbound URLs")
            }
            Self::Ipv6ZoneIdDenied => {
                f.write_str("IPv6 zone identifiers are forbidden in outbound URLs")
            }
            Self::OddIpEncodingDenied(h) => {
                write!(f, "non-canonical numeric IP encoding `{h}` is forbidden")
            }
            Self::BlockedHostname(h) => write!(f, "prohibited internal/metadata hostname `{h}`"),
            Self::InvalidPort(p) => write!(f, "invalid outbound TCP port `{p}`"),
        }
    }
}

impl std::error::Error for UrlParseError {}

/// Validated URL components ready for policy checking and DNS resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTargetUrl {
    /// Original parsed scheme.
    pub scheme: Scheme,
    /// Host token extracted from authority (without brackets for IPv6).
    pub raw_host: String,
    /// Lowercase hostname with any trailing FQDN dot removed (or canonical IP string).
    pub normalized_host: String,
    /// Parsed literal IP when authority specified a canonical IPv4 or IPv6 literal.
    pub literal_ip: Option<IpAddr>,
    /// Effective TCP port.
    pub port: u16,
    /// Authority string (`host` or `host:port` suitable for HTTP `Host:` / SNI formatting).
    pub host_header: String,
    /// Path and query suffix starting with `/` (always non-empty).
    pub path_and_query: String,
    /// Whether `normalized_host` matches an internal/localhost/metadata hostname blocklist.
    pub blocked_internal_hostname: bool,
}

/// Parses and canonicalizes an outbound HTTP(S) target URL while blocking parser-confusion and
/// odd-IP SSRF tricks (`SEC-075`, `SEC-076`).
///
/// # Errors
///
/// Returns [`UrlParseError`] when the syntax, scheme, authority, or hostname violates egress rules.
pub fn parse_target_url(raw: &str) -> Result<ParsedTargetUrl, UrlParseError> {
    if raw.is_empty() {
        return Err(UrlParseError::MalformedSyntax("URL is empty".to_owned()));
    }
    if raw.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return Err(UrlParseError::MalformedSyntax(
            "URL contains whitespace or ASCII control characters".to_owned(),
        ));
    }

    let (scheme_raw, rest) = raw
        .split_once("://")
        .ok_or_else(|| UrlParseError::UnsupportedScheme(raw.to_owned()))?;
    let scheme = match scheme_raw.to_ascii_lowercase().as_str() {
        "https" => Scheme::Https,
        "http" => Scheme::Http,
        other => return Err(UrlParseError::UnsupportedScheme(other.to_owned())),
    };

    // Strip fragment first so `@` / `/` inside a fragment cannot spoof authority parsing, then
    // locate authority end at `/`, `?`, or `\`. Backslashes inside authority are rejected outright.
    let without_frag = rest.split('#').next().unwrap_or(rest);
    let auth_end = without_frag.find(['/', '?']).unwrap_or(without_frag.len());
    let authority = &without_frag[..auth_end];
    let suffix = &without_frag[auth_end..];

    if authority.is_empty() {
        return Err(UrlParseError::MalformedSyntax(
            "missing URL authority".to_owned(),
        ));
    }
    if authority.contains('\\') {
        return Err(UrlParseError::MalformedSyntax(
            "backslash in URL authority is forbidden".to_owned(),
        ));
    }
    if authority.contains('@') {
        return Err(UrlParseError::UserinfoDenied);
    }

    let (raw_host, port, is_ipv6_bracketed) = parse_authority(authority, scheme)?;
    let lower_raw = raw_host.to_ascii_lowercase();

    // Reject trailing-dot tricks explicitly for localhost and dotted numeric literals, while
    // normalizing single trailing dot only after rejecting dangerous variants.
    if lower_raw.contains('%') {
        if is_ipv6_bracketed {
            return Err(UrlParseError::Ipv6ZoneIdDenied);
        }
        return Err(UrlParseError::MalformedSyntax(
            "percent-encoding is forbidden in outbound hostnames".to_owned(),
        ));
    }

    if raw_host.ends_with("..") {
        return Err(UrlParseError::MalformedSyntax(
            "multiple trailing dots in hostname".to_owned(),
        ));
    }

    let had_trailing_dot = lower_raw.ends_with('.');
    let normalized_host = lower_raw.trim_end_matches('.').to_owned();
    if normalized_host.is_empty() {
        return Err(UrlParseError::MalformedSyntax("empty hostname".to_owned()));
    }

    if !is_ipv6_bracketed && had_trailing_dot && is_dangerous_trailing_dot_host(&normalized_host) {
        return Err(UrlParseError::BlockedHostname(raw_host));
    }

    let literal_ip = if is_ipv6_bracketed {
        let v6 = Ipv6Addr::from_str(&normalized_host).map_err(|_| {
            UrlParseError::MalformedSyntax(format!("invalid IPv6 literal `[{normalized_host}]`"))
        })?;
        Some(IpAddr::V6(v6))
    } else if looks_like_numeric_ip_attempt(&normalized_host) {
        let v4 = parse_strict_canonical_ipv4(&normalized_host)
            .ok_or_else(|| UrlParseError::OddIpEncodingDenied(raw_host.clone()))?;
        if had_trailing_dot {
            return Err(UrlParseError::OddIpEncodingDenied(raw_host));
        }
        Some(IpAddr::V4(v4))
    } else {
        None
    };

    let blocked_internal_hostname =
        literal_ip.is_none() && is_blocked_internal_hostname(&normalized_host);

    let host_header = if is_ipv6_bracketed {
        if port == scheme.default_port() {
            format!("[{normalized_host}]")
        } else {
            format!("[{normalized_host}]:{port}")
        }
    } else if port == scheme.default_port() {
        normalized_host.clone()
    } else {
        format!("{normalized_host}:{port}")
    };

    let path_and_query = if suffix.is_empty() {
        "/".to_owned()
    } else if suffix.starts_with('?') {
        format!("/{suffix}")
    } else {
        suffix.to_owned()
    };

    Ok(ParsedTargetUrl {
        scheme,
        raw_host,
        normalized_host,
        literal_ip,
        port,
        host_header,
        path_and_query,
        blocked_internal_hostname,
    })
}

/// Resolves an HTTP `Location` header value relative to `base` before re-running `EgressGuard`.
///
/// # Errors
///
/// Returns [`UrlParseError`] when the resulting redirect target is invalid or uses an unsafe scheme.
pub fn resolve_redirect_url(
    base: &ParsedTargetUrl,
    location: &str,
) -> Result<String, UrlParseError> {
    let trimmed = location.trim();
    if trimmed.is_empty() {
        return Err(UrlParseError::MalformedSyntax(
            "empty redirect Location header".to_owned(),
        ));
    }
    if trimmed.contains("://") {
        return Ok(trimmed.to_owned());
    }
    if let Some(after_slashes) = trimmed.strip_prefix("//") {
        return Ok(format!("{}://{after_slashes}", base.scheme.as_str()));
    }
    if trimmed.starts_with('/') {
        return Ok(format!(
            "{}://{}{trimmed}",
            base.scheme.as_str(),
            base.host_header
        ));
    }
    let base_path = base.path_and_query.split('?').next().unwrap_or("/");
    let dir = match base_path.rfind('/') {
        Some(idx) => &base_path[..=idx],
        None => "/",
    };
    Ok(format!(
        "{}://{}{dir}{trimmed}",
        base.scheme.as_str(),
        base.host_header
    ))
}

fn parse_authority(authority: &str, scheme: Scheme) -> Result<(String, u16, bool), UrlParseError> {
    if let Some(after_open) = authority.strip_prefix('[') {
        let Some(close_idx) = after_open.find(']') else {
            return Err(UrlParseError::MalformedSyntax(
                "unclosed `[` in IPv6 authority".to_owned(),
            ));
        };
        let host_inside = &after_open[..close_idx];
        if host_inside.contains('%') {
            return Err(UrlParseError::Ipv6ZoneIdDenied);
        }
        let remainder = &after_open[close_idx + 1..];
        let port = if remainder.is_empty() {
            scheme.default_port()
        } else if let Some(port_str) = remainder.strip_prefix(':') {
            parse_port(port_str)?
        } else {
            return Err(UrlParseError::MalformedSyntax(
                "unexpected characters after IPv6 `]`".to_owned(),
            ));
        };
        return Ok((host_inside.to_owned(), port, true));
    }

    if authority.contains('[') || authority.contains(']') {
        return Err(UrlParseError::MalformedSyntax(
            "stray IPv6 bracket in authority".to_owned(),
        ));
    }

    match authority.rsplit_once(':') {
        Some((host, port_str)) => {
            if host.contains(':') {
                return Err(UrlParseError::MalformedSyntax(
                    "unbracketed colons in URL host".to_owned(),
                ));
            }
            let port = parse_port(port_str)?;
            Ok((host.to_owned(), port, false))
        }
        None => Ok((authority.to_owned(), scheme.default_port(), false)),
    }
}

fn parse_port(port_str: &str) -> Result<u16, UrlParseError> {
    if port_str.is_empty() || !port_str.bytes().all(|b| b.is_ascii_digit()) {
        return Err(UrlParseError::InvalidPort(port_str.to_owned()));
    }
    let port: u16 = port_str
        .parse()
        .map_err(|_| UrlParseError::InvalidPort(port_str.to_owned()))?;
    if port == 0 {
        return Err(UrlParseError::InvalidPort(port_str.to_owned()));
    }
    Ok(port)
}

fn is_dangerous_trailing_dot_host(normalized_host: &str) -> bool {
    is_blocked_internal_hostname(normalized_host) || looks_like_numeric_ip_attempt(normalized_host)
}

/// Checks reserved internal/metadata hostnames regardless of trailing dot normalization.
#[must_use]
pub fn is_blocked_internal_hostname(normalized_host: &str) -> bool {
    let h = normalized_host.trim_end_matches('.').to_ascii_lowercase();
    h == "localhost"
        || h.ends_with(".localhost")
        || h == "localhost.localdomain"
        || h == "ip6-localhost"
        || h == "ip6-loopback"
        || h == "metadata"
        || h == "metadata.google.internal"
        || h == "instance-data"
        || h.ends_with(".internal")
        || h.ends_with(".local")
}

/// Detects attempts to encode an IPv4 address using decimal, octal, hex, or shorthand forms
/// (e.g. `2130706433`, `0177.0.0.1`, `0x7f000001`, `0x7f.0.0.1`, `127.1`, `127.0.1`).
fn looks_like_numeric_ip_attempt(host: &str) -> bool {
    if host.is_empty() || host.contains("..") {
        return false;
    }
    let parts: Vec<&str> = host.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return false;
    }
    parts.iter().all(|part| is_numeric_ip_label(part))
}

fn is_numeric_ip_label(part: &str) -> bool {
    if part.is_empty() {
        return false;
    }
    if let Some(hex_body) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
        return !hex_body.is_empty() && hex_body.bytes().all(|b| b.is_ascii_hexdigit());
    }
    part.bytes().all(|b| b.is_ascii_digit())
}

/// Accepts ONLY canonical dotted-quad decimal IPv4 without leading zeros (`0`..=`255`).
fn parse_strict_canonical_ipv4(host: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0_u8; 4];
    for (idx, part) in parts.iter().enumerate() {
        if part.is_empty() || (part.len() > 1 && part.starts_with('0')) {
            return None;
        }
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        octets[idx] = part.parse::<u8>().ok()?;
    }
    Some(Ipv4Addr::from(octets))
}

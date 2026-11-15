//! Core DNS-pinning `EgressGuard`, redirect validator, connector adapters, and self-test (`SEC-075`, `SEC-076`, `SEC-072`).

use std::fmt;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use gap_core::error::{ErrorCode, GapError};

use super::ip::{BlockedIpReason, CidrBlock, classify_blocked_ip};
use super::url_parse::{
    ParsedTargetUrl, Scheme, UrlParseError, parse_target_url, resolve_redirect_url,
};

/// Maximum HTTP redirects permitted by default (`SEC-075`).
pub const DEFAULT_MAX_REDIRECTS: usize = 3;

/// Default TCP dial timeout applied by [`PinnedEndpoint::connect_tcp`].
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Branch-level `egress.allow[]` exception entry (`SEC-075`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowRule {
    /// Exact normalized DNS hostname (e.g. `"internal.svc.local"` or `"localhost"` for local dev).
    Host(String),
    /// Explicit CIDR prefix exception (e.g. `10.42.0.0/16`).
    Cidr(CidrBlock),
}

impl AllowRule {
    /// Parses a branch `egress.allow[]` rule string as either a CIDR block or normalized hostname.
    ///
    /// # Errors
    ///
    /// Returns [`EgressError::InvalidPolicy`] when `raw` is empty, malformed CIDR, or contains URL syntax.
    pub fn parse(raw: &str) -> Result<Self, EgressError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(EgressError::InvalidPolicy(
                "empty branch egress.allow[] entry".to_owned(),
            ));
        }
        if trimmed.contains('/') || trimmed.parse::<IpAddr>().is_ok() {
            let cidr = CidrBlock::parse(trimmed).map_err(EgressError::InvalidPolicy)?;
            return Ok(Self::Cidr(cidr));
        }
        if trimmed.contains("://") || trimmed.contains('@') || trimmed.contains(' ') {
            return Err(EgressError::InvalidPolicy(format!(
                "invalid host in branch egress.allow[]: `{trimmed}`"
            )));
        }
        Ok(Self::Host(
            trimmed.trim_end_matches('.').to_ascii_lowercase(),
        ))
    }
}

/// Policy configuration governing [`EgressGuard`] checks (`SEC-075`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressPolicy {
    /// Explicit per-branch `egress.allow[]` host or CIDR overrides.
    pub allow_rules: Vec<AllowRule>,
    /// Hosts explicitly permitted to use cleartext `http://` (typically local dev hosts).
    pub allow_insecure_http_hosts: Vec<String>,
    /// Maximum HTTP redirect hops followed before aborting (`<= 3` enforced).
    pub max_redirects: usize,
    /// Timeout applied when dialing pinned socket addresses.
    pub connect_timeout: Duration,
}

impl Default for EgressPolicy {
    fn default() -> Self {
        Self {
            allow_rules: Vec::new(),
            allow_insecure_http_hosts: Vec::new(),
            max_redirects: DEFAULT_MAX_REDIRECTS,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        }
    }
}

impl EgressPolicy {
    /// Returns a policy that allows `http://` only for explicitly listed local development hosts.
    #[must_use]
    pub fn with_local_dev_http_host(mut self, host: impl Into<String>) -> Self {
        let normalized = host
            .into()
            .trim()
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if !normalized.is_empty() {
            self.allow_rules.push(AllowRule::Host(normalized.clone()));
            self.allow_insecure_http_hosts.push(normalized);
        }
        self
    }

    /// Adds a parsed `egress.allow[]` override rule (host or CIDR).
    ///
    /// # Errors
    ///
    /// Returns [`EgressError::InvalidPolicy`] when `rule` cannot be parsed.
    pub fn add_allow_entry(&mut self, rule: &str) -> Result<(), EgressError> {
        self.allow_rules.push(AllowRule::parse(rule)?);
        Ok(())
    }

    fn host_is_explicitly_allowed(&self, normalized_host: &str) -> bool {
        self.allow_rules.iter().any(|rule| match rule {
            AllowRule::Host(h) => h == normalized_host,
            AllowRule::Cidr(_) => false,
        })
    }

    fn ip_is_explicitly_allowed(&self, ip: IpAddr, normalized_host: &str) -> bool {
        self.allow_rules.iter().any(|rule| match rule {
            AllowRule::Host(h) => h == normalized_host,
            AllowRule::Cidr(cidr) => cidr.contains(ip),
        })
    }

    fn insecure_http_allowed_for(&self, normalized_host: &str) -> bool {
        self.allow_insecure_http_hosts.iter().any(|h| {
            h.trim_end_matches('.')
                .eq_ignore_ascii_case(normalized_host)
        })
    }

    const fn effective_max_redirects(&self) -> usize {
        if self.max_redirects > DEFAULT_MAX_REDIRECTS {
            DEFAULT_MAX_REDIRECTS
        } else {
            self.max_redirects
        }
    }
}

/// Errors returned by [`EgressGuard`] when an outbound dial violates SSRF policy (`SEC-075`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressError {
    /// Invalid branch `EgressPolicy` entry.
    InvalidPolicy(String),
    /// Outbound URL syntax failed pre-resolution hardening rules.
    InvalidUrl(UrlParseError),
    /// Cleartext `http://` was denied because the host is not explicitly allow-listed for local dev.
    InsecureSchemeDenied {
        /// Target URL host.
        host: String,
    },
    /// Internal/metadata hostname was denied and not present in `egress.allow[]`.
    BlockedHostname {
        /// Hostname that failed policy checks.
        host: String,
    },
    /// DNS lookup failed or returned zero addresses.
    DnsResolutionFailed {
        /// Hostname that could not be resolved.
        host: String,
        /// Underlying resolver message.
        reason: String,
    },
    /// A resolved (or literal) IP address matched a blocked SSRF destination range.
    BlockedIp {
        /// Target hostname or literal.
        host: String,
        /// Blocked socket address returned by resolution.
        addr: IpAddr,
        /// Classification reason.
        reason: BlockedIpReason,
    },
    /// HTTP redirect count exceeded the configured cap (`<= 3`).
    TooManyRedirects {
        /// Maximum hops allowed.
        limit: usize,
    },
    /// HTTP redirect attempted to downgrade `https://` to `http://`.
    SchemeDowngradeDenied {
        /// Redirect destination URL.
        location: String,
    },
    /// TCP connection to every pinned address failed.
    ConnectFailed(String),
    /// Built-in `EgressGuard::self_test()` invariant check failed (`SEC-072`).
    SelfTestFailed(String),
}

impl EgressError {
    /// Converts an egress policy/transport failure into a gateway [`GapError`].
    #[must_use]
    pub fn into_gap_error(self) -> GapError {
        let code = match &self {
            Self::DnsResolutionFailed { .. } | Self::ConnectFailed(_) => {
                ErrorCode::BranchUnavailable
            }
            Self::InvalidPolicy(_) | Self::InvalidUrl(_) => ErrorCode::ValidationError,
            Self::InsecureSchemeDenied { .. }
            | Self::BlockedHostname { .. }
            | Self::BlockedIp { .. }
            | Self::TooManyRedirects { .. }
            | Self::SchemeDowngradeDenied { .. } => ErrorCode::Forbidden,
            Self::SelfTestFailed(_) => ErrorCode::Internal,
        };
        GapError::new(code, self.to_string())
    }
}

impl fmt::Display for EgressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPolicy(msg) => write!(f, "invalid egress policy: {msg}"),
            Self::InvalidUrl(err) => err.fmt(f),
            Self::InsecureSchemeDenied { host } => {
                write!(f, "cleartext http:// scheme denied for `{host}`")
            }
            Self::BlockedHostname { host } => {
                write!(f, "egress blocked internal/metadata hostname `{host}`")
            }
            Self::DnsResolutionFailed { host, reason } => {
                write!(f, "DNS resolution failed for `{host}`: {reason}")
            }
            Self::BlockedIp { host, addr, reason } => {
                write!(f, "egress blocked `{host}` resolved to {addr} ({reason})")
            }
            Self::TooManyRedirects { limit } => {
                write!(f, "outbound redirect count exceeded maximum of {limit}")
            }
            Self::SchemeDowngradeDenied { location } => {
                write!(
                    f,
                    "redirect downgrade from https:// to http:// denied ({location})"
                )
            }
            Self::ConnectFailed(msg) => {
                write!(f, "connect to pinned egress endpoint failed: {msg}")
            }
            Self::SelfTestFailed(msg) => write!(f, "egress guard self-test failed: {msg}"),
        }
    }
}

impl std::error::Error for EgressError {}

/// Synchronous DNS resolution interface abstracted so DNS rebinding can be deterministically tested (`SEC-076`).
pub trait DnsResolver: Send + Sync {
    /// Resolves `(host, port)` into concrete socket addresses.
    ///
    /// # Errors
    ///
    /// Returns a human-readable reason string if resolution fails.
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String>;
}

/// Standard OS resolver using `std::net::ToSocketAddrs`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdDnsResolver;

impl DnsResolver for StdDnsResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        let iter = (host, port)
            .to_socket_addrs()
            .map_err(|err| err.to_string())?;
        Ok(iter.collect())
    }
}

/// Validated, DNS-pinned outbound endpoint produced by [`EgressGuard::check_and_pin`] (`SEC-075`).
///
/// Downstream connectors (`reqwest`/`hyper`/HTTP/gRPC/`SourceFetcher`/`HttpSidecar`) **MUST**
/// dial [`Self::pinned_addrs`] directly without performing another DNS lookup while using
/// [`Self::tls_sni_host`] for TLS certificate validation and [`Self::host_header`] for HTTP `Host`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedEndpoint {
    /// Original URL passed to `EgressGuard`.
    pub original_url: String,
    /// Validated transport scheme (`https` or allow-listed local dev `http`).
    pub scheme: Scheme,
    /// Lowercase hostname used for TLS SNI and server certificate verification.
    pub tls_sni_host: String,
    /// Normalized HTTP `Host` header value (`host` or `host:port`).
    pub host_header: String,
    /// Effective TCP port.
    pub port: u16,
    /// All DNS-pinned socket addresses validated against SSRF policy.
    pub pinned_addrs: Vec<SocketAddr>,
    /// Canonical path + query string (`/` if empty).
    pub path_and_query: String,
}

impl PinnedEndpoint {
    /// Returns the first pinned socket address validated by `EgressGuard`.
    #[must_use]
    pub fn primary_addr(&self) -> SocketAddr {
        self.pinned_addrs[0]
    }

    /// Returns all pinned socket addresses in resolution order.
    #[must_use]
    pub fn dial_targets(&self) -> &[SocketAddr] {
        &self.pinned_addrs
    }

    /// Opens a TCP connection strictly to the pinned IP addresses without re-resolving DNS (`SEC-075`).
    ///
    /// # Errors
    ///
    /// Returns [`EgressError::ConnectFailed`] if all pinned addresses fail to connect within `timeout`.
    pub fn connect_tcp(&self, timeout: Duration) -> Result<TcpStream, EgressError> {
        let mut last_err = String::new();
        for addr in &self.pinned_addrs {
            match TcpStream::connect_timeout(addr, timeout) {
                Ok(stream) => return Ok(stream),
                Err(err) => {
                    last_err = format!("{addr}: {err}");
                }
            }
        }
        Err(EgressError::ConnectFailed(last_err))
    }
}

/// Redirect step outcome returned by caller transport hooks inside [`EgressGuard::follow_redirects`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HopOutcome<T> {
    /// Terminal response returned by the pinned endpoint.
    Final(T),
    /// HTTP 3xx redirect pointing to `location` (absolute or relative `Location` header value).
    Redirect {
        /// Redirect target URI reference.
        location: String,
    },
}

/// DNS-pinning SSRF egress guard (`SEC-075`, `SEC-076`, `SEC-072`).
#[derive(Debug, Clone)]
pub struct EgressGuard<R = StdDnsResolver> {
    policy: EgressPolicy,
    resolver: R,
}

impl Default for EgressGuard<StdDnsResolver> {
    fn default() -> Self {
        Self::new(EgressPolicy::default())
    }
}

impl EgressGuard<StdDnsResolver> {
    /// Creates an [`EgressGuard`] backed by [`StdDnsResolver`].
    #[must_use]
    pub fn new(policy: EgressPolicy) -> Self {
        Self {
            policy,
            resolver: StdDnsResolver,
        }
    }

    /// Executes deterministic SSRF + DNS rebinding self-tests for `gap doctor` (`SEC-072`).
    ///
    /// # Errors
    ///
    /// Returns [`EgressError::SelfTestFailed`] if any built-in check regresses.
    pub fn self_test() -> Result<(), EgressError> {
        let mut guard = EgressGuard::with_resolver(
            EgressPolicy::default(),
            SequenceResolver::new(vec![
                vec![SocketAddr::from(([93, 184, 216, 34], 443))],
                vec![SocketAddr::from(([127, 0, 0, 1], 443))],
            ]),
        );

        let pinned = guard
            .check_and_pin("https://api.example.com/v1/schema.json")
            .map_err(|err| {
                EgressError::SelfTestFailed(format!("valid public URL failed: {err}"))
            })?;
        if pinned.primary_addr() != SocketAddr::from(([93, 184, 216, 34], 443)) {
            return Err(EgressError::SelfTestFailed(
                "pinned address did not retain initial public resolution".to_owned(),
            ));
        }

        // Second lookup simulates DNS rebinding to loopback -> must fail fail-closed.
        if guard
            .check_and_pin("https://api.example.com/v1/schema.json")
            .is_ok()
        {
            return Err(EgressError::SelfTestFailed(
                "DNS rebinding to loopback was not blocked".to_owned(),
            ));
        }
        guard = EgressGuard::with_resolver(EgressPolicy::default(), SequenceResolver::empty());

        for denied in [
            "https://169.254.169.254/latest/meta-data/",
            "https://[fd00:ec2::254]/latest/meta-data/",
            "https://[::ffff:169.254.169.254]/",
            "https://metadata.google.internal/computeMetadata/v1/",
            "https://2130706433/",
            "https://0177.0.0.1/",
            "https://0x7f000001/",
            "https://[fe80::1%25lo0]/",
            "https://localhost./",
            "https://user:pass@example.com/",
            "http://example.com/",
        ] {
            if guard.check_and_pin(denied).is_ok() {
                return Err(EgressError::SelfTestFailed(format!(
                    "hostile SSRF vector `{denied}` was unexpectedly allowed"
                )));
            }
        }
        Ok(())
    }
}

impl<R: DnsResolver> EgressGuard<R> {
    /// Creates an [`EgressGuard`] with a custom [`DnsResolver`] (e.g. for deterministic tests).
    #[must_use]
    pub const fn with_resolver(policy: EgressPolicy, resolver: R) -> Self {
        Self { policy, resolver }
    }

    /// Returns the active [`EgressPolicy`].
    #[must_use]
    pub const fn policy(&self) -> &EgressPolicy {
        &self.policy
    }

    /// Validates `url`, resolves DNS once, checks EVERY returned IP, and pins socket targets (`SEC-075`).
    ///
    /// # Errors
    ///
    /// Returns [`EgressError`] on syntax, scheme, blocked host, DNS failure, or blocked IP violation.
    pub fn check_and_pin(&self, url: &str) -> Result<PinnedEndpoint, EgressError> {
        let parsed = parse_target_url(url).map_err(EgressError::InvalidUrl)?;
        self.check_parsed_and_pin(url, &parsed)
    }

    fn check_parsed_and_pin(
        &self,
        original_url: &str,
        parsed: &ParsedTargetUrl,
    ) -> Result<PinnedEndpoint, EgressError> {
        if parsed.scheme == Scheme::Http
            && !self
                .policy
                .insecure_http_allowed_for(&parsed.normalized_host)
        {
            return Err(EgressError::InsecureSchemeDenied {
                host: parsed.normalized_host.clone(),
            });
        }

        let host_allowed = self
            .policy
            .host_is_explicitly_allowed(&parsed.normalized_host);
        if parsed.blocked_internal_hostname && !host_allowed {
            return Err(EgressError::BlockedHostname {
                host: parsed.raw_host.clone(),
            });
        }

        let resolved: Vec<SocketAddr> = match parsed.literal_ip {
            Some(ip) => vec![SocketAddr::new(ip, parsed.port)],
            None => self
                .resolver
                .resolve(&parsed.normalized_host, parsed.port)
                .map_err(|reason| EgressError::DnsResolutionFailed {
                    host: parsed.normalized_host.clone(),
                    reason,
                })?,
        };

        if resolved.is_empty() {
            return Err(EgressError::DnsResolutionFailed {
                host: parsed.normalized_host.clone(),
                reason: "resolver returned 0 addresses".to_owned(),
            });
        }

        for addr in &resolved {
            let ip = addr.ip();
            if self
                .policy
                .ip_is_explicitly_allowed(ip, &parsed.normalized_host)
            {
                continue;
            }
            if let Some(reason) = classify_blocked_ip(ip) {
                return Err(EgressError::BlockedIp {
                    host: parsed.normalized_host.clone(),
                    addr: ip,
                    reason,
                });
            }
        }

        Ok(PinnedEndpoint {
            original_url: original_url.to_owned(),
            scheme: parsed.scheme,
            tls_sni_host: parsed.normalized_host.clone(),
            host_header: parsed.host_header.clone(),
            port: parsed.port,
            pinned_addrs: resolved,
            path_and_query: parsed.path_and_query.clone(),
        })
    }

    /// Executes an HTTP hop callback with redirect limit (`<= 3`), scheme-downgrade rejection,
    /// and full `check_and_pin` re-validation on every redirect target (`SEC-075`).
    ///
    /// # Errors
    ///
    /// Returns [`EgressError`] if any hop fails validation, downgrades `https` to `http`, or
    /// exceeds the redirect limit.
    pub fn follow_redirects<F, T>(
        &self,
        initial_url: &str,
        mut perform_hop: F,
    ) -> Result<(PinnedEndpoint, T), EgressError>
    where
        F: FnMut(&PinnedEndpoint) -> Result<HopOutcome<T>, EgressError>,
    {
        let max_redirects = self.policy.effective_max_redirects();
        let mut current_url = initial_url.to_owned();
        let mut seen_https = false;

        for hop_idx in 0..=max_redirects {
            let parsed = parse_target_url(&current_url).map_err(EgressError::InvalidUrl)?;
            if seen_https && parsed.scheme == Scheme::Http {
                return Err(EgressError::SchemeDowngradeDenied {
                    location: current_url,
                });
            }
            if parsed.scheme == Scheme::Https {
                seen_https = true;
            }
            let pinned = self.check_parsed_and_pin(&current_url, &parsed)?;
            match perform_hop(&pinned)? {
                HopOutcome::Final(value) => return Ok((pinned, value)),
                HopOutcome::Redirect { location } => {
                    if hop_idx >= max_redirects {
                        return Err(EgressError::TooManyRedirects {
                            limit: max_redirects,
                        });
                    }
                    let next_url = resolve_redirect_url(&parsed, &location)
                        .map_err(EgressError::InvalidUrl)?;
                    current_url = next_url;
                }
            }
        }

        Err(EgressError::TooManyRedirects {
            limit: max_redirects,
        })
    }
}

/// Connector/fetcher/enricher adapter wrapping [`EgressGuard`] for HTTP(S) clients (`reqwest`/`hyper`/`SourceFetcher`/`HttpSidecar`).
#[derive(Debug, Clone)]
pub struct EgressHttpAdapter<R = StdDnsResolver> {
    guard: EgressGuard<R>,
}

impl<R: DnsResolver> EgressHttpAdapter<R> {
    /// Wraps an [`EgressGuard`] for HTTP/REST/OpenAPI/GraphQL/MCP/`SourceFetcher`/`HttpSidecar` callers.
    #[must_use]
    pub const fn new(guard: EgressGuard<R>) -> Self {
        Self { guard }
    }

    /// Validates an HTTP endpoint or spec source URL and returns a DNS-pinned dial descriptor.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] (`ErrorCode::Forbidden` / `ValidationError` / `BranchUnavailable`) on denial.
    pub fn prepare_request(&self, url: &str) -> Result<PinnedEndpoint, GapError> {
        self.guard
            .check_and_pin(url)
            .map_err(EgressError::into_gap_error)
    }

    /// Runs a multi-hop HTTP flow (`<= 3` redirects, no `https` -> `http` downgrade, re-check every hop).
    ///
    /// # Errors
    ///
    /// Returns [`EgressError`] if any hop or redirect violates policy.
    pub fn execute_with_redirects<F, T>(
        &self,
        initial_url: &str,
        perform_hop: F,
    ) -> Result<(PinnedEndpoint, T), EgressError>
    where
        F: FnMut(&PinnedEndpoint) -> Result<HopOutcome<T>, EgressError>,
    {
        self.guard.follow_redirects(initial_url, perform_hop)
    }
}

/// Connector adapter wrapping [`EgressGuard`] for gRPC/A2A outbound channel endpoints (`SEC-075`).
#[derive(Debug, Clone)]
pub struct EgressGrpcAdapter<R = StdDnsResolver> {
    guard: EgressGuard<R>,
}

impl<R: DnsResolver> EgressGrpcAdapter<R> {
    /// Wraps an [`EgressGuard`] for gRPC / A2A connector endpoint pinning.
    #[must_use]
    pub const fn new(guard: EgressGuard<R>) -> Self {
        Self { guard }
    }

    /// Validates a gRPC endpoint URL and returns the pinned socket addresses + TLS SNI host.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if the gRPC endpoint resolves to a forbidden target or uses an invalid URL.
    pub fn prepare_channel_target(&self, endpoint_url: &str) -> Result<PinnedEndpoint, GapError> {
        self.guard
            .check_and_pin(endpoint_url)
            .map_err(EgressError::into_gap_error)
    }
}

/// Deterministic scripted resolver for `EgressGuard::self_test()` and SSRF rebinding suites (`SEC-076`).
#[derive(Debug)]
pub struct SequenceResolver {
    responses: Vec<Vec<SocketAddr>>,
    cursor: AtomicUsize,
}

impl SequenceResolver {
    /// Constructs a scripted sequence resolver yielding one response entry per lookup call.
    #[must_use]
    pub fn new(responses: Vec<Vec<SocketAddr>>) -> Self {
        Self {
            responses,
            cursor: AtomicUsize::new(0),
        }
    }

    fn empty() -> Self {
        Self::new(Vec::new())
    }
}

impl DnsResolver for SequenceResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        let idx = self.cursor.fetch_add(1, Ordering::AcqRel);
        match self.responses.get(idx) {
            Some(addrs) => Ok(addrs
                .iter()
                .map(|addr| SocketAddr::new(addr.ip(), port))
                .collect()),
            None => Err(format!("no scripted DNS entry for `{host}` lookup #{idx}")),
        }
    }
}

/// Convenience shared-resolver wrapper for Arc-based mock resolvers in tests/callers.
impl<T: DnsResolver + ?Sized> DnsResolver for Arc<T> {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        (**self).resolve(host, port)
    }
}

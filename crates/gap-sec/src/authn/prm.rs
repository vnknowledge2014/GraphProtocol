//! RFC 9728 Protected Resource Metadata (`SEC-004`) and constant-shape failure rate limiter (`SEC-003`).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;

use gap_core::error::{ErrorCode, GapError, GapResult};

use super::json::{JsonValue, write_json_escaped};

/// Canonical RFC 9728 well-known HTTP path (`SEC-004`).
pub const PRM_WELL_KNOWN_PATH: &str = "/.well-known/oauth-protected-resource";

/// Canonical RFC 9728 MCP resource well-known HTTP path (`SEC-004`).
pub const PRM_MCP_WELL_KNOWN_PATH: &str = "/.well-known/oauth-protected-resource/mcp";

/// Constant wire message returned for all authentication failures to prevent fingerprinting (`SEC-003`).
pub const CONSTANT_AUTH_FAILURE_MESSAGE: &str = "authentication required";

/// RFC 9728 OAuth 2.0 Protected Resource Metadata document (`SEC-004`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedResourceMetadata {
    /// Canonical RFC 8707 / RFC 9728 resource identifier URI (e.g. `https://gap.example.com/mcp`).
    pub resource: String,
    /// Trusted authorization server issuer URLs.
    pub authorization_servers: Vec<String>,
    /// Scopes understood by this GAP resource server.
    pub scopes_supported: Vec<String>,
    /// Supported bearer methods (`["header"]`).
    pub bearer_methods_supported: Vec<String>,
    /// Optional URL to human documentation.
    pub resource_documentation: Option<String>,
    /// Derived metadata document URL used in HTTP `401 WWW-Authenticate`.
    pub metadata_url: String,
}

impl ProtectedResourceMetadata {
    /// Creates a minimal RFC 9728 PRM document defaulting `bearer_methods_supported` to `["header"]`.
    #[must_use]
    pub fn new(
        resource: impl Into<String>,
        authorization_servers: Vec<String>,
        metadata_url: impl Into<String>,
    ) -> Self {
        Self {
            resource: resource.into(),
            authorization_servers,
            scopes_supported: Vec::new(),
            bearer_methods_supported: vec!["header".to_owned()],
            resource_documentation: None,
            metadata_url: metadata_url.into(),
        }
    }

    /// Serves the exact RFC 9728 JSON document when `path` matches [`PRM_WELL_KNOWN_PATH`] or
    /// [`PRM_MCP_WELL_KNOWN_PATH`], or `None` for any other path (`SEC-004`).
    #[must_use]
    pub fn handle_well_known_request(&self, path: &str) -> Option<String> {
        if path == PRM_WELL_KNOWN_PATH || path == PRM_MCP_WELL_KNOWN_PATH {
            Some(self.to_json_string())
        } else {
            None
        }
    }

    /// Formats the canonical RFC 9728 `WWW-Authenticate` HTTP response header (`SEC-004`).
    #[must_use]
    pub fn www_authenticate_header(&self, required_scope: Option<&str>) -> String {
        let mut out = format!(
            "Bearer resource_metadata=\"{}\"",
            escape_header_quoted(&self.metadata_url)
        );
        if let Some(scope) = required_scope
            && !scope.is_empty()
        {
            out.push_str(", scope=\"");
            out.push_str(&escape_header_quoted(scope));
            out.push('"');
        }
        out
    }

    /// Serializes this metadata record into JSON bytes (`application/json`).
    #[must_use]
    pub fn to_json_string(&self) -> String {
        let mut root = BTreeMap::new();
        root.insert(
            "authorization_servers".to_owned(),
            JsonValue::Array(
                self.authorization_servers
                    .iter()
                    .cloned()
                    .map(JsonValue::String)
                    .collect(),
            ),
        );
        root.insert(
            "bearer_methods_supported".to_owned(),
            JsonValue::Array(
                self.bearer_methods_supported
                    .iter()
                    .cloned()
                    .map(JsonValue::String)
                    .collect(),
            ),
        );
        root.insert(
            "resource".to_owned(),
            JsonValue::String(self.resource.clone()),
        );
        if let Some(doc) = &self.resource_documentation {
            root.insert(
                "resource_documentation".to_owned(),
                JsonValue::String(doc.clone()),
            );
        }
        if !self.scopes_supported.is_empty() {
            root.insert(
                "scopes_supported".to_owned(),
                JsonValue::Array(
                    self.scopes_supported
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                ),
            );
        }
        JsonValue::Object(root).to_jcs_string()
    }
}

fn escape_header_quoted(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if matches!(ch, '"' | '\\') {
            out.push('\\');
        }
        if !ch.is_control() {
            out.push(ch);
        }
    }
    out
}

/// Wire-visible constant-shape authentication challenge paired with internal audit detail (`SEC-003`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthChallenge {
    /// HTTP status code (`401` on normal failure, `429` when per-IP rate limit is tripped).
    pub http_status: u16,
    /// Constant public wire message (never distinguishes bad sig vs expired vs unknown user/tenant).
    pub wire_error: GapError,
    /// `WWW-Authenticate` header value (`RFC 9728` when PRM is configured).
    pub www_authenticate: String,
    /// Optional `Retry-After` seconds header when IP failure throttling is active.
    pub retry_after_secs: Option<u64>,
    /// Internal audit reason (`NEVER` returned on wire).
    internal_audit_reason: String,
}

impl AuthChallenge {
    /// Builds a uniform HTTP `401` challenge hiding underlying failure details (`SEC-003`, `SEC-004`).
    #[must_use]
    pub fn unauthenticated(
        prm: Option<&ProtectedResourceMetadata>,
        internal_reason: impl Into<String>,
    ) -> Self {
        let www_authenticate =
            prm.map_or_else(|| "Bearer".to_owned(), |m| m.www_authenticate_header(None));
        Self {
            http_status: 401,
            wire_error: GapError::new(ErrorCode::Unauthenticated, CONSTANT_AUTH_FAILURE_MESSAGE),
            www_authenticate,
            retry_after_secs: None,
            internal_audit_reason: internal_reason.into(),
        }
    }

    /// Builds a constant-shape HTTP `429` challenge when an IP exceeds its auth failure quota (`SEC-003`).
    #[must_use]
    pub fn rate_limited(
        retry_after_secs: u64,
        prm: Option<&ProtectedResourceMetadata>,
        internal_reason: impl Into<String>,
    ) -> Self {
        let mut challenge = Self::unauthenticated(prm, internal_reason);
        challenge.http_status = 429;
        challenge.wire_error = GapError::new(ErrorCode::RateLimited, CONSTANT_AUTH_FAILURE_MESSAGE);
        challenge.retry_after_secs = Some(retry_after_secs.max(1));
        challenge
    }

    /// Formats the redacted JSON challenge body safe for wire output (`SEC-003`).
    #[must_use]
    pub fn wire_json_body(&self) -> String {
        let mut out = String::from("{\"error\":");
        write_json_escaped(CONSTANT_AUTH_FAILURE_MESSAGE, &mut out);
        out.push('}');
        out
    }

    /// Returns the non-wire audit diagnostic explaining the actual failure (`SEC-003`).
    #[must_use]
    pub fn internal_audit_reason(&self) -> &str {
        &self.internal_audit_reason
    }
}

/// Configuration for per-IP failed authentication rate limiting (`SEC-003`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpAuthRateLimitConfig {
    /// Maximum failed attempts permitted per `window_secs` for a single IP before throttling.
    pub max_failures_per_window: usize,
    /// Sliding failure window length in seconds.
    pub window_secs: i64,
    /// Backoff / ban duration emitted in `Retry-After` once triggered.
    pub cooldown_secs: u64,
    /// Hard cap on distinct tracked IPs to bound memory under spoofed-IP floods.
    pub max_tracked_ips: usize,
}

impl Default for IpAuthRateLimitConfig {
    fn default() -> Self {
        Self {
            max_failures_per_window: 10,
            window_secs: 60,
            cooldown_secs: 60,
            max_tracked_ips: 8192,
        }
    }
}

#[derive(Debug)]
struct IpWindow {
    failures: VecDeque<i64>,
    blocked_until: i64,
}

/// Sliding-window per-IP failed-authentication limiter (`SEC-003`).
#[derive(Debug)]
pub struct IpAuthRateLimiter {
    config: IpAuthRateLimitConfig,
    state: Mutex<HashMap<IpAddr, IpWindow>>,
}

impl IpAuthRateLimiter {
    /// Creates a new per-IP authentication failure limiter.
    #[must_use]
    pub fn new(config: IpAuthRateLimitConfig) -> Self {
        Self {
            config,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Checks whether `ip` is currently rate-limited at `now_unix_secs`, returning `Some(retry_after)`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Internal`] if the internal limiter state is poisoned.
    pub fn check_blocked(&self, ip: IpAddr, now_unix_secs: i64) -> GapResult<Option<u64>> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| GapError::new(ErrorCode::Internal, "auth rate limiter lock poisoned"))?;
        let Some(entry) = guard.get_mut(&ip) else {
            return Ok(None);
        };
        if entry.blocked_until > now_unix_secs {
            let rem = (entry.blocked_until - now_unix_secs).max(1) as u64;
            return Ok(Some(rem));
        }
        Ok(None)
    }

    /// Records one failed authentication attempt for `ip` and returns `Some(retry_after_secs)` if
    /// the threshold has now been exceeded (`SEC-003`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Internal`] if the internal limiter state is poisoned.
    pub fn record_failure(&self, ip: IpAddr, now_unix_secs: i64) -> GapResult<Option<u64>> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| GapError::new(ErrorCode::Internal, "auth rate limiter lock poisoned"))?;

        if !guard.contains_key(&ip) && guard.len() >= self.config.max_tracked_ips {
            let cutoff = now_unix_secs - self.config.window_secs;
            guard.retain(|_, w| {
                w.blocked_until > now_unix_secs
                    || w.failures.back().copied().unwrap_or(i64::MIN) > cutoff
            });
            if guard.len() >= self.config.max_tracked_ips
                && let Some(evict_ip) = guard.keys().next().copied()
            {
                guard.remove(&evict_ip);
            }
        }

        let entry = guard.entry(ip).or_insert_with(|| IpWindow {
            failures: VecDeque::new(),
            blocked_until: i64::MIN,
        });

        let cutoff = now_unix_secs - self.config.window_secs;
        while let Some(&front) = entry.failures.front() {
            if front <= cutoff {
                entry.failures.pop_front();
            } else {
                break;
            }
        }
        entry.failures.push_back(now_unix_secs);

        if entry.failures.len() >= self.config.max_failures_per_window {
            let cooldown = i64::try_from(self.config.cooldown_secs).unwrap_or(60);
            entry.blocked_until = now_unix_secs.saturating_add(cooldown);
            return Ok(Some(self.config.cooldown_secs.max(1)));
        }
        Ok(None)
    }
}

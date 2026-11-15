//! DNS-pinning SSRF `EgressGuard`, IP/URL classifiers, and connector adapters (`SEC-075`, `SEC-076`, `SEC-072`, `WP-M6-08`).

pub mod guard;
pub mod ip;
pub mod url_parse;

#[cfg(test)]
mod tests;

pub use guard::{
    AllowRule, DEFAULT_CONNECT_TIMEOUT, DEFAULT_MAX_REDIRECTS, DnsResolver, EgressError,
    EgressGrpcAdapter, EgressGuard, EgressHttpAdapter, EgressPolicy, HopOutcome, PinnedEndpoint,
    SequenceResolver, StdDnsResolver,
};
pub use ip::{BlockedIpReason, CidrBlock, classify_blocked_ip};
pub use url_parse::{
    ParsedTargetUrl, Scheme, UrlParseError, is_blocked_internal_hostname, parse_target_url,
    resolve_redirect_url,
};

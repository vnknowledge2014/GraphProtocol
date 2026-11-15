//! mTLS SPIFFE principal mapping and local `stdio` OS-user service principal (`SEC-001`, `SEC-010`).

use std::collections::{BTreeMap, BTreeSet};

use gap_core::context::{AttrValue, PrincipalKind};
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::TenantId;

use super::principal::{
    CredentialClaims, DerivedPrincipalOutput, UntrustedRequestHints, derive_authenticated_principal,
};

/// Verified TLS peer certificate metadata handed from the transport terminator (`SEC-001`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedClientCert {
    /// Must be `true` after CA chain verification and revocation checks succeeded at the TLS layer.
    pub chain_verified: bool,
    /// Subject Alternative Name (SAN) URIs (`spiffe://<trust_domain>/...`).
    pub san_uris: Vec<String>,
    /// Subject Alternative Name (SAN) DNS names.
    pub san_dns: Vec<String>,
    /// X.509 Subject DN string (audit metadata only).
    pub subject_dn: String,
    /// Certificate SHA-256 thumbprint hex (`INV-15` safe identifier).
    pub sha256_thumbprint: String,
}

/// Trust mapping rule resolving a validated SPIFFE identity into tenant & scopes (`SEC-001`, `SEC-010`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpiffeBinding {
    /// Expected SPIFFE trust domain (`spiffe://<trust_domain>/...`).
    pub trust_domain: String,
    /// Exact tenant override if the trust domain maps to a single tenant (`None` requires `/tenant/<id>/...` path).
    pub fixed_tenant: Option<TenantId>,
    /// Default principal kind mapped for matched SPIFFE peers (default `Service`).
    pub kind: PrincipalKind,
    /// Scopes bound to the SPIFFE workload identity.
    pub scopes: BTreeSet<String>,
}

/// Maps TLS client certificates with SPIFFE SAN URIs to authenticated [`gap_core::context::Principal`]s (`SEC-001`).
#[derive(Debug, Default)]
pub struct MtlsAuthenticator {
    bindings_by_domain: BTreeMap<String, SpiffeBinding>,
}

impl MtlsAuthenticator {
    /// Creates an mTLS authenticator from trusted SPIFFE trust-domain bindings.
    #[must_use]
    pub fn new(bindings: impl IntoIterator<Item = SpiffeBinding>) -> Self {
        let mut map = BTreeMap::new();
        for b in bindings {
            map.insert(b.trust_domain.clone(), b);
        }
        Self {
            bindings_by_domain: map,
        }
    }

    /// Maps a verified peer certificate into an authenticated [`DerivedPrincipalOutput`]
    /// carrying a [`gap_core::context::Principal`].
    ///
    /// Expects at least one `spiffe://<trust_domain>/<workload_path>` SAN URI. When `fixed_tenant`
    /// is unset on the trust domain rule, `<workload_path>` must begin with `tenant/<tenant_id>/...`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Unauthenticated`] if `chain_verified` is false, no trusted SPIFFE SAN
    /// matches, or tenant derivation fails.
    pub fn authenticate(
        &self,
        cert: &VerifiedClientCert,
        hints: Option<&UntrustedRequestHints>,
    ) -> GapResult<DerivedPrincipalOutput> {
        if !cert.chain_verified {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "mTLS peer certificate chain not verified",
            ));
        }

        for uri in &cert.san_uris {
            if let Some((trust_domain, path)) = parse_spiffe_uri(uri)
                && let Some(binding) = self.bindings_by_domain.get(trust_domain)
            {
                let tenant = if let Some(fixed) = &binding.fixed_tenant {
                    fixed.clone()
                } else {
                    extract_tenant_from_spiffe_path(path)?
                };
                let mut attrs = BTreeMap::new();
                attrs.insert(
                    "cert_thumbprint".to_owned(),
                    AttrValue::Text(cert.sha256_thumbprint.clone()),
                );
                let claims = CredentialClaims {
                    subject: uri.clone(),
                    kind: binding.kind,
                    tenant,
                    scopes: binding.scopes.clone(),
                    attrs,
                    delegation: Vec::new(),
                };
                return Ok(derive_authenticated_principal(claims, hints));
            }
        }

        Err(GapError::new(
            ErrorCode::Unauthenticated,
            "mTLS certificate carries no trusted SPIFFE SAN",
        ))
    }
}

fn parse_spiffe_uri(uri: &str) -> Option<(&str, &str)> {
    let rest = uri.strip_prefix("spiffe://")?;
    let (domain, path) = rest.split_once('/')?;
    if domain.is_empty() || path.is_empty() {
        return None;
    }
    Some((domain, path))
}

fn extract_tenant_from_spiffe_path(path: &str) -> GapResult<TenantId> {
    let mut segs = path.split('/');
    match (segs.next(), segs.next()) {
        (Some("tenant"), Some(raw)) if !raw.is_empty() => TenantId::new(raw).map_err(|_| {
            GapError::new(
                ErrorCode::Unauthenticated,
                "invalid tenant segment in SPIFFE URI",
            )
        }),
        _ => Err(GapError::new(
            ErrorCode::Unauthenticated,
            "SPIFFE path must encode /tenant/<id>/... when trust domain is multi-tenant",
        )),
    }
}

/// Authenticates trusted local `stdio` transport sessions as OS-user `Service` principals (`SEC-001`, `SEC-004`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioAuthenticator {
    /// Authoritative local tenant assigned to this `stdio` server instance.
    pub tenant: TenantId,
    /// Scopes granted to the local OS user session.
    pub scopes: BTreeSet<String>,
}

impl StdioAuthenticator {
    /// Creates a `stdio` OS-user authenticator pinned to `tenant`.
    #[must_use]
    pub const fn new(tenant: TenantId, scopes: BTreeSet<String>) -> Self {
        Self { tenant, scopes }
    }

    /// Derives a [`PrincipalKind::Service`] principal for local OS user `os_user`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Unauthenticated`] if `os_user` is empty.
    pub fn authenticate_os_user(
        &self,
        os_user: &str,
        hints: Option<&UntrustedRequestHints>,
    ) -> GapResult<DerivedPrincipalOutput> {
        let trimmed = os_user.trim();
        if trimmed.is_empty() {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "empty OS user identity on stdio transport",
            ));
        }
        let mut attrs = BTreeMap::new();
        attrs.insert("transport".to_owned(), AttrValue::Text("stdio".to_owned()));
        let claims = CredentialClaims {
            subject: format!("os:{trimmed}"),
            kind: PrincipalKind::Service,
            tenant: self.tenant.clone(),
            scopes: self.scopes.clone(),
            attrs,
            delegation: Vec::new(),
        };
        Ok(derive_authenticated_principal(claims, hints))
    }
}

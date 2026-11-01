//! `[[branches]]` and `[[api_keys]]` of `gap.toml`: the branch inventory a gateway is assembled from and the
//! API keys it accepts (`spec/00-conventions.md` §8, `OPS-001`, `SEC-001`, `SEC-010`, `INV-15`).
//!
//! The model lives in `gap-core` so every front end (`gap-server`, `gap-cli doctor/config`) reads one
//! definition and one JSON Schema. Secrets are references ([`SecretRef`]) and API keys are stored as
//! `blake3` digests, never as literals (`INV-15`).

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ConfigError, SecretRef};
use crate::ids::{BranchId, BranchUid, TenantId};
use crate::snapshot::TrustTier;

/// Wire protocol of a configured branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BranchProtocol {
    /// An OpenAPI 3.x REST service (`gap-ngc-openapi`).
    Openapi,
    /// A Model Context Protocol server (`gap-mcp` outbound).
    Mcp,
}

/// How a branch authenticates to its upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BranchAuthKind {
    /// No upstream credentials.
    #[default]
    None,
    /// `Authorization: Bearer <secret>`.
    Bearer,
    /// An API key sent in the header or query parameter named by `name`.
    ApiKey,
}

/// Upstream authentication of a branch; the credential is only ever a [`SecretRef`] (`INV-15`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BranchAuthSpec {
    /// Authentication scheme.
    pub kind: BranchAuthKind,
    /// Reference to the credential (`env://NAME`, `file:///abs/path`, `secret://vault/name`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<SecretRef>,
    /// Header or query-parameter name for [`BranchAuthKind::ApiKey`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `[branches.openapi]`: an OpenAPI 3.x REST upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenApiBranchSpec {
    /// OpenAPI document path, relative to the directory of `gap.toml` (no `..`, not absolute).
    pub spec: String,
    /// Upstream origin and optional base path (`http://` or `https://`).
    pub base_url: String,
    /// Permit cleartext `http://` credentials to a non-loopback host.
    #[serde(default)]
    pub allow_insecure_upstream: bool,
    /// Connector-enforced concurrent in-flight RPC cap (`>= 1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_in_flight: Option<u32>,
    /// Largest page the connector advertises (`>= 1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_page_size: Option<u32>,
    /// Upstream authentication.
    #[serde(default)]
    pub auth: BranchAuthSpec,
}

/// Transport of an MCP branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum McpTransportKind {
    /// Streamable HTTP at `url`.
    Http,
    /// A child process speaking newline-delimited JSON-RPC (allow-listed, hash-pinned).
    Stdio,
}

/// `[branches.mcp]`: an MCP server upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpBranchSpec {
    /// Transport.
    pub transport: McpTransportKind,
    /// Endpoint for [`McpTransportKind::Http`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Absolute executable path for [`McpTransportKind::Stdio`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments of the child process.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Pinned `sha256` (64 hex characters) of the executable; required for stdio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Permit cleartext `http://` to a non-loopback host.
    #[serde(default)]
    pub allow_insecure: bool,
    /// Upstream authentication (HTTP only).
    #[serde(default)]
    pub auth: BranchAuthSpec,
}

/// One `[[branches]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BranchSpec {
    /// Unique branch slug.
    pub id: BranchId,
    /// Wire protocol; selects which of `openapi` / `mcp` must be present.
    pub protocol: BranchProtocol,
    /// Trust tier (`ADR-013`); `T1` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<TrustTier>,
    /// Explicit stable branch UUIDv7; derived from `id` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<BranchUid>,
    /// OpenAPI settings (`protocol = "openapi"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openapi: Option<OpenApiBranchSpec>,
    /// MCP settings (`protocol = "mcp"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpBranchSpec>,
}

impl BranchSpec {
    /// Effective trust tier (`T1` unless configured).
    #[must_use]
    pub fn trust_tier(&self) -> TrustTier {
        self.trust.unwrap_or(TrustTier::T1)
    }
}

/// One `[[api_keys]]` entry: the `blake3` digest of a key, never the key (`INV-15`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApiKeySpec {
    /// Stable non-secret identifier (becomes the principal id).
    pub id: String,
    /// 64 hex characters: `blake3-256` of the raw key.
    pub key_hash: String,
    /// Tenant the key is bound to (`SEC-010`); the default tenant when absent.
    #[serde(default = "TenantId::default_tenant")]
    pub tenant: TenantId,
    /// Authorization scopes granted to the key.
    #[serde(default)]
    pub scopes: Vec<String>,
}

fn invalid(path: impl Into<String>, reason: impl Into<String>) -> ConfigError {
    ConfigError::InvalidEntry {
        path: path.into(),
        reason: reason.into(),
    }
}

fn is_hex64(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_http_url(text: &str) -> bool {
    let rest = text
        .strip_prefix("http://")
        .or_else(|| text.strip_prefix("https://"));
    rest.is_some_and(|r| {
        !r.is_empty() && !r.starts_with('/') && !r.chars().any(char::is_whitespace)
    })
}

fn check_auth(path: &str, auth: &BranchAuthSpec) -> Result<(), ConfigError> {
    match (auth.kind, auth.secret.is_some()) {
        (BranchAuthKind::None, true) => Err(invalid(
            format!("{path}.auth.secret"),
            "a secret is set but auth.kind is \"none\"",
        )),
        (BranchAuthKind::Bearer | BranchAuthKind::ApiKey, false) => Err(invalid(
            format!("{path}.auth.secret"),
            "auth.kind needs a secret reference (env://NAME, file:///path or secret://vault/name)",
        )),
        _ => match (auth.kind, auth.name.as_deref()) {
            (BranchAuthKind::ApiKey, None | Some("")) => Err(invalid(
                format!("{path}.auth.name"),
                "auth.kind = \"api_key\" needs the header or query parameter name",
            )),
            (BranchAuthKind::None | BranchAuthKind::Bearer, Some(_)) => Err(invalid(
                format!("{path}.auth.name"),
                "auth.name only applies to auth.kind = \"api_key\"",
            )),
            _ => Ok(()),
        },
    }
}

fn check_openapi(path: &str, spec: &OpenApiBranchSpec) -> Result<(), ConfigError> {
    let doc = spec.spec.as_str();
    if doc.is_empty()
        || doc.starts_with('/')
        || doc.contains('\\')
        || doc.split('/').any(|segment| segment == "..")
    {
        return Err(invalid(
            format!("{path}.spec"),
            "must be a non-empty relative path without `..` (relative to the directory of gap.toml)",
        ));
    }
    if !is_http_url(&spec.base_url) {
        return Err(invalid(
            format!("{path}.base_url"),
            "must be an http:// or https:// URL",
        ));
    }
    for (field, value) in [
        ("max_in_flight", spec.max_in_flight),
        ("max_page_size", spec.max_page_size),
    ] {
        if value == Some(0) {
            return Err(invalid(format!("{path}.{field}"), "must be at least 1"));
        }
    }
    check_auth(path, &spec.auth)
}

fn check_mcp(path: &str, spec: &McpBranchSpec) -> Result<(), ConfigError> {
    match spec.transport {
        McpTransportKind::Http => {
            if !spec.url.as_deref().is_some_and(is_http_url) {
                return Err(invalid(
                    format!("{path}.url"),
                    "transport = \"http\" needs an http:// or https:// url",
                ));
            }
            for (field, present) in [
                ("command", spec.command.is_some()),
                ("args", !spec.args.is_empty()),
                ("sha256", spec.sha256.is_some()),
            ] {
                if present {
                    return Err(invalid(
                        format!("{path}.{field}"),
                        "only applies to transport = \"stdio\"",
                    ));
                }
            }
            check_auth(path, &spec.auth)
        }
        McpTransportKind::Stdio => {
            if !spec
                .command
                .as_deref()
                .is_some_and(|c| c.starts_with('/') && !c.contains('\0'))
            {
                return Err(invalid(
                    format!("{path}.command"),
                    "transport = \"stdio\" needs an absolute executable path",
                ));
            }
            if !spec.sha256.as_deref().is_some_and(is_hex64) {
                return Err(invalid(
                    format!("{path}.sha256"),
                    "transport = \"stdio\" needs the pinned sha256 of the executable (64 hex characters)",
                ));
            }
            if spec.url.is_some() {
                return Err(invalid(
                    format!("{path}.url"),
                    "only applies to transport = \"http\"",
                ));
            }
            if spec.auth != BranchAuthSpec::default() {
                return Err(invalid(
                    format!("{path}.auth"),
                    "upstream authentication does not apply to a stdio child process",
                ));
            }
            Ok(())
        }
    }
}

/// Validates the branch inventory and the API keys (cross-entry and per-entry rules).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidEntry`] naming the offending key path (for example
/// `branches[1].openapi.spec`); the message never contains a secret value.
pub fn validate_branches_and_keys(
    branches: &[BranchSpec],
    api_keys: &[ApiKeySpec],
) -> Result<(), ConfigError> {
    let mut ids = BTreeSet::new();
    let mut uids = BTreeSet::new();
    for (i, branch) in branches.iter().enumerate() {
        let path = format!("branches[{i}]");
        if !ids.insert(branch.id.to_string()) {
            return Err(invalid(format!("{path}.id"), "duplicate branch id"));
        }
        if let Some(uid) = branch.uid
            && !uids.insert(uid)
        {
            return Err(invalid(format!("{path}.uid"), "duplicate branch uid"));
        }
        match (branch.protocol, &branch.openapi, &branch.mcp) {
            (BranchProtocol::Openapi, Some(spec), None) => {
                check_openapi(&format!("{path}.openapi"), spec)?;
            }
            (BranchProtocol::Mcp, None, Some(spec)) => check_mcp(&format!("{path}.mcp"), spec)?,
            (BranchProtocol::Openapi, None, _) => {
                return Err(invalid(
                    format!("{path}.openapi"),
                    "protocol = \"openapi\" needs a [branches.openapi] table",
                ));
            }
            (BranchProtocol::Mcp, _, None) => {
                return Err(invalid(
                    format!("{path}.mcp"),
                    "protocol = \"mcp\" needs a [branches.mcp] table",
                ));
            }
            (BranchProtocol::Openapi, Some(_), Some(_)) => {
                return Err(invalid(
                    format!("{path}.mcp"),
                    "protocol = \"openapi\" must not carry a [branches.mcp] table",
                ));
            }
            (BranchProtocol::Mcp, Some(_), Some(_)) => {
                return Err(invalid(
                    format!("{path}.openapi"),
                    "protocol = \"mcp\" must not carry a [branches.openapi] table",
                ));
            }
        }
    }

    let mut key_ids = BTreeSet::new();
    for (i, key) in api_keys.iter().enumerate() {
        let path = format!("api_keys[{i}]");
        let id_ok = !key.id.is_empty()
            && key.id.len() <= 64
            && key
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
        if !id_ok {
            return Err(invalid(
                format!("{path}.id"),
                "must be 1..=64 characters of [A-Za-z0-9_.-]",
            ));
        }
        if !key_ids.insert(key.id.as_str()) {
            return Err(invalid(format!("{path}.id"), "duplicate api key id"));
        }
        if !is_hex64(&key.key_hash) {
            return Err(invalid(
                format!("{path}.key_hash"),
                "must be the 64-hex-character blake3 digest of the key (never the key itself)",
            ));
        }
        if key
            .scopes
            .iter()
            .any(|s| s.is_empty() || s.chars().any(char::is_whitespace))
        {
            return Err(invalid(
                format!("{path}.scopes"),
                "scopes must be non-empty and contain no whitespace",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::GapConfig;
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn parse(extra: &str) -> Result<GapConfig, ConfigError> {
        GapConfig::from_toml_str(extra)
    }

    fn path_of(result: Result<GapConfig, ConfigError>) -> String {
        match result.expect_err("must be refused") {
            ConfigError::InvalidEntry { path, .. } => path,
            other => panic!("unexpected error: {other}"),
        }
    }

    const FULL: &str = r#"
profile = "dev"

[[branches]]
id = "shop"
protocol = "openapi"
trust = "T0"
[branches.openapi]
spec = "shop.openapi.json"
base_url = "http://127.0.0.1:9101"
allow_insecure_upstream = true
max_in_flight = 8
[branches.openapi.auth]
kind = "api_key"
secret = "env://SHOP_KEY"
name = "x-api-key"

[[branches]]
id = "notes"
protocol = "mcp"
[branches.mcp]
transport = "http"
url = "http://127.0.0.1:9102/mcp"

[[branches]]
id = "local"
protocol = "mcp"
trust = "T2"
[branches.mcp]
transport = "stdio"
command = "/usr/bin/python3"
args = ["server.py"]
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[[api_keys]]
id = "agent-1"
key_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
scopes = ["read"]
"#;

    // spec: OPS-001, INV-15
    #[test]
    fn a_full_branch_inventory_parses_and_round_trips() {
        let cfg = parse(FULL).expect("valid");
        assert_eq!(cfg.branches.len(), 3);
        assert_eq!(cfg.branches[0].trust_tier(), TrustTier::T0);
        assert_eq!(
            cfg.branches[1].trust_tier(),
            TrustTier::T1,
            "T1 unless configured"
        );
        assert_eq!(cfg.branches[2].trust_tier(), TrustTier::T2);
        assert_eq!(cfg.api_keys[0].tenant, TenantId::default_tenant());
        let text = toml::to_string(&cfg).expect("serialises");
        assert_eq!(parse(&text).expect("re-parses"), cfg);
        assert!(parse("").expect("empty file").branches.is_empty());
    }

    // spec: OPS-001
    #[test]
    fn every_invalid_entry_names_its_key_path() {
        let cases: &[(&str, String)] = &[
            (
                "[[branches]]\nid = \"aa\"\nprotocol = \"openapi\"\n",
                "branches[0].openapi".to_owned(),
            ),
            (
                "[[branches]]\nid = \"aa\"\nprotocol = \"mcp\"\n",
                "branches[0].mcp".to_owned(),
            ),
            (
                "[[branches]]\nid = \"aa\"\nprotocol = \"mcp\"\n[branches.mcp]\ntransport = \"http\"\nurl = \"http://h\"\n[branches.openapi]\nspec = \"s.json\"\nbase_url = \"http://h\"\n",
                "branches[0].openapi".to_owned(),
            ),
            (
                "[[branches]]\nid = \"aa\"\nprotocol = \"openapi\"\n[branches.openapi]\nspec = \"../s.json\"\nbase_url = \"http://h\"\n",
                "branches[0].openapi.spec".to_owned(),
            ),
            (
                "[[branches]]\nid = \"aa\"\nprotocol = \"openapi\"\n[branches.openapi]\nspec = \"/etc/s.json\"\nbase_url = \"http://h\"\n",
                "branches[0].openapi.spec".to_owned(),
            ),
            (
                "[[branches]]\nid = \"aa\"\nprotocol = \"openapi\"\n[branches.openapi]\nspec = \"s.json\"\nbase_url = \"ftp://h\"\n",
                "branches[0].openapi.base_url".to_owned(),
            ),
            (
                "[[branches]]\nid = \"aa\"\nprotocol = \"openapi\"\n[branches.openapi]\nspec = \"s.json\"\nbase_url = \"http://h\"\nmax_in_flight = 0\n",
                "branches[0].openapi.max_in_flight".to_owned(),
            ),
            (
                "[[branches]]\nid = \"aa\"\nprotocol = \"openapi\"\n[branches.openapi]\nspec = \"s.json\"\nbase_url = \"http://h\"\n[branches.openapi.auth]\nkind = \"bearer\"\n",
                "branches[0].openapi.auth.secret".to_owned(),
            ),
            (
                "[[branches]]\nid = \"aa\"\nprotocol = \"openapi\"\n[branches.openapi]\nspec = \"s.json\"\nbase_url = \"http://h\"\n[branches.openapi.auth]\nkind = \"api_key\"\nsecret = \"env://K\"\n",
                "branches[0].openapi.auth.name".to_owned(),
            ),
            (
                "[[branches]]\nid = \"aa\"\nprotocol = \"mcp\"\n[branches.mcp]\ntransport = \"http\"\n",
                "branches[0].mcp.url".to_owned(),
            ),
            (
                "[[branches]]\nid = \"aa\"\nprotocol = \"mcp\"\n[branches.mcp]\ntransport = \"stdio\"\ncommand = \"relative/bin\"\nsha256 = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\n",
                "branches[0].mcp.command".to_owned(),
            ),
            (
                "[[branches]]\nid = \"aa\"\nprotocol = \"mcp\"\n[branches.mcp]\ntransport = \"stdio\"\ncommand = \"/bin/x\"\n",
                "branches[0].mcp.sha256".to_owned(),
            ),
            (
                "[[branches]]\nid = \"aa\"\nprotocol = \"mcp\"\n[branches.mcp]\ntransport = \"http\"\nurl = \"http://h\"\nsha256 = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\n",
                "branches[0].mcp.sha256".to_owned(),
            ),
            (
                "[[api_keys]]\nid = \"k\"\nkey_hash = \"short\"\n",
                "api_keys[0].key_hash".to_owned(),
            ),
            (
                "[[api_keys]]\nid = \"bad id\"\nkey_hash = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\n",
                "api_keys[0].id".to_owned(),
            ),
            (
                "[[api_keys]]\nid = \"k\"\nkey_hash = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\nscopes = [\"two words\"]\n",
                "api_keys[0].scopes".to_owned(),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(&path_of(parse(text)), expected, "{text}");
        }
    }

    // spec: OPS-001
    #[test]
    fn duplicates_are_refused_with_the_index_of_the_second_entry() {
        let two = format!(
            "{b}{b}",
            b = "[[branches]]\nid = \"aa\"\nprotocol = \"mcp\"\n[branches.mcp]\ntransport = \"http\"\nurl = \"http://h\"\n"
        );
        assert_eq!(path_of(parse(&two)), "branches[1].id");
        let keys = format!(
            "{k}{k}",
            k = format!("[[api_keys]]\nid = \"k\"\nkey_hash = \"{HASH}\"\n")
        );
        assert_eq!(path_of(parse(&keys)), "api_keys[1].id");
        let uid = "018bcfe5-6c01-7010-8101-010101010101";
        let uids = format!(
            "[[branches]]\nid = \"aa\"\nprotocol = \"mcp\"\nuid = \"{uid}\"\n[branches.mcp]\ntransport = \"http\"\nurl = \"http://h\"\n[[branches]]\nid = \"bb\"\nprotocol = \"mcp\"\nuid = \"{uid}\"\n[branches.mcp]\ntransport = \"http\"\nurl = \"http://h\"\n"
        );
        assert_eq!(path_of(parse(&uids)), "branches[1].uid");
    }

    // spec: INV-15
    #[test]
    fn a_literal_secret_is_refused_and_never_echoed() {
        let canary = "sk-live-CANARY-0123456789";
        let text = format!(
            "[[branches]]\nid = \"aa\"\nprotocol = \"openapi\"\n[branches.openapi]\nspec = \"s.json\"\nbase_url = \"http://h\"\n[branches.openapi.auth]\nkind = \"bearer\"\nsecret = \"{canary}\"\n"
        );
        let err = parse(&text)
            .expect_err("inline secrets are forbidden")
            .to_string();
        assert!(!err.contains(canary), "{err}");
        let key = format!("[[api_keys]]\nid = \"k\"\nkey_hash = \"{canary}\"\n");
        let err = parse(&key).expect_err("not a digest").to_string();
        assert!(!err.contains(canary), "{err}");
    }

    // spec: OPS-001
    #[test]
    fn unknown_keys_in_branch_tables_are_errors() {
        for text in [
            "[[branches]]\nid = \"aa\"\nprotocol = \"mcp\"\nextra = 1\n",
            "[[api_keys]]\nid = \"k\"\nkey_hash = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\nextra = 1\n",
        ] {
            assert!(matches!(parse(text), Err(ConfigError::Parse(_))), "{text}");
        }
    }
}

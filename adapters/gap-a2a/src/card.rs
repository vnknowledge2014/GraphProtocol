//! Deterministic `A2A v1.0` `AgentCard` generation and JWS signing/verification (`CMP-020`, `Q-12`, `Q-15`).
//!
//! Skills are synthesized strictly from [`TrustTier::T0`] and [`TrustTier::T1`] branch metadata
//! and capability groups (`INV-05`, `INV-07`); untrusted [`TrustTier::T2`] branch descriptions are
//! never reflected into advertised `AgentCard` JSON.

use std::collections::BTreeMap;

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::snapshot::{BranchStatus, Snapshot, TrustTier};
use gap_sec::crypto::{Ed25519SigningKey, Ed25519VerifyingKey, base64url_decode, base64url_encode};
use gap_sec::json::JsonValue;

use crate::wire::{A2A_PROTOCOL_VERSION, GAP_TRACE_EXT_URI};

/// Canonical A2A v1.0 `protocolBinding` literals (`SPEC-NOTES.md` `Q-12`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AgentProtocolBinding {
    /// JSON-RPC 2.0 over HTTP (`"JSONRPC"`).
    JsonRpc,
    /// HTTP+JSON + SSE (`"HTTP+JSON"`).
    HttpJson,
    /// Protobuf over gRPC (`"GRPC"`).
    Grpc,
}

impl AgentProtocolBinding {
    /// Canonical `protocolBinding` wire literal.
    #[must_use]
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::JsonRpc => "JSONRPC",
            Self::HttpJson => "HTTP+JSON",
            Self::Grpc => "GRPC",
        }
    }
}

/// Interface declaration inside `AgentCard.supportedInterfaces[]` (`Q-12`, `Q-15`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInterface {
    /// Interface base URL or gRPC endpoint.
    pub url: String,
    /// Official protocol binding (`JSONRPC`, `HTTP+JSON`, `GRPC`).
    pub protocol_binding: AgentProtocolBinding,
    /// A2A protocol version (`"1.0"`).
    pub protocol_version: String,
    /// Optional routing hint echoed by A2A `v1.0.1` clients (`Q-15`).
    pub tenant: Option<String>,
}

impl AgentInterface {
    /// Encodes to JSON DOM.
    #[must_use]
    pub fn to_json_value(&self) -> JsonValue {
        let mut obj = BTreeMap::new();
        obj.insert("url".to_owned(), JsonValue::String(self.url.clone()));
        obj.insert(
            "protocolBinding".to_owned(),
            JsonValue::String(self.protocol_binding.as_wire_str().to_owned()),
        );
        obj.insert(
            "protocolVersion".to_owned(),
            JsonValue::String(self.protocol_version.clone()),
        );
        if let Some(ref tenant) = self.tenant {
            obj.insert("tenant".to_owned(), JsonValue::String(tenant.clone()));
        }
        JsonValue::Object(obj)
    }

    /// Parses an `AgentInterface` object from JSON DOM.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] when required fields or bindings are invalid.
    pub fn from_json_value(val: &JsonValue) -> GapResult<Self> {
        let obj = val.as_object().ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                "AgentInterface must be a JSON object",
            )
        })?;
        let url = obj
            .get("url")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                GapError::new(ErrorCode::ValidationError, "AgentInterface.url is required")
            })?
            .to_owned();
        let binding_raw = obj
            .get("protocolBinding")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "AgentInterface.protocolBinding is required",
                )
            })?;
        let protocol_binding = match binding_raw {
            "JSONRPC" => AgentProtocolBinding::JsonRpc,
            "HTTP+JSON" => AgentProtocolBinding::HttpJson,
            "GRPC" => AgentProtocolBinding::Grpc,
            other => {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("unsupported AgentInterface.protocolBinding `{other}`"),
                ));
            }
        };
        let protocol_version = obj
            .get("protocolVersion")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "AgentInterface.protocolVersion is required",
                )
            })?
            .to_owned();
        let tenant = obj
            .get("tenant")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned);
        Ok(Self {
            url,
            protocol_binding,
            protocol_version,
            tenant,
        })
    }
}

/// Declared extension inside `AgentCard.capabilities.extensions[]` (`Q-13`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentExtension {
    /// Versioned extension URI (e.g. `https://gap-protocol.org/ext/trace/v1`).
    pub uri: String,
    /// Human-readable description.
    pub description: Option<String>,
    /// Whether callers are expected to understand the extension.
    pub required: bool,
}

impl AgentExtension {
    /// Encodes to JSON DOM.
    #[must_use]
    pub fn to_json_value(&self) -> JsonValue {
        let mut obj = BTreeMap::new();
        obj.insert("uri".to_owned(), JsonValue::String(self.uri.clone()));
        if let Some(ref desc) = self.description {
            obj.insert("description".to_owned(), JsonValue::String(desc.clone()));
        }
        obj.insert("required".to_owned(), JsonValue::Bool(self.required));
        JsonValue::Object(obj)
    }

    /// Parses an `AgentExtension` object from JSON DOM.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] when `uri` is missing.
    pub fn from_json_value(val: &JsonValue) -> GapResult<Self> {
        let obj = val.as_object().ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                "AgentExtension must be a JSON object",
            )
        })?;
        let uri = obj
            .get("uri")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                GapError::new(ErrorCode::ValidationError, "AgentExtension.uri is required")
            })?
            .to_owned();
        let description = obj
            .get("description")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned);
        let required = obj
            .get("required")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        Ok(Self {
            uri,
            description,
            required,
        })
    }
}

/// Capabilities declared in `AgentCard.capabilities` (`CMP-020`, `Q-12`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCapabilities {
    /// SSE task streaming (`SendStreamingMessage` / `SubscribeToTask`).
    pub streaming: bool,
    /// Push notification webhooks (`false` in GAP 1.0).
    pub push_notifications: bool,
    /// Authenticated extended card endpoint (`false` in GAP 1.0, `Q-12`).
    pub extended_agent_card: bool,
    /// Declared protocol extensions (always includes GAP trace URI).
    pub extensions: Vec<AgentExtension>,
}

impl Default for AgentCapabilities {
    fn default() -> Self {
        Self {
            streaming: true,
            push_notifications: false,
            extended_agent_card: false,
            extensions: vec![AgentExtension {
                uri: GAP_TRACE_EXT_URI.to_owned(),
                description: Some("GAP loop detection and hop-limit trace propagation".to_owned()),
                required: false,
            }],
        }
    }
}

impl AgentCapabilities {
    /// Encodes to JSON DOM.
    #[must_use]
    pub fn to_json_value(&self) -> JsonValue {
        let mut obj = BTreeMap::new();
        obj.insert("streaming".to_owned(), JsonValue::Bool(self.streaming));
        obj.insert(
            "pushNotifications".to_owned(),
            JsonValue::Bool(self.push_notifications),
        );
        obj.insert(
            "extendedAgentCard".to_owned(),
            JsonValue::Bool(self.extended_agent_card),
        );
        if !self.extensions.is_empty() {
            obj.insert(
                "extensions".to_owned(),
                JsonValue::Array(
                    self.extensions
                        .iter()
                        .map(AgentExtension::to_json_value)
                        .collect(),
                ),
            );
        }
        JsonValue::Object(obj)
    }

    /// Parses `AgentCapabilities` from JSON DOM.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] when fields have unexpected types.
    pub fn from_json_value(val: &JsonValue) -> GapResult<Self> {
        let obj = val.as_object().ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                "AgentCapabilities must be a JSON object",
            )
        })?;
        let streaming = obj
            .get("streaming")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        let push_notifications = obj
            .get("pushNotifications")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        let extended_agent_card = obj
            .get("extendedAgentCard")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        let mut extensions = Vec::new();
        if let Some(arr) = obj.get("extensions").and_then(JsonValue::as_array) {
            for ext in arr {
                extensions.push(AgentExtension::from_json_value(ext)?);
            }
        }
        Ok(Self {
            streaming,
            push_notifications,
            extended_agent_card,
            extensions,
        })
    }
}

/// Skill summary entry in `AgentCard.skills[]` (`CMP-020`, `Q-12`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSkill {
    /// Stable skill identifier (`gapql`, `resolve`, or trusted branch id).
    pub id: String,
    /// Human-readable skill title.
    pub name: String,
    /// Concise summary generated deterministically without `T2` text.
    pub description: String,
    /// Required non-empty tag list (`Q-12`).
    pub tags: Vec<String>,
}

impl AgentSkill {
    /// Encodes to JSON DOM.
    #[must_use]
    pub fn to_json_value(&self) -> JsonValue {
        let mut obj = BTreeMap::new();
        obj.insert("id".to_owned(), JsonValue::String(self.id.clone()));
        obj.insert("name".to_owned(), JsonValue::String(self.name.clone()));
        obj.insert(
            "description".to_owned(),
            JsonValue::String(self.description.clone()),
        );
        obj.insert(
            "tags".to_owned(),
            JsonValue::Array(self.tags.iter().cloned().map(JsonValue::String).collect()),
        );
        JsonValue::Object(obj)
    }

    /// Parses an `AgentSkill` from JSON DOM.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] when required `id`, `name`, `description`, or `tags` are missing.
    pub fn from_json_value(val: &JsonValue) -> GapResult<Self> {
        let obj = val.as_object().ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                "AgentSkill must be a JSON object",
            )
        })?;
        let id = obj
            .get("id")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "AgentSkill.id is required"))?
            .to_owned();
        let name = obj
            .get("name")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                GapError::new(ErrorCode::ValidationError, "AgentSkill.name is required")
            })?
            .to_owned();
        let description = obj
            .get("description")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "AgentSkill.description is required",
                )
            })?
            .to_owned();
        let tags_arr = obj
            .get("tags")
            .and_then(JsonValue::as_array)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "AgentSkill.tags must be a non-empty string array",
                )
            })?;
        let mut tags = Vec::with_capacity(tags_arr.len());
        for tag in tags_arr {
            let s = tag.as_str().ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "AgentSkill.tags items must be strings",
                )
            })?;
            tags.push(s.to_owned());
        }
        Ok(Self {
            id,
            name,
            description,
            tags,
        })
    }
}

/// Inline detached JWS signature inside `AgentCard.signatures[]` (`CMP-020`, `Q-12`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCardSignature {
    /// Base64url-encoded JWS protected header (`{"alg":"EdDSA","kid":"..."}`).
    pub protected: String,
    /// Base64url-encoded 64-byte Ed25519 signature over `<protected>.<base64url(jcs_payload)>`.
    pub signature: String,
}

impl AgentCardSignature {
    /// Encodes to JSON DOM.
    #[must_use]
    pub fn to_json_value(&self) -> JsonValue {
        let mut obj = BTreeMap::new();
        obj.insert(
            "protected".to_owned(),
            JsonValue::String(self.protected.clone()),
        );
        obj.insert(
            "signature".to_owned(),
            JsonValue::String(self.signature.clone()),
        );
        JsonValue::Object(obj)
    }

    /// Parses an `AgentCardSignature` from JSON DOM.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] when `protected` or `signature` is missing.
    pub fn from_json_value(val: &JsonValue) -> GapResult<Self> {
        let obj = val.as_object().ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                "AgentCardSignature must be a JSON object",
            )
        })?;
        let protected = obj
            .get("protected")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "AgentCardSignature.protected is required",
                )
            })?
            .to_owned();
        let signature = obj
            .get("signature")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "AgentCardSignature.signature is required",
                )
            })?
            .to_owned();
        Ok(Self {
            protected,
            signature,
        })
    }
}

/// Configuration inputs used to construct a GAP gateway [`AgentCard`] (`CMP-020`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCardConfig {
    /// Public agent name.
    pub name: String,
    /// Concise summary (generated deterministically; never contains secrets).
    pub description: String,
    /// Agent version string (for example `"0.1.0"`).
    pub version: String,
    /// Exposed `supportedInterfaces`.
    pub supported_interfaces: Vec<AgentInterface>,
    /// Optional provider organization metadata.
    pub provider_organization: Option<String>,
    /// Optional provider website URL.
    pub provider_url: Option<String>,
}

impl Default for AgentCardConfig {
    fn default() -> Self {
        Self {
            name: "gap-gateway".to_owned(),
            description: "Deterministic typed Graph Agent Protocol gateway".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            supported_interfaces: vec![
                AgentInterface {
                    url: "http://127.0.0.1:8080".to_owned(),
                    protocol_binding: AgentProtocolBinding::HttpJson,
                    protocol_version: A2A_PROTOCOL_VERSION.to_owned(),
                    tenant: None,
                },
                AgentInterface {
                    url: "http://127.0.0.1:8080/a2a".to_owned(),
                    protocol_binding: AgentProtocolBinding::JsonRpc,
                    protocol_version: A2A_PROTOCOL_VERSION.to_owned(),
                    tenant: None,
                },
            ],
            provider_organization: Some("Graph Agent Protocol".to_owned()),
            provider_url: None,
        }
    }
}

/// Complete `A2A v1.0` `AgentCard` (`CMP-020`, `Q-12`, `Q-15`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCard {
    /// Agent name.
    pub name: String,
    /// Agent summary.
    pub description: String,
    /// Agent version string.
    pub version: String,
    /// Required default input media types.
    pub default_input_modes: Vec<String>,
    /// Required default output media types.
    pub default_output_modes: Vec<String>,
    /// Supported protocol bindings.
    pub supported_interfaces: Vec<AgentInterface>,
    /// Capability flags and declared extensions.
    pub capabilities: AgentCapabilities,
    /// Skills surfaced safely from pinned snapshot.
    pub skills: Vec<AgentSkill>,
    /// Optional provider organization metadata.
    pub provider_organization: Option<String>,
    /// Optional provider URL.
    pub provider_url: Option<String>,
    /// Optional inline detached JWS signatures over RFC 8785 JCS JSON (`Q-12`).
    pub signatures: Vec<AgentCardSignature>,
}

impl AgentCard {
    /// Serializes the unsigned payload (`signatures` omitted) in canonical RFC 8785 JCS JSON (`Q-12`).
    #[must_use]
    pub fn canonical_unsigned_json(&self) -> String {
        self.to_json_value_inner(false).to_jcs_string()
    }

    /// Serializes the full `AgentCard` (including `signatures[]` when present) in JCS JSON.
    #[must_use]
    pub fn to_json_string(&self) -> String {
        self.to_json_value().to_jcs_string()
    }

    /// Encodes the full `AgentCard` into JSON DOM.
    #[must_use]
    pub fn to_json_value(&self) -> JsonValue {
        self.to_json_value_inner(true)
    }

    fn to_json_value_inner(&self, include_signatures: bool) -> JsonValue {
        let mut obj = BTreeMap::new();
        obj.insert("name".to_owned(), JsonValue::String(self.name.clone()));
        obj.insert(
            "description".to_owned(),
            JsonValue::String(self.description.clone()),
        );
        obj.insert(
            "version".to_owned(),
            JsonValue::String(self.version.clone()),
        );
        obj.insert(
            "defaultInputModes".to_owned(),
            JsonValue::Array(
                self.default_input_modes
                    .iter()
                    .cloned()
                    .map(JsonValue::String)
                    .collect(),
            ),
        );
        obj.insert(
            "defaultOutputModes".to_owned(),
            JsonValue::Array(
                self.default_output_modes
                    .iter()
                    .cloned()
                    .map(JsonValue::String)
                    .collect(),
            ),
        );
        obj.insert(
            "supportedInterfaces".to_owned(),
            JsonValue::Array(
                self.supported_interfaces
                    .iter()
                    .map(AgentInterface::to_json_value)
                    .collect(),
            ),
        );
        obj.insert("capabilities".to_owned(), self.capabilities.to_json_value());
        obj.insert(
            "skills".to_owned(),
            JsonValue::Array(self.skills.iter().map(AgentSkill::to_json_value).collect()),
        );
        if self.provider_organization.is_some() || self.provider_url.is_some() {
            let mut prov = BTreeMap::new();
            if let Some(ref org) = self.provider_organization {
                prov.insert("organization".to_owned(), JsonValue::String(org.clone()));
            }
            if let Some(ref url) = self.provider_url {
                prov.insert("url".to_owned(), JsonValue::String(url.clone()));
            }
            obj.insert("provider".to_owned(), JsonValue::Object(prov));
        }
        if include_signatures && !self.signatures.is_empty() {
            obj.insert(
                "signatures".to_owned(),
                JsonValue::Array(
                    self.signatures
                        .iter()
                        .map(AgentCardSignature::to_json_value)
                        .collect(),
                ),
            );
        }
        JsonValue::Object(obj)
    }

    /// Parses an [`AgentCard`] from JSON DOM, validating all v1.0 required fields (`Q-12`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if any required `AgentCard` field is missing or invalid.
    pub fn from_json_value(val: &JsonValue) -> GapResult<Self> {
        let obj = val.as_object().ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                "AgentCard must be a JSON object",
            )
        })?;
        let name = obj
            .get("name")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "AgentCard.name is required"))?
            .to_owned();
        let description = obj
            .get("description")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "AgentCard.description is required",
                )
            })?
            .to_owned();
        let version = obj
            .get("version")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                GapError::new(ErrorCode::ValidationError, "AgentCard.version is required")
            })?
            .to_owned();

        let default_input_modes =
            crate::wire::parse_optional_string_list(obj.get("defaultInputModes"))?
                .into_iter()
                .collect::<Vec<_>>();
        if default_input_modes.is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "AgentCard.defaultInputModes is required and must be non-empty",
            ));
        }
        let default_output_modes =
            crate::wire::parse_optional_string_list(obj.get("defaultOutputModes"))?;
        if default_output_modes.is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "AgentCard.defaultOutputModes is required and must be non-empty",
            ));
        }

        let ifaces_arr = obj
            .get("supportedInterfaces")
            .and_then(JsonValue::as_array)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "AgentCard.supportedInterfaces is required and must be non-empty",
                )
            })?;
        let mut supported_interfaces = Vec::with_capacity(ifaces_arr.len());
        for iface in ifaces_arr {
            supported_interfaces.push(AgentInterface::from_json_value(iface)?);
        }

        let capabilities =
            AgentCapabilities::from_json_value(obj.get("capabilities").ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "AgentCard.capabilities is required",
                )
            })?)?;

        let skills_arr = obj
            .get("skills")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| {
                GapError::new(ErrorCode::ValidationError, "AgentCard.skills is required")
            })?;
        let mut skills = Vec::with_capacity(skills_arr.len());
        for skill in skills_arr {
            skills.push(AgentSkill::from_json_value(skill)?);
        }

        let (provider_organization, provider_url) = match obj.get("provider") {
            Some(p) => {
                let pmap = p.as_object().ok_or_else(|| {
                    GapError::new(
                        ErrorCode::ValidationError,
                        "AgentCard.provider must be object",
                    )
                })?;
                (
                    crate::wire::opt_string(pmap.get("organization"))?,
                    crate::wire::opt_string(pmap.get("url"))?,
                )
            }
            None => (None, None),
        };

        let mut signatures = Vec::new();
        if let Some(arr) = obj.get("signatures").and_then(JsonValue::as_array) {
            for sig in arr {
                signatures.push(AgentCardSignature::from_json_value(sig)?);
            }
        }

        Ok(Self {
            name,
            description,
            version,
            default_input_modes,
            default_output_modes,
            supported_interfaces,
            capabilities,
            skills,
            provider_organization,
            provider_url,
            signatures,
        })
    }
}

/// Builds a deterministic `A2A v1.0` [`AgentCard`] from the pinned [`Snapshot`] (`CMP-020`, `Q-12`).
///
/// Only `Active` branches at [`TrustTier::T0`] or [`TrustTier::T1`] contribute skill entries;
/// untrusted [`TrustTier::T2`] branch descriptions are strictly omitted (`INV-07`).
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] if `config.supported_interfaces` is empty or JWS signing fails.
pub fn build_agent_card(
    snapshot: &Snapshot,
    config: &AgentCardConfig,
    signing_key: Option<(&str, &Ed25519SigningKey)>,
) -> GapResult<AgentCard> {
    if config.supported_interfaces.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "AgentCardConfig.supported_interfaces must be non-empty",
        ));
    }

    let mut skills = vec![
        AgentSkill {
            id: "gap.resolve".to_owned(),
            name: "GAP Intent Resolution".to_owned(),
            description: format!(
                "Resolve natural-language intent into a bounded candidate subgraph on snapshot generation {}",
                snapshot.generation
            ),
            tags: vec![
                "gap".to_owned(),
                "discovery".to_owned(),
                "resolve".to_owned(),
            ],
        },
        AgentSkill {
            id: "gap.gapql".to_owned(),
            name: "GAP-QL Execution".to_owned(),
            description: "Execute typed GAP-QL documents asynchronously against composed branches"
                .to_owned(),
            tags: vec!["gap".to_owned(), "gapql".to_owned(), "task".to_owned()],
        },
    ];

    // Sort trusted branches deterministically by slug (`id`) and exclude all T2 branches.
    let mut trusted_branches: Vec<_> = snapshot
        .branches
        .values()
        .filter(|b| b.status == BranchStatus::Active && !matches!(b.trust, TrustTier::T2))
        .collect();
    trusted_branches.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));

    for branch in trusted_branches {
        let trust_label = match branch.trust {
            TrustTier::T0 => "t0",
            TrustTier::T1 => "t1",
            TrustTier::T2 => continue,
        };
        let proto_label = format!("{:?}", branch.protocol).to_ascii_lowercase();
        let description = if branch.synopsis.is_empty() {
            format!(
                "Trusted {} branch `{}` exposing {} operation(s)",
                proto_label,
                branch.id.as_str(),
                branch.operations.len()
            )
        } else {
            branch.synopsis.clone()
        };
        skills.push(AgentSkill {
            id: format!("branch.{}", branch.id.as_str()),
            name: format!("Branch {}", branch.id.as_str()),
            description,
            tags: vec![
                "gap".to_owned(),
                "branch".to_owned(),
                proto_label,
                trust_label.to_owned(),
            ],
        });
    }

    let mut card = AgentCard {
        name: config.name.clone(),
        description: config.description.clone(),
        version: config.version.clone(),
        default_input_modes: vec!["application/json".to_owned(), "text/plain".to_owned()],
        default_output_modes: vec!["application/json".to_owned(), "text/plain".to_owned()],
        supported_interfaces: config.supported_interfaces.clone(),
        capabilities: AgentCapabilities::default(),
        skills,
        provider_organization: config.provider_organization.clone(),
        provider_url: config.provider_url.clone(),
        signatures: Vec::new(),
    };

    if let Some((kid, sk)) = signing_key {
        let sig = sign_agent_card(&card, kid, sk)?;
        card.signatures.push(sig);
    }

    Ok(card)
}

/// Produces an inline detached JWS signature over the card's canonical JCS unsigned payload (`Q-12`).
///
/// # Errors
///
/// Returns [`gap_core::error::GapError`] if signing fails.
pub fn sign_agent_card(
    card: &AgentCard,
    kid: &str,
    signing_key: &Ed25519SigningKey,
) -> GapResult<AgentCardSignature> {
    let mut hdr = BTreeMap::new();
    hdr.insert("alg".to_owned(), JsonValue::String("EdDSA".to_owned()));
    hdr.insert("kid".to_owned(), JsonValue::String(kid.to_owned()));
    let protected = base64url_encode(JsonValue::Object(hdr).to_jcs_string().as_bytes());
    let payload_b64 = base64url_encode(card.canonical_unsigned_json().as_bytes());
    let signing_input = format!("{protected}.{payload_b64}");
    let raw_sig = signing_key.sign(signing_input.as_bytes())?;
    Ok(AgentCardSignature {
        protected,
        signature: base64url_encode(&raw_sig),
    })
}

/// Verifies every inline JWS signature on `card` against `verifying_key` (`Q-12`).
///
/// Returns `Ok(true)` if `card.signatures` is non-empty and at least one signature verifies
/// cleanly; returns `Err` on malformed signature data or invalid cryptographic verification.
///
/// # Errors
///
/// Returns [`ErrorCode::Unauthenticated`] when signatures are missing, malformed, or invalid.
pub fn verify_agent_card_signatures(
    card: &AgentCard,
    verifying_key: &Ed25519VerifyingKey,
) -> GapResult<bool> {
    if card.signatures.is_empty() {
        return Err(GapError::new(
            ErrorCode::Unauthenticated,
            "AgentCard carries no inline signatures",
        ));
    }
    let payload_b64 = base64url_encode(card.canonical_unsigned_json().as_bytes());
    for entry in &card.signatures {
        let protected_bytes = base64url_decode(&entry.protected)?;
        let protected_str = std::str::from_utf8(&protected_bytes).map_err(|_| {
            GapError::new(
                ErrorCode::ValidationError,
                "JWS protected header is not UTF-8",
            )
        })?;
        let protected_json = JsonValue::parse(protected_str)?;
        if protected_json.get_str("alg") != Some("EdDSA") {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "AgentCard JWS alg must be EdDSA",
            ));
        }
        let sig_bytes = base64url_decode(&entry.signature)?;
        let signing_input = format!("{}.{payload_b64}", entry.protected);
        verifying_key.verify(signing_input.as_bytes(), &sig_bytes)?;
    }
    Ok(true)
}

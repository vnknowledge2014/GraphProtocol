//! Validated identifier newtypes, tenant scope token, and `GQL-004` reserved-name checks (`L0`).

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::{Builder, Uuid, Variant, Version};

/// Validation failure returned when constructing or deserializing a GAP identifier.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdValidationError {
    /// Input was empty or violated the slug rule `[a-z][a-z0-9_-]{1,62}`.
    #[error("invalid {kind} slug `{value}`: expected `[a-z][a-z0-9_-]{{1,62}}`")]
    InvalidSlug {
        /// Identifier type name.
        kind: &'static str,
        /// Rejected input string.
        value: String,
    },
    /// Input was not a valid UUID string.
    #[error("invalid {kind} UUID `{value}`")]
    InvalidUuid {
        /// Identifier type name.
        kind: &'static str,
        /// Rejected input string.
        value: String,
    },
    /// UUID parsed, but its version/variant was not RFC 9562 UUIDv7.
    #[error("invalid {kind} UUID `{value}`: expected RFC 9562 version 7")]
    NotUuidV7 {
        /// Identifier type name.
        kind: &'static str,
        /// Rejected UUID value.
        value: Uuid,
    },
    /// Input violated `<branch>.<name>` format or `GQL-004` name constraints.
    #[error("invalid OperationId `{value}`: {reason}")]
    InvalidOperationId {
        /// Rejected input string.
        value: String,
        /// Specific reason for failure.
        reason: &'static str,
    },
    /// Input violated the 32-char lowercase hex rule for `blake3-128` `GroupId`.
    #[error("invalid GroupId `{value}`: expected 32 lowercase hex characters `[0-9a-f]{{32}}`")]
    InvalidGroupId {
        /// Rejected input string.
        value: String,
    },
    /// Input violated `[A-Za-z0-9._:-]{1,128}` for `IdempotencyKey`.
    #[error("invalid IdempotencyKey `{value}`: expected 1..=128 ASCII `[A-Za-z0-9._:-]` chars")]
    InvalidIdempotencyKey {
        /// Rejected input string.
        value: String,
    },
}

/// Default single-tenant slug (`ADR-018`).
pub const DEFAULT_TENANT_SLUG: &str = "default";

/// Minimum length (bytes) of a `TenantId` or `BranchId` slug (`spec/00-conventions.md` §7).
pub const MIN_SLUG_LEN: usize = 2;

/// Maximum length (bytes) of a `TenantId` or `BranchId` slug (`spec/00-conventions.md` §7).
pub const MAX_SLUG_LEN: usize = 63;

/// Exact hex string length of a 128-bit `blake3-128` `GroupId` (`spec/00-conventions.md` §7).
pub const GROUP_ID_HEX_LEN: usize = 32;

/// Maximum length (bytes) of the `<name>` segment in `<branch>.<name>` (`OperationId`).
pub const MAX_OPERATION_NAME_LEN: usize = 128;

/// Maximum length (bytes) of an [`IdempotencyKey`].
pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 128;

/// Built-in GraphQL root field names reserved by GAP (`GQL-004`, `spec/02-gap-ql.md` §§1, 3, 4).
pub const RESERVED_ROOT_FIELDS: &[&str] = &["gap", "__search", "__metaGraph", "gapTaskEvents"];

/// Built-in GraphQL type, input, scalar, and enum names reserved by GAP (`GQL-004`, `spec/02-gap-ql.md` §3–§5).
pub const RESERVED_BUILTIN_TYPES: &[&str] = &[
    "DateTime",
    "JSON",
    "Cursor",
    "Direction",
    "TraverseStrategy",
    "ContextScope",
    "ElementKind",
    "ResolveOutcome",
    "AbstentionReason",
    "SideEffectLevel",
    "TrustTier",
    "BranchStatus",
    "Protocol",
    "TaskState",
    "TaskStrategy",
    "ApprovalDecision",
    "GapQuery",
    "GapMutation",
    "ResolveConstraints",
    "Resolution",
    "Candidate",
    "OperationRef",
    "NodeRef",
    "NodeKey",
    "EdgeFilter",
    "TraverseInput",
    "Predicate",
    "Compare",
    "CompareList",
    "Subgraph",
    "GapNode",
    "GapEdge",
    "EdgeResolution",
    "TraverseStats",
    "PageInfo",
    "Provenance",
    "SubmitTaskInput",
    "TaskStepInput",
    "DelegateInput",
    "Task",
    "TaskStep",
    "Artifact",
    "Approval",
    "TaskError",
    "TaskEvent",
    "TaskConnection",
    "TaskFilter",
    "MetaGraph",
    "MetaBranch",
    "PlanExplanation",
    "ProposeSourceInput",
    "ProposalStatus",
    "Proposal",
    "ProposalConnection",
    "SearchResult",
    "SearchHit",
    "JoinKey",
    "Cardinality",
    "JoinStrategy",
    "JoinStatus",
    "Format",
];

/// Standard GraphQL introspection identifiers permitted to start with `__` (`GQL-004`).
pub const STANDARD_INTROSPECTION_NAMES: &[&str] = &[
    "__schema",
    "__type",
    "__typename",
    "__Schema",
    "__Type",
    "__TypeKind",
    "__Field",
    "__InputValue",
    "__EnumValue",
    "__Directive",
    "__DirectiveLocation",
    "__search",
    "__metaGraph",
];

fn is_valid_slug(s: &str) -> bool {
    let bytes = s.as_bytes();
    if !(MIN_SLUG_LEN..=MAX_SLUG_LEN).contains(&bytes.len()) {
        return false;
    }
    if !bytes[0].is_ascii_lowercase() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

/// Returns `true` if `name` matches GraphQL's `[_A-Za-z][_0-9A-Za-z]*` lexical grammar (`NGC-012`).
#[must_use]
pub fn is_valid_graphql_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let Some((&first, rest)) = bytes.split_first() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    rest.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// Returns `true` if `name` is a valid GraphQL identifier and obeys `GQL-004` (`__` prohibited
/// except standard/GAP introspection identifiers).
#[must_use]
pub fn is_allowed_graphql_identifier(name: &str) -> bool {
    if !is_valid_graphql_name(name) {
        return false;
    }
    if name.starts_with("__") {
        return STANDARD_INTROSPECTION_NAMES.contains(&name);
    }
    true
}

/// Returns `true` if `name` collides with a GAP reserved root field (`GQL-004`).
#[must_use]
pub fn is_reserved_root_field(name: &str) -> bool {
    RESERVED_ROOT_FIELDS.contains(&name)
}

/// Returns `true` if `name` collides with a GAP built-in GraphQL type (`GQL-004`).
#[must_use]
pub fn is_reserved_builtin_type(name: &str) -> bool {
    RESERVED_BUILTIN_TYPES.contains(&name)
}

fn validate_uuid_v7(kind: &'static str, uuid: Uuid) -> Result<Uuid, IdValidationError> {
    if uuid.get_variant() == Variant::RFC4122 && uuid.get_version() == Some(Version::SortRand) {
        Ok(uuid)
    } else {
        Err(IdValidationError::NotUuidV7 { kind, value: uuid })
    }
}

macro_rules! define_slug_id {
    ($(#[$meta:meta])* $vis:vis struct $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        $vis struct $name(String);

        impl $name {
            /// Validates and constructs a new identifier matching `[a-z][a-z0-9_-]{1,62}`.
            pub fn new(raw: impl Into<String>) -> Result<Self, IdValidationError> {
                let value = raw.into();
                if is_valid_slug(&value) {
                    Ok(Self(value))
                } else {
                    Err(IdValidationError::InvalidSlug {
                        kind: $kind,
                        value,
                    })
                }
            }

            /// Borrows the validated slug string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdValidationError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdValidationError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdValidationError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed($kind)
            }

            fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
                json_schema!({
                    "type": "string",
                    "minLength": MIN_SLUG_LEN,
                    "maxLength": MAX_SLUG_LEN,
                    "pattern": "^[a-z][a-z0-9_-]{1,62}$"
                })
            }
        }
    };
}

define_slug_id!(
    /// Validated tenant isolation identifier (`[a-z][a-z0-9_-]{1,62}`, `ADR-018`, `INV-09`).
    pub struct TenantId,
    "TenantId"
);

impl TenantId {
    /// Returns the constant default tenant used by single-tenant deployments (`ADR-018`).
    #[must_use]
    pub fn default_tenant() -> Self {
        Self(DEFAULT_TENANT_SLUG.to_owned())
    }

    /// Mints a [`TenantScope`] token bound to this tenant (`ADR-018`, `INV-09`, `INV-19`).
    #[must_use]
    pub fn scope(&self) -> TenantScope {
        TenantScope(self.clone())
    }
}

impl Default for TenantId {
    fn default() -> Self {
        Self::default_tenant()
    }
}

/// Type-level tenant scope token required by tenant-partitioned store and cache APIs (`ADR-018`, `INV-09`, `INV-19`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TenantScope(TenantId);

impl TenantScope {
    /// Creates a [`TenantScope`] from a validated [`TenantId`].
    #[must_use]
    pub fn new(tenant: TenantId) -> Self {
        Self(tenant)
    }

    /// Returns the bound [`TenantId`].
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.0
    }
}

define_slug_id!(
    /// Human-readable branch slug (`[a-z][a-z0-9_-]{1,62}`, `spec/00-conventions.md` §7).
    pub struct BranchId,
    "BranchId"
);

macro_rules! define_uuid_v7_id {
    ($(#[$meta:meta])* $vis:vis struct $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        $vis struct $name(Uuid);

        impl $name {
            /// Generates a new monotonic RFC 9562 UUIDv7 identifier from system time.
            #[must_use]
            pub fn new_v7() -> Self {
                Self(Uuid::now_v7())
            }

            /// Deterministically constructs an RFC 9562 UUIDv7 from Unix milliseconds and 10 random bytes.
            #[must_use]
            pub fn from_unix_ms(unix_ms: u64, random_bytes: [u8; 10]) -> Self {
                Self(Builder::from_unix_timestamp_millis(unix_ms, &random_bytes).into_uuid())
            }

            /// Validates that `uuid` is RFC 9562 UUIDv7.
            pub fn from_uuid(uuid: Uuid) -> Result<Self, IdValidationError> {
                validate_uuid_v7($kind, uuid).map(Self)
            }

            /// Parses and validates a UUIDv7 string.
            pub fn parse(raw: &str) -> Result<Self, IdValidationError> {
                let parsed = Uuid::parse_str(raw).map_err(|_| IdValidationError::InvalidUuid {
                    kind: $kind,
                    value: raw.to_owned(),
                })?;
                Self::from_uuid(parsed)
            }

            /// Returns the underlying [`Uuid`].
            #[must_use]
            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = IdValidationError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdValidationError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdValidationError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(&value)
            }
        }

        impl TryFrom<Uuid> for $name {
            type Error = IdValidationError;

            fn try_from(value: Uuid) -> Result<Self, Self::Error> {
                Self::from_uuid(value)
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0.to_string()
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed($kind)
            }

            fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
                json_schema!({
                    "type": "string",
                    "format": "uuid",
                    "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-7[0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}$"
                })
            }
        }
    };
}

define_uuid_v7_id!(
    /// Generation-stable branch UUIDv7 (`spec/00-conventions.md` §7, `ADR-009` §7).
    pub struct BranchUid,
    "BranchUid"
);

define_uuid_v7_id!(
    /// Durable A2A-compatible task UUIDv7 (`spec/00-conventions.md` §7, `ADR-016`).
    pub struct TaskId,
    "TaskId"
);

define_uuid_v7_id!(
    /// Ingestion proposal UUIDv7 (`spec/00-conventions.md` §7, `ADR-013`).
    pub struct ProposalId,
    "ProposalId"
);

define_uuid_v7_id!(
    /// Mutation/task approval gate UUIDv7 (`spec/00-conventions.md` §7, `ADR-015`).
    pub struct ApprovalId,
    "ApprovalId"
);

/// Qualified operation identifier `<branch>.<name>` (`spec/00-conventions.md` §7, `NGC-012`, `GQL-004`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OperationId {
    raw: String,
    dot_index: usize,
}

impl OperationId {
    /// Validates and constructs an [`OperationId`] of the form `<branch>.<name>`.
    pub fn new(raw: impl Into<String>) -> Result<Self, IdValidationError> {
        let value = raw.into();
        let Some((branch_part, name_part)) = value.split_once('.') else {
            return Err(IdValidationError::InvalidOperationId {
                value,
                reason: "missing `.` separator between `<branch>` and `<name>`",
            });
        };
        if name_part.contains('.') {
            return Err(IdValidationError::InvalidOperationId {
                value,
                reason: "unexpected extra `.` in `<name>`",
            });
        }
        if !is_valid_slug(branch_part) {
            return Err(IdValidationError::InvalidOperationId {
                value,
                reason: "`<branch>` must match `[a-z][a-z0-9_-]{1,62}`",
            });
        }
        if name_part.is_empty() || name_part.len() > MAX_OPERATION_NAME_LEN {
            return Err(IdValidationError::InvalidOperationId {
                value,
                reason: "`<name>` length must be in 1..=128 bytes",
            });
        }
        if !is_valid_graphql_name(name_part) {
            return Err(IdValidationError::InvalidOperationId {
                value,
                reason: "`<name>` must match `[A-Za-z_][A-Za-z0-9_]*`",
            });
        }
        if name_part.starts_with("__") {
            return Err(IdValidationError::InvalidOperationId {
                value,
                reason: "`<name>` must not begin with reserved `__` prefix (GQL-004)",
            });
        }
        let dot_index = branch_part.len();
        Ok(Self {
            raw: value,
            dot_index,
        })
    }

    /// Constructs an [`OperationId`] from a validated [`BranchId`] and an operation `name`.
    pub fn from_parts(branch: &BranchId, name: &str) -> Result<Self, IdValidationError> {
        Self::new(format!("{branch}.{name}"))
    }

    /// Returns the full `<branch>.<name>` string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Returns the `<branch>` slug slice.
    #[must_use]
    pub fn branch_str(&self) -> &str {
        &self.raw[..self.dot_index]
    }

    /// Returns the validated [`BranchId`] prefix.
    #[must_use]
    pub fn branch(&self) -> BranchId {
        BranchId(self.branch_str().to_owned())
    }

    /// Returns the `<name>` segment slice.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.raw[self.dot_index + 1..]
    }
}

impl AsRef<str> for OperationId {
    fn as_ref(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl FromStr for OperationId {
    type Err = IdValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<&str> for OperationId {
    type Error = IdValidationError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for OperationId {
    type Error = IdValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<OperationId> for String {
    fn from(id: OperationId) -> Self {
        id.raw
    }
}

impl JsonSchema for OperationId {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("OperationId")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[a-z][a-z0-9_-]{1,62}\\.(?!__)[A-Za-z_][A-Za-z0-9_]{0,127}$"
        })
    }
}

/// Capability group identifier: 32 lowercase hex characters (`blake3-128`, `spec/00-conventions.md` §7, `ADR-006`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GroupId(String);

impl GroupId {
    /// Validates and constructs a [`GroupId`] matching `[0-9a-f]{32}`.
    pub fn new(raw: impl Into<String>) -> Result<Self, IdValidationError> {
        let value = raw.into();
        let bytes = value.as_bytes();
        let valid = bytes.len() == GROUP_ID_HEX_LEN
            && bytes
                .iter()
                .all(|b| b.is_ascii_digit() || (*b >= b'a' && *b <= b'f'));
        if valid {
            Ok(Self(value))
        } else {
            Err(IdValidationError::InvalidGroupId { value })
        }
    }

    /// Formats a 16-byte (`128`-bit) `blake3-128` digest as a lowercase hex [`GroupId`].
    #[must_use]
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(GROUP_ID_HEX_LEN);
        for byte in bytes {
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Self(out)
    }

    /// Decodes the 32-char lowercase hex string back into its 16 raw digest bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 16] {
        let ascii = self.0.as_bytes();
        let mut out = [0u8; 16];
        for (idx, slot) in out.iter_mut().enumerate() {
            let hi = decode_lower_hex_nibble(ascii[idx * 2]);
            let lo = decode_lower_hex_nibble(ascii[idx * 2 + 1]);
            *slot = (hi << 4) | lo;
        }
        out
    }

    /// Borrows the 32-char lowercase hex string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn decode_lower_hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => 0,
    }
}

impl AsRef<str> for GroupId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for GroupId {
    type Err = IdValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<&str> for GroupId {
    type Error = IdValidationError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for GroupId {
    type Error = IdValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<GroupId> for String {
    fn from(id: GroupId) -> Self {
        id.0
    }
}

impl JsonSchema for GroupId {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("GroupId")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": GROUP_ID_HEX_LEN,
            "maxLength": GROUP_ID_HEX_LEN,
            "pattern": "^[0-9a-f]{32}$"
        })
    }
}

/// Validated mutation/task idempotency key (`[A-Za-z0-9._:-]{1,128}`, `ADR-015`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// Validates and constructs an [`IdempotencyKey`].
    pub fn new(raw: impl Into<String>) -> Result<Self, IdValidationError> {
        let value = raw.into();
        let bytes = value.as_bytes();
        let valid = (1..=MAX_IDEMPOTENCY_KEY_LEN).contains(&bytes.len())
            && bytes.iter().all(|b| {
                b.is_ascii_alphanumeric() || *b == b'.' || *b == b'_' || *b == b':' || *b == b'-'
            });
        if valid {
            Ok(Self(value))
        } else {
            Err(IdValidationError::InvalidIdempotencyKey { value })
        }
    }

    /// Generates a new UUIDv7-backed [`IdempotencyKey`].
    #[must_use]
    pub fn generate_v7() -> Self {
        Self(Uuid::now_v7().to_string())
    }

    /// Borrows the validated key string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for IdempotencyKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for IdempotencyKey {
    type Err = IdValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<&str> for IdempotencyKey {
    type Error = IdValidationError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for IdempotencyKey {
    type Error = IdValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<IdempotencyKey> for String {
    fn from(key: IdempotencyKey) -> Self {
        key.0
    }
}

impl JsonSchema for IdempotencyKey {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("IdempotencyKey")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_IDEMPOTENCY_KEY_LEN,
            "pattern": "^[A-Za-z0-9._:-]{1,128}$"
        })
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    // spec: INV-09 (ADR-018)
    #[test]
    fn inv_09_tenant_id_accepts_valid_slug_and_mints_scope() {
        let default_tenant = TenantId::default();
        assert_eq!(default_tenant.as_str(), "default");

        let tenant = TenantId::new("acme-prod_01").expect("valid slug");
        assert_eq!(tenant.as_str(), "acme-prod_01");
        assert_eq!(tenant.scope().tenant(), &tenant);

        for bad in ["", "a", "Acme", "9tenant", "bad.slug", &"a".repeat(64)] {
            assert!(
                TenantId::new(bad).is_err(),
                "expected rejection for `{bad}`"
            );
            let json = serde_json::to_string(bad).expect("json string");
            assert!(serde_json::from_str::<TenantId>(&json).is_err());
        }
    }

    // spec: GQL-004
    #[test]
    fn gql_004_reserved_names_and_operation_ids_are_enforced() {
        assert!(is_reserved_root_field("gap"));
        assert!(is_reserved_root_field("__search"));
        assert!(is_reserved_root_field("__metaGraph"));
        assert!(is_reserved_root_field("gapTaskEvents"));
        assert!(!is_reserved_root_field("billing"));

        assert!(is_reserved_builtin_type("GapQuery"));
        assert!(is_reserved_builtin_type("Subgraph"));
        assert!(is_reserved_builtin_type("SearchResult"));
        assert!(!is_reserved_builtin_type("Invoice"));

        assert!(!is_allowed_graphql_identifier("__customSecret"));
        assert!(is_allowed_graphql_identifier("__typename"));
        assert!(is_allowed_graphql_identifier("__search"));
        assert!(is_allowed_graphql_identifier("invoiceTotal"));

        let op = OperationId::new("billing.getInvoice_1").expect("valid op");
        assert_eq!(op.branch().as_str(), "billing");
        assert_eq!(op.name(), "getInvoice_1");
        assert_eq!(
            OperationId::from_parts(&BranchId::new("billing").expect("branch"), "getInvoice_1")
                .expect("from_parts"),
            op
        );

        assert!(OperationId::new("billing.__hidden").is_err());
        assert!(OperationId::new("billing.a.b").is_err());
        assert!(OperationId::new("Billing.get").is_err());
        assert!(OperationId::new("billing.1bad").is_err());
        assert!(OperationId::new("no_dot").is_err());
    }

    // spec: ARC-005 (uuidv7 stability across generations)
    #[test]
    fn uuid_v7_ids_reject_v4_nil_and_invalid_strings() {
        let v4 = Uuid::new_v4();
        let nil = Uuid::nil();

        assert!(BranchUid::from_uuid(v4).is_err());
        assert!(BranchUid::from_uuid(nil).is_err());
        assert!(TaskId::from_uuid(v4).is_err());
        assert!(ProposalId::from_uuid(v4).is_err());
        assert!(ApprovalId::from_uuid(v4).is_err());

        let v4_json = serde_json::to_string(&v4.to_string()).expect("json");
        assert!(serde_json::from_str::<BranchUid>(&v4_json).is_err());

        let uid = BranchUid::new_v7();
        let encoded = serde_json::to_string(&uid).expect("serialize");
        let decoded: BranchUid = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, uid);
    }

    proptest! {
        // spec: INV-09, GQL-004 (ADR-018)
        #[test]
        fn prop_slug_ids_roundtrip(slug in "[a-z][a-z0-9_-]{1,62}") {
            let tenant = TenantId::from_str(&slug).expect("valid TenantId");
            prop_assert_eq!(tenant.to_string(), slug.as_str());
            let json = serde_json::to_string(&tenant).expect("serialize");
            let back: TenantId = serde_json::from_str(&json).expect("deserialize");
            prop_assert_eq!(back, tenant);

            let branch = BranchId::from_str(&slug).expect("valid BranchId");
            prop_assert_eq!(branch.to_string(), slug.as_str());
            let bjson = serde_json::to_string(&branch).expect("serialize");
            let bback: BranchId = serde_json::from_str(&bjson).expect("deserialize");
            prop_assert_eq!(bback, branch);
        }

        // spec: INV-09 (ADR-018)
        #[test]
        fn prop_slug_ids_reject_invalid(raw in "\\PC*") {
            prop_assume!(!is_valid_slug(&raw));
            prop_assert!(TenantId::new(&raw).is_err());
            prop_assert!(BranchId::new(&raw).is_err());
        }

        // spec: GQL-004
        #[test]
        fn prop_operation_id_roundtrip(
            branch in "[a-z][a-z0-9_-]{1,62}",
            name in "[A-Za-z][A-Za-z0-9_]{0,63}"
        ) {
            let full = format!("{branch}.{name}");
            let op = OperationId::new(&full).expect("valid OperationId");
            prop_assert_eq!(op.branch_str(), branch.as_str());
            prop_assert_eq!(op.name(), name.as_str());
            let json = serde_json::to_string(&op).expect("serialize");
            let back: OperationId = serde_json::from_str(&json).expect("deserialize");
            prop_assert_eq!(back, op);
        }

        // spec: GQL-004
        #[test]
        fn prop_operation_id_rejects_arbitrary_invalid(raw in "\\PC*") {
            let valid = raw.split_once('.').is_some_and(|(b, n)| {
                !n.contains('.')
                    && is_valid_slug(b)
                    && (1..=MAX_OPERATION_NAME_LEN).contains(&n.len())
                    && is_valid_graphql_name(n)
                    && !n.starts_with("__")
            });
            prop_assume!(!valid);
            prop_assert!(OperationId::new(&raw).is_err());
        }

        // spec: ARC-005
        #[test]
        fn prop_group_id_roundtrip_and_invalid_rejection(
            bytes in prop::array::uniform16(any::<u8>()),
            bad in "\\PC*"
        ) {
            let gid = GroupId::from_bytes(bytes);
            prop_assert_eq!(gid.to_bytes(), bytes);
            let parsed = GroupId::from_str(gid.as_str()).expect("valid GroupId");
            prop_assert_eq!(&parsed, &gid);
            let json = serde_json::to_string(&gid).expect("serialize");
            let back: GroupId = serde_json::from_str(&json).expect("deserialize");
            prop_assert_eq!(back, gid);

            let bad_is_valid = bad.len() == GROUP_ID_HEX_LEN
                && bad.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !bad_is_valid {
                prop_assert!(GroupId::new(&bad).is_err());
            }
        }

        // spec: ARC-005
        #[test]
        fn prop_uuid_v7_ids_roundtrip(
            unix_ms in 0u64..(1u64 << 48),
            rand_bytes in prop::array::uniform10(any::<u8>())
        ) {
            let buid = BranchUid::from_unix_ms(unix_ms, rand_bytes);
            let tid = TaskId::from_unix_ms(unix_ms, rand_bytes);
            let pid = ProposalId::from_unix_ms(unix_ms, rand_bytes);
            let aid = ApprovalId::from_unix_ms(unix_ms, rand_bytes);

            prop_assert_eq!(BranchUid::from_str(&buid.to_string()).expect("parse"), buid);
            prop_assert_eq!(TaskId::from_str(&tid.to_string()).expect("parse"), tid);
            prop_assert_eq!(ProposalId::from_str(&pid.to_string()).expect("parse"), pid);
            prop_assert_eq!(ApprovalId::from_str(&aid.to_string()).expect("parse"), aid);
        }

        // spec: ARC-010
        #[test]
        fn prop_idempotency_key_roundtrip(key in "[A-Za-z0-9._:-]{1,64}") {
            let ik = IdempotencyKey::new(&key).expect("valid IdempotencyKey");
            let json = serde_json::to_string(&ik).expect("serialize");
            let back: IdempotencyKey = serde_json::from_str(&json).expect("deserialize");
            prop_assert_eq!(back, ik);
        }
    }
}

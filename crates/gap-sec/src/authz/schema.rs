//! Cedar schema parsing and structural validation (`SEC-020`, `SEC-021`, `SEC-022`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use gap_core::error::{ErrorCode, GapError, GapResult};

/// Canonical Cedar schema embedded as fallback default from `policies/schema.cedarschema`.
pub const DEFAULT_CEDAR_SCHEMA: &str = include_str!("../../../../policies/schema.cedarschema");

/// Default Cedar policy set embedded from `policies/default.cedar`.
pub const DEFAULT_CEDAR_POLICIES: &str = include_str!("../../../../policies/default.cedar");

/// GAP Cedar authorization actions (`SEC-021`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuthzAction {
    /// Progressive-disclosure element discovery & introspection (`Discover`).
    Discover,
    /// Query execution per touched field/operation including RHS join fields (`Execute`).
    Execute,
    /// Context-graph node/edge traversal (`Traverse`).
    Traverse,
    /// Side-effecting branch mutation (`Mutate`).
    Mutate,
    /// Asynchronous task submission (`SubmitTask`).
    SubmitTask,
    /// Human approval decision (`DecideApproval`).
    DecideApproval,
    /// Asynchronous task cancellation (`CancelTask`).
    CancelTask,
    /// Ingestion proposal submission (`ProposeSource`).
    ProposeSource,
    /// Ingestion proposal approval/rejection (`DecideProposal`).
    DecideProposal,
    /// Schema/supergraph administration (`AdminSchema`).
    AdminSchema,
    /// Ingestion administration (`AdminIngest`).
    AdminIngest,
    /// Tenant impersonation privilege (`AdminImpersonate`).
    AdminImpersonate,
    /// Cedar policy management (`AdminPolicy`).
    AdminPolicy,
}

impl AuthzAction {
    /// Canonical Cedar `Action::"..."` literal.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discover => "Discover",
            Self::Execute => "Execute",
            Self::Traverse => "Traverse",
            Self::Mutate => "Mutate",
            Self::SubmitTask => "SubmitTask",
            Self::DecideApproval => "DecideApproval",
            Self::CancelTask => "CancelTask",
            Self::ProposeSource => "ProposeSource",
            Self::DecideProposal => "DecideProposal",
            Self::AdminSchema => "AdminSchema",
            Self::AdminIngest => "AdminIngest",
            Self::AdminImpersonate => "AdminImpersonate",
            Self::AdminPolicy => "AdminPolicy",
        }
    }

    /// Parses an action identifier from Cedar syntax (`SEC-021`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] when `raw` is not a recognized GAP action.
    pub fn parse(raw: &str) -> GapResult<Self> {
        match raw {
            "Discover" => Ok(Self::Discover),
            "Execute" => Ok(Self::Execute),
            "Traverse" => Ok(Self::Traverse),
            "Mutate" => Ok(Self::Mutate),
            "SubmitTask" => Ok(Self::SubmitTask),
            "DecideApproval" => Ok(Self::DecideApproval),
            "CancelTask" => Ok(Self::CancelTask),
            "ProposeSource" => Ok(Self::ProposeSource),
            "DecideProposal" => Ok(Self::DecideProposal),
            "AdminSchema" => Ok(Self::AdminSchema),
            "AdminIngest" => Ok(Self::AdminIngest),
            "AdminImpersonate" => Ok(Self::AdminImpersonate),
            "AdminPolicy" => Ok(Self::AdminPolicy),
            _ => Err(GapError::new(
                ErrorCode::ValidationError,
                format!("unknown Cedar action `{raw}` in policy"),
            )),
        }
    }

    /// Returns `true` for privileged actions logged at 100% (`Mutate`, `Admin*`, `Decide*`; `SEC-024`).
    #[must_use]
    pub const fn is_always_logged_on_allow(self) -> bool {
        matches!(
            self,
            Self::Mutate
                | Self::DecideApproval
                | Self::DecideProposal
                | Self::AdminSchema
                | Self::AdminIngest
                | Self::AdminImpersonate
                | Self::AdminPolicy
        )
    }
}

impl fmt::Display for AuthzAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Resource kind targeted by an authorization evaluation (`SEC-021`, `SEC-022`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceKind {
    /// Discovery or graph schema element (`Element`).
    Element,
    /// Callable branch operation (`Operation`).
    Operation,
    /// Registered upstream branch (`Branch`).
    Branch,
    /// Tenant scope entity (`Tenant`).
    Tenant,
    /// Durable execution task (`Task`).
    Task,
    /// Human approval gate record (`Approval`).
    Approval,
    /// Control-plane ingestion proposal (`Proposal`).
    Proposal,
}

impl ResourceKind {
    /// Canonical Cedar entity type name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Element => "Element",
            Self::Operation => "Operation",
            Self::Branch => "Branch",
            Self::Tenant => "Tenant",
            Self::Task => "Task",
            Self::Approval => "Approval",
            Self::Proposal => "Proposal",
        }
    }
}

/// Attribute field type declared in a `.cedarschema` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaAttrType {
    /// UTF-8 string value.
    String,
    /// Signed 64-bit integer value.
    Long,
    /// Boolean value.
    Bool,
    /// Set of UTF-8 strings (`Set<String>`).
    SetString,
}

impl SchemaAttrType {
    fn parse(raw: &str) -> GapResult<Self> {
        let trimmed = raw.trim().trim_end_matches(';').trim();
        match trimmed {
            "String" => Ok(Self::String),
            "Long" => Ok(Self::Long),
            "Bool" => Ok(Self::Bool),
            "Set<String>" => Ok(Self::SetString),
            other => Err(GapError::new(
                ErrorCode::ValidationError,
                format!("unsupported Cedar schema attribute type `{other}`"),
            )),
        }
    }
}

/// Parsed and validated Cedar schema (`policies/schema.cedarschema`, `SEC-020`, `SEC-022`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CedarSchema {
    /// Entity declarations mapping entity type -> attribute type map.
    pub entities: BTreeMap<String, BTreeMap<String, SchemaAttrType>>,
    /// Entity parent declarations (`entity Element in [Branch]`).
    pub entity_parents: BTreeMap<String, BTreeSet<String>>,
    /// Declared actions mapped to allowed principal/resource entity types.
    pub actions: BTreeSet<String>,
    /// Union of valid principal attribute types.
    pub principal_attrs: BTreeMap<String, SchemaAttrType>,
    /// Union of valid resource attribute types across all non-principal/non-group entities.
    pub resource_attrs: BTreeMap<String, SchemaAttrType>,
    /// Context attribute schema shared across actions.
    pub context_attrs: BTreeMap<String, SchemaAttrType>,
    /// Canonical source text normalized at parse time.
    pub canonical_text: String,
}

impl CedarSchema {
    /// Parses a `.cedarschema` text definition (`SEC-020`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if declarations are syntactically invalid or omit
    /// required GAP entity types (`Principal`, `Element`, `Operation`) or actions.
    pub fn parse(raw: &str) -> GapResult<Self> {
        let stripped = strip_line_comments(raw);
        let statements = split_top_level_statements(&stripped)?;
        if statements.is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "Cedar schema cannot be empty",
            ));
        }

        let mut schema = Self {
            entities: BTreeMap::new(),
            entity_parents: BTreeMap::new(),
            actions: BTreeSet::new(),
            principal_attrs: BTreeMap::new(),
            resource_attrs: BTreeMap::new(),
            context_attrs: BTreeMap::new(),
            canonical_text: stripped,
        };

        for stmt in statements {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("entity") {
                schema.parse_entity_stmt(rest)?;
            } else if let Some(rest) = trimmed.strip_prefix("action") {
                schema.parse_action_stmt(rest)?;
            } else {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("unexpected Cedar schema declaration: `{trimmed}`"),
                ));
            }
        }

        if !schema.entities.contains_key("Principal") || !schema.entities.contains_key("Group") {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "Cedar schema must declare `Principal` and `Group` entities",
            ));
        }
        if schema.actions.is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "Cedar schema must declare at least one action",
            ));
        }
        if let Some(p_attrs) = schema.entities.get("Principal") {
            schema.principal_attrs.clone_from(p_attrs);
        }
        for (entity_name, attrs) in &schema.entities {
            if entity_name != "Principal" && entity_name != "Group" {
                for (k, v) in attrs {
                    schema.resource_attrs.insert(k.clone(), *v);
                }
            }
        }

        Ok(schema)
    }

    fn parse_entity_stmt(&mut self, rest: &str) -> GapResult<()> {
        let (header, body) = if let Some(brace_idx) = rest.find('{') {
            let close_idx = rest.rfind('}').ok_or_else(|| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "unclosed `{` in entity declaration",
                )
            })?;
            (&rest[..brace_idx], Some(&rest[brace_idx + 1..close_idx]))
        } else {
            (rest, None)
        };

        let (names_part, parents_part) = if let Some((left, right)) = header.split_once(" in ") {
            (left.trim(), Some(right.trim()))
        } else {
            (header.trim(), None)
        };

        let mut parent_set = BTreeSet::new();
        if let Some(parents_raw) = parents_part {
            let inner = parents_raw
                .trim()
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .ok_or_else(|| {
                    GapError::new(
                        ErrorCode::ValidationError,
                        "entity `in [...]` clause must be bracketed",
                    )
                })?;
            for p in inner.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                parent_set.insert(p.to_owned());
            }
        }

        let attrs = match body {
            Some(fields_str) => parse_attr_block(fields_str)?,
            None => BTreeMap::new(),
        };

        for raw_name in names_part
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !is_ident(raw_name) {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("invalid entity identifier `{raw_name}` in schema"),
                ));
            }
            self.entities.insert(raw_name.to_owned(), attrs.clone());
            self.entity_parents
                .insert(raw_name.to_owned(), parent_set.clone());
        }
        Ok(())
    }

    fn parse_action_stmt(&mut self, rest: &str) -> GapResult<()> {
        let (names_part, applies_part) = rest.split_once("appliesTo").ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                "Cedar schema action must include `appliesTo { ... }`",
            )
        })?;
        for raw_name in names_part
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            AuthzAction::parse(raw_name)?;
            self.actions.insert(raw_name.to_owned());
        }
        let brace_open = applies_part.find('{').ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                "missing `{` in `appliesTo` block",
            )
        })?;
        let brace_close = applies_part.rfind('}').ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                "unclosed `{` in `appliesTo` block",
            )
        })?;
        let body = &applies_part[brace_open + 1..brace_close];
        if let Some(ctx_pos) = body.find("context") {
            let after_ctx = &body[ctx_pos + "context".len()..];
            if let (Some(c_open), Some(c_close)) = (after_ctx.find('{'), after_ctx.rfind('}')) {
                let ctx_body = &after_ctx[c_open + 1..c_close];
                let parsed_ctx = parse_attr_block(ctx_body)?;
                for (k, v) in parsed_ctx {
                    self.context_attrs.insert(k, v);
                }
            }
        }
        Ok(())
    }

    /// Returns `true` if `entity_type` exists in this schema.
    #[must_use]
    pub fn has_entity(&self, entity_type: &str) -> bool {
        self.entities.contains_key(entity_type)
    }

    /// Returns `true` if `action_name` exists in this schema.
    #[must_use]
    pub fn has_action(&self, action_name: &str) -> bool {
        self.actions.contains(action_name)
    }
}

fn parse_attr_block(block: &str) -> GapResult<BTreeMap<String, SchemaAttrType>> {
    let mut map = BTreeMap::new();
    for entry in block.split(',') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (name_raw, type_raw) = trimmed.split_once(':').ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                format!("invalid schema attribute declaration `{trimmed}`"),
            )
        })?;
        let attr_name = name_raw.trim().trim_end_matches('?').trim();
        if !is_ident(attr_name) {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!("invalid schema attribute name `{attr_name}`"),
            ));
        }
        let attr_ty = SchemaAttrType::parse(type_raw)?;
        map.insert(attr_name.to_owned(), attr_ty);
    }
    Ok(map)
}

fn strip_line_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for line in input.lines() {
        let mut in_str = false;
        let mut escape = false;
        let mut cut = line.len();
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if escape {
                escape = false;
                i += 1;
                continue;
            }
            if b == b'\\' && in_str {
                escape = true;
            } else if b == b'"' {
                in_str = !in_str;
            } else if !in_str && b == b'/' && bytes.get(i + 1) == Some(&b'/') {
                cut = i;
                break;
            }
            i += 1;
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

fn split_top_level_statements(input: &str) -> GapResult<Vec<String>> {
    let mut out = Vec::new();
    let mut depth: usize = 0;
    let mut start = 0;
    let bytes = input.as_bytes();
    for (idx, &b) in bytes.iter().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => {
                if depth == 0 {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        "unmatched `}` in Cedar schema",
                    ));
                }
                depth -= 1;
            }
            b';' if depth == 0 => {
                out.push(input[start..idx].to_owned());
                start = idx + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "unclosed `{` in Cedar schema",
        ));
    }
    let tail = input[start..].trim();
    if !tail.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("missing trailing `;` in Cedar schema near `{tail}`"),
        ));
    }
    Ok(out)
}

fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

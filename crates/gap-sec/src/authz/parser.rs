//! Pure-Rust Cedar policy lexer, parser, and schema-checked AST (`SEC-020`..`SEC-022`).

use std::fmt;

use gap_core::error::{ErrorCode, GapError, GapResult};

use super::schema::{AuthzAction, CedarSchema, SchemaAttrType};

/// Policy statement effect (`permit` or `forbid`; `SEC-020`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effect {
    /// Grants access when matched unless overridden by an explicit `forbid`.
    Permit,
    /// Explicitly denies access; always overrides every matching `permit`.
    Forbid,
}

/// Entity reference (`Type::"id"`, e.g. `Group::"agents-readonly"` or `Action::"Mutate"`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntityUid {
    /// Entity type name (`Group`, `Action`, `Principal`, etc.).
    pub entity_type: String,
    /// Entity identifier string.
    pub id: String,
}

/// Principal scope constraint in a Cedar policy head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrincipalConstraint {
    /// Unconstrained (`principal`).
    Any,
    /// Exact match (`principal == Type::"id"`).
    Eq(EntityUid),
    /// Group / hierarchy membership (`principal in Group::"id"`).
    In(EntityUid),
}

/// Action scope constraint in a Cedar policy head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionConstraint {
    /// Unconstrained (`action`).
    Any,
    /// Exact match (`action == Action::"Name"`).
    Eq(AuthzAction),
    /// Set membership (`action in [Action::"A", Action::"B"]`).
    In(Vec<AuthzAction>),
}

/// Resource scope constraint in a Cedar policy head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceConstraint {
    /// Unconstrained (`resource`).
    Any,
    /// Exact match (`resource == Type::"id"`).
    Eq(EntityUid),
    /// Ancestor/branch membership (`resource in Type::"id"`).
    In(EntityUid),
    /// Entity type restriction (`resource is Type`).
    IsType(String),
}

/// Optional condition clause kind (`when { ... }` or `unless { ... }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClauseKind {
    /// Matches when expression evaluates to `true`.
    When,
    /// Matches when expression evaluates to `false`.
    Unless,
}

/// Literal scalar/set value inside a Cedar expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    /// Boolean literal (`true` / `false`).
    Bool(bool),
    /// 64-bit signed integer literal.
    Long(i64),
    /// UTF-8 string literal.
    String(String),
    /// Entity UID (`Type::"id"`).
    Entity(EntityUid),
}

/// Root object of an attribute path (`principal`, `resource`, or `context`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrRoot {
    /// Immediate or delegated principal entity.
    Principal,
    /// Target resource entity.
    Resource,
    /// Request evaluation context.
    Context,
}

/// Comparison operators supported in Cedar policy conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// Cedar boolean/value condition AST node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// Literal value.
    Literal(Literal),
    /// Explicit set literal (`[...]`).
    Set(Vec<Expr>),
    /// Attribute access (`principal.tenant`, `resource.sideEffect`, `context.riskScore`).
    Attr {
        /// Root object.
        root: AttrRoot,
        /// Attribute name.
        attr: String,
    },
    /// Logical negation (`!expr`).
    Not(Box<Expr>),
    /// Logical conjunction (`lhs && rhs`).
    And(Box<Expr>, Box<Expr>),
    /// Logical disjunction (`lhs || rhs`).
    Or(Box<Expr>, Box<Expr>),
    /// Binary comparison.
    Cmp {
        /// Comparison operator.
        op: CmpOp,
        /// Left-hand expression.
        lhs: Box<Expr>,
        /// Right-hand expression.
        rhs: Box<Expr>,
    },
    /// Membership check (`lhs in rhs`).
    In {
        /// Left-hand expression.
        lhs: Box<Expr>,
        /// Right-hand set/hierarchy expression.
        rhs: Box<Expr>,
    },
    /// Set `.contains(item)` predicate.
    Contains {
        /// Target set expression.
        target: Box<Expr>,
        /// Candidate element expression.
        item: Box<Expr>,
    },
    /// String `.startsWith(prefix)` predicate.
    StartsWith {
        /// Target string expression.
        target: Box<Expr>,
        /// Prefix string expression.
        prefix: Box<Expr>,
    },
}

/// Parsed single Cedar policy (`SEC-020`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Policy identifier (`@id("...")` annotation or synthesized deterministic ID).
    pub id: String,
    /// `permit` or `forbid`.
    pub effect: Effect,
    /// Principal scope constraint.
    pub principal: PrincipalConstraint,
    /// Action scope constraint.
    pub action: ActionConstraint,
    /// Resource scope constraint.
    pub resource: ResourceConstraint,
    /// Optional `when` / `unless` condition clauses in source order.
    pub clauses: Vec<(ClauseKind, Expr)>,
}

/// Parses and validates Cedar policies against `schema` (`SEC-020`).
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] on syntax errors, unclosed quotes/delimiters,
/// unknown actions/entities, or attributes absent from `schema`.
pub fn parse_and_validate_policies(raw: &str, schema: &CedarSchema) -> GapResult<Vec<Policy>> {
    let tokens = tokenize(raw)?;
    let mut parser = Parser { tokens, pos: 0 };
    let mut policies = Vec::new();
    let mut idx = 0_usize;
    while !parser.at_end() {
        let mut pol = parser.parse_policy(idx)?;
        validate_policy(&pol, schema)?;
        if pol.id.is_empty() {
            pol.id = format!("policy#{idx}");
        }
        policies.push(pol);
        idx += 1;
    }
    Ok(policies)
}

fn validate_policy(policy: &Policy, schema: &CedarSchema) -> GapResult<()> {
    match &policy.principal {
        PrincipalConstraint::Any => {}
        PrincipalConstraint::Eq(uid) | PrincipalConstraint::In(uid) => {
            if !schema.has_entity(&uid.entity_type) {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("unknown principal entity type `{}`", uid.entity_type),
                ));
            }
        }
    }
    match &policy.action {
        ActionConstraint::Any => {}
        ActionConstraint::Eq(act) => {
            if !schema.has_action(act.as_str()) {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("action `{}` is not declared in schema", act.as_str()),
                ));
            }
        }
        ActionConstraint::In(acts) => {
            for act in acts {
                if !schema.has_action(act.as_str()) {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        format!("action `{}` is not declared in schema", act.as_str()),
                    ));
                }
            }
        }
    }
    match &policy.resource {
        ResourceConstraint::Any => {}
        ResourceConstraint::Eq(uid) | ResourceConstraint::In(uid) => {
            if !schema.has_entity(&uid.entity_type) {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("unknown resource entity type `{}`", uid.entity_type),
                ));
            }
        }
        ResourceConstraint::IsType(ty) => {
            if !schema.has_entity(ty) {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("unknown resource type `{ty}`"),
                ));
            }
        }
    }
    for (_, expr) in &policy.clauses {
        validate_expr(expr, schema)?;
    }
    Ok(())
}

fn validate_expr(expr: &Expr, schema: &CedarSchema) -> GapResult<()> {
    match expr {
        Expr::Literal(Literal::Entity(uid)) => {
            if !schema.has_entity(&uid.entity_type) {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("unknown entity type `{}` in expression", uid.entity_type),
                ));
            }
            Ok(())
        }
        Expr::Literal(_) => Ok(()),
        Expr::Set(items) => {
            for item in items {
                validate_expr(item, schema)?;
            }
            Ok(())
        }
        Expr::Attr { root, attr } => {
            let valid = match root {
                AttrRoot::Principal => schema.principal_attrs.contains_key(attr),
                AttrRoot::Resource => schema.resource_attrs.contains_key(attr),
                AttrRoot::Context => schema.context_attrs.contains_key(attr),
            };
            if !valid {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("attribute `{root}.{attr}` is not declared in Cedar schema"),
                ));
            }
            Ok(())
        }
        Expr::Not(inner) => validate_expr(inner, schema),
        Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Cmp { lhs: a, rhs: b, .. }
        | Expr::In { lhs: a, rhs: b }
        | Expr::Contains { target: a, item: b }
        | Expr::StartsWith {
            target: a,
            prefix: b,
        } => {
            validate_expr(a, schema)?;
            validate_expr(b, schema)?;
            if let (Expr::Attr { root, attr }, Expr::Literal(lit)) = (&**a, &**b) {
                check_attr_literal_type(*root, attr, lit, schema)?;
            }
            Ok(())
        }
    }
}

fn check_attr_literal_type(
    root: AttrRoot,
    attr: &str,
    lit: &Literal,
    schema: &CedarSchema,
) -> GapResult<()> {
    let ty = match root {
        AttrRoot::Principal => schema.principal_attrs.get(attr),
        AttrRoot::Resource => schema.resource_attrs.get(attr),
        AttrRoot::Context => schema.context_attrs.get(attr),
    };
    let Some(expected) = ty else {
        return Ok(());
    };
    let ok = matches!(
        (expected, lit),
        (SchemaAttrType::String, Literal::String(_))
            | (SchemaAttrType::Long, Literal::Long(_))
            | (SchemaAttrType::Bool, Literal::Bool(_))
            | (SchemaAttrType::SetString, Literal::String(_))
    );
    if !ok {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("type mismatch comparing `{root}.{attr}` against literal"),
        ));
    }
    Ok(())
}

impl fmt::Display for AttrRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Principal => f.write_str("principal"),
            Self::Resource => f.write_str("resource"),
            Self::Context => f.write_str("context"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Ident(String),
    String(String),
    Int(i64),
    At,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Semi,
    Dot,
    ColonColon,
    EqEq,
    BangEq,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Bang,
}

fn tokenize(raw: &str) -> GapResult<Vec<Token>> {
    let mut tokens = Vec::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        match b {
            b'@' => {
                tokens.push(Token::At);
                i += 1;
            }
            b'(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            b')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            b'{' => {
                tokens.push(Token::LBrace);
                i += 1;
            }
            b'}' => {
                tokens.push(Token::RBrace);
                i += 1;
            }
            b'[' => {
                tokens.push(Token::LBracket);
                i += 1;
            }
            b']' => {
                tokens.push(Token::RBracket);
                i += 1;
            }
            b',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            b';' => {
                tokens.push(Token::Semi);
                i += 1;
            }
            b'.' => {
                tokens.push(Token::Dot);
                i += 1;
            }
            b':' if bytes.get(i + 1) == Some(&b':') => {
                tokens.push(Token::ColonColon);
                i += 2;
            }
            b'=' if bytes.get(i + 1) == Some(&b'=') => {
                tokens.push(Token::EqEq);
                i += 2;
            }
            b'!' if bytes.get(i + 1) == Some(&b'=') => {
                tokens.push(Token::BangEq);
                i += 2;
            }
            b'!' => {
                tokens.push(Token::Bang);
                i += 1;
            }
            b'<' if bytes.get(i + 1) == Some(&b'=') => {
                tokens.push(Token::Le);
                i += 2;
            }
            b'<' => {
                tokens.push(Token::Lt);
                i += 1;
            }
            b'>' if bytes.get(i + 1) == Some(&b'=') => {
                tokens.push(Token::Ge);
                i += 2;
            }
            b'>' => {
                tokens.push(Token::Gt);
                i += 1;
            }
            b'&' if bytes.get(i + 1) == Some(&b'&') => {
                tokens.push(Token::AndAnd);
                i += 2;
            }
            b'|' if bytes.get(i + 1) == Some(&b'|') => {
                tokens.push(Token::OrOr);
                i += 2;
            }
            b'"' => {
                i += 1;
                let mut s = String::new();
                let mut closed = false;
                while i < bytes.len() {
                    let ch = raw[i..].chars().next().ok_or_else(|| {
                        GapError::new(ErrorCode::ValidationError, "invalid UTF-8 in policy string")
                    })?;
                    i += ch.len_utf8();
                    if ch == '"' {
                        closed = true;
                        break;
                    }
                    if ch == '\\' {
                        let esc = raw[i..].chars().next().ok_or_else(|| {
                            GapError::new(ErrorCode::ValidationError, "unterminated string escape")
                        })?;
                        i += esc.len_utf8();
                        match esc {
                            '"' => s.push('"'),
                            '\\' => s.push('\\'),
                            'n' => s.push('\n'),
                            't' => s.push('\t'),
                            'r' => s.push('\r'),
                            other => s.push(other),
                        }
                    } else {
                        s.push(ch);
                    }
                }
                if !closed {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        "unterminated string literal in Cedar policy",
                    ));
                }
                tokens.push(Token::String(s));
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let num_str = &raw[start..i];
                let val = num_str.parse::<i64>().map_err(|_| {
                    GapError::new(
                        ErrorCode::ValidationError,
                        format!("invalid integer `{num_str}` in Cedar policy"),
                    )
                })?;
                tokens.push(Token::Int(val));
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                tokens.push(Token::Ident(raw[start..i].to_owned()));
            }
            _ => {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!(
                        "unexpected character `{}` in Cedar policy",
                        raw[i..].chars().next().unwrap_or('?')
                    ),
                ));
            }
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> GapResult<Token> {
        let tok = self.tokens.get(self.pos).cloned().ok_or_else(|| {
            GapError::new(ErrorCode::ValidationError, "unexpected end of Cedar policy")
        })?;
        self.pos += 1;
        Ok(tok)
    }

    fn expect(&mut self, expected: &Token) -> GapResult<()> {
        let tok = self.next()?;
        if &tok != expected {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!("unexpected Cedar token `{tok:?}`, expected `{expected:?}`"),
            ));
        }
        Ok(())
    }

    fn expect_ident(&mut self, expected: &str) -> GapResult<()> {
        match self.next()? {
            Token::Ident(ref s) if s == expected => Ok(()),
            other => Err(GapError::new(
                ErrorCode::ValidationError,
                format!("expected identifier `{expected}`, got `{other:?}`"),
            )),
        }
    }

    fn parse_policy(&mut self, idx: usize) -> GapResult<Policy> {
        let mut id = String::new();
        while matches!(self.peek(), Some(Token::At)) {
            self.next()?;
            let ann = match self.next()? {
                Token::Ident(name) => name,
                other => {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        format!("expected annotation name, got `{other:?}`"),
                    ));
                }
            };
            self.expect(&Token::LParen)?;
            let val = match self.next()? {
                Token::String(s) => s,
                other => {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        format!("expected annotation string value, got `{other:?}`"),
                    ));
                }
            };
            self.expect(&Token::RParen)?;
            if ann == "id" {
                id = val;
            }
        }

        let effect = match self.next()? {
            Token::Ident(ref kw) if kw == "permit" => Effect::Permit,
            Token::Ident(ref kw) if kw == "forbid" => Effect::Forbid,
            other => {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("expected `permit` or `forbid`, got `{other:?}`"),
                ));
            }
        };

        self.expect(&Token::LParen)?;
        let principal = self.parse_principal_constraint()?;
        self.expect(&Token::Comma)?;
        let action = self.parse_action_constraint()?;
        self.expect(&Token::Comma)?;
        let resource = self.parse_resource_constraint()?;
        self.expect(&Token::RParen)?;

        let mut clauses = Vec::new();
        while let Some(Token::Ident(kw)) = self.peek().cloned() {
            let kind = match kw.as_str() {
                "when" => ClauseKind::When,
                "unless" => ClauseKind::Unless,
                _ => break,
            };
            self.next()?;
            self.expect(&Token::LBrace)?;
            let expr = self.parse_expr()?;
            self.expect(&Token::RBrace)?;
            clauses.push((kind, expr));
        }

        self.expect(&Token::Semi)?;
        if id.is_empty() {
            id = format!("policy#{idx}");
        }
        Ok(Policy {
            id,
            effect,
            principal,
            action,
            resource,
            clauses,
        })
    }

    fn parse_entity_uid(&mut self) -> GapResult<EntityUid> {
        let ty = match self.next()? {
            Token::Ident(s) => s,
            other => {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("expected entity type, got `{other:?}`"),
                ));
            }
        };
        self.expect(&Token::ColonColon)?;
        let id = match self.next()? {
            Token::String(s) => s,
            other => {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("expected entity string id, got `{other:?}`"),
                ));
            }
        };
        Ok(EntityUid {
            entity_type: ty,
            id,
        })
    }

    fn parse_principal_constraint(&mut self) -> GapResult<PrincipalConstraint> {
        self.expect_ident("principal")?;
        match self.peek() {
            Some(Token::EqEq) => {
                self.next()?;
                Ok(PrincipalConstraint::Eq(self.parse_entity_uid()?))
            }
            Some(Token::Ident(kw)) if kw == "in" => {
                self.next()?;
                Ok(PrincipalConstraint::In(self.parse_entity_uid()?))
            }
            _ => Ok(PrincipalConstraint::Any),
        }
    }

    fn parse_action_constraint(&mut self) -> GapResult<ActionConstraint> {
        self.expect_ident("action")?;
        match self.peek() {
            Some(Token::EqEq) => {
                self.next()?;
                let uid = self.parse_entity_uid()?;
                if uid.entity_type != "Action" {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        "action constraint must reference `Action::\"...\"`",
                    ));
                }
                Ok(ActionConstraint::Eq(AuthzAction::parse(&uid.id)?))
            }
            Some(Token::Ident(kw)) if kw == "in" => {
                self.next()?;
                if matches!(self.peek(), Some(Token::LBracket)) {
                    self.next()?;
                    let mut actions = Vec::new();
                    while !matches!(self.peek(), Some(Token::RBracket)) {
                        let uid = self.parse_entity_uid()?;
                        if uid.entity_type != "Action" {
                            return Err(GapError::new(
                                ErrorCode::ValidationError,
                                "action set must contain `Action::\"...\"` items",
                            ));
                        }
                        actions.push(AuthzAction::parse(&uid.id)?);
                        if matches!(self.peek(), Some(Token::Comma)) {
                            self.next()?;
                        } else {
                            break;
                        }
                    }
                    self.expect(&Token::RBracket)?;
                    Ok(ActionConstraint::In(actions))
                } else {
                    let uid = self.parse_entity_uid()?;
                    if uid.entity_type != "Action" {
                        return Err(GapError::new(
                            ErrorCode::ValidationError,
                            "action constraint must reference `Action::\"...\"`",
                        ));
                    }
                    Ok(ActionConstraint::In(vec![AuthzAction::parse(&uid.id)?]))
                }
            }
            _ => Ok(ActionConstraint::Any),
        }
    }

    fn parse_resource_constraint(&mut self) -> GapResult<ResourceConstraint> {
        self.expect_ident("resource")?;
        match self.peek() {
            Some(Token::EqEq) => {
                self.next()?;
                Ok(ResourceConstraint::Eq(self.parse_entity_uid()?))
            }
            Some(Token::Ident(kw)) if kw == "in" => {
                self.next()?;
                Ok(ResourceConstraint::In(self.parse_entity_uid()?))
            }
            Some(Token::Ident(kw)) if kw == "is" => {
                self.next()?;
                match self.next()? {
                    Token::Ident(ty) => Ok(ResourceConstraint::IsType(ty)),
                    other => Err(GapError::new(
                        ErrorCode::ValidationError,
                        format!("expected entity type after `resource is`, got `{other:?}`"),
                    )),
                }
            }
            _ => Ok(ResourceConstraint::Any),
        }
    }

    fn parse_expr(&mut self) -> GapResult<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> GapResult<Expr> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek(), Some(Token::OrOr)) {
            self.next()?;
            let rhs = self.parse_and()?;
            lhs = Expr::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> GapResult<Expr> {
        let mut lhs = self.parse_cmp()?;
        while matches!(self.peek(), Some(Token::AndAnd)) {
            self.next()?;
            let rhs = self.parse_cmp()?;
            lhs = Expr::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_cmp(&mut self) -> GapResult<Expr> {
        let lhs = self.parse_unary()?;
        let op = match self.peek() {
            Some(Token::EqEq) => Some(CmpOp::Eq),
            Some(Token::BangEq) => Some(CmpOp::Ne),
            Some(Token::Lt) => Some(CmpOp::Lt),
            Some(Token::Le) => Some(CmpOp::Le),
            Some(Token::Gt) => Some(CmpOp::Gt),
            Some(Token::Ge) => Some(CmpOp::Ge),
            Some(Token::Ident(kw)) if kw == "in" => {
                self.next()?;
                let rhs = self.parse_unary()?;
                return Ok(Expr::In {
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                });
            }
            _ => None,
        };
        if let Some(cmp_op) = op {
            self.next()?;
            let rhs = self.parse_unary()?;
            Ok(Expr::Cmp {
                op: cmp_op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            })
        } else {
            Ok(lhs)
        }
    }

    fn parse_unary(&mut self) -> GapResult<Expr> {
        if matches!(self.peek(), Some(Token::Bang)) {
            self.next()?;
            Ok(Expr::Not(Box::new(self.parse_unary()?)))
        } else {
            self.parse_postfix()
        }
    }

    fn parse_postfix(&mut self) -> GapResult<Expr> {
        let mut expr = self.parse_primary()?;
        while matches!(self.peek(), Some(Token::Dot)) {
            self.next()?;
            let method = match self.next()? {
                Token::Ident(s) => s,
                other => {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        format!("expected attribute/method name after `.`, got `{other:?}`"),
                    ));
                }
            };
            if matches!(self.peek(), Some(Token::LParen)) {
                self.next()?;
                let arg = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                match method.as_str() {
                    "contains" => {
                        expr = Expr::Contains {
                            target: Box::new(expr),
                            item: Box::new(arg),
                        };
                    }
                    "startsWith" => {
                        expr = Expr::StartsWith {
                            target: Box::new(expr),
                            prefix: Box::new(arg),
                        };
                    }
                    other => {
                        return Err(GapError::new(
                            ErrorCode::ValidationError,
                            format!("unsupported Cedar method `.{other}(...)`"),
                        ));
                    }
                }
            } else {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("unexpected nested property `.{method}`"),
                ));
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> GapResult<Expr> {
        match self.next()? {
            Token::LParen => {
                let inner = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                Ok(inner)
            }
            Token::LBracket => {
                let mut items = Vec::new();
                while !matches!(self.peek(), Some(Token::RBracket)) {
                    items.push(self.parse_expr()?);
                    if matches!(self.peek(), Some(Token::Comma)) {
                        self.next()?;
                    } else {
                        break;
                    }
                }
                self.expect(&Token::RBracket)?;
                Ok(Expr::Set(items))
            }
            Token::String(s) => Ok(Expr::Literal(Literal::String(s))),
            Token::Int(n) => Ok(Expr::Literal(Literal::Long(n))),
            Token::Ident(ident) => match ident.as_str() {
                "true" => Ok(Expr::Literal(Literal::Bool(true))),
                "false" => Ok(Expr::Literal(Literal::Bool(false))),
                "principal" | "resource" | "context" if matches!(self.peek(), Some(Token::Dot)) => {
                    self.next()?;
                    let attr = match self.next()? {
                        Token::Ident(a) => a,
                        other => {
                            return Err(GapError::new(
                                ErrorCode::ValidationError,
                                format!("expected attribute after `{ident}.`, got `{other:?}`"),
                            ));
                        }
                    };
                    let root = match ident.as_str() {
                        "principal" => AttrRoot::Principal,
                        "resource" => AttrRoot::Resource,
                        _ => AttrRoot::Context,
                    };
                    Ok(Expr::Attr { root, attr })
                }
                _ if matches!(self.peek(), Some(Token::ColonColon)) => {
                    self.next()?;
                    let id = match self.next()? {
                        Token::String(s) => s,
                        other => {
                            return Err(GapError::new(
                                ErrorCode::ValidationError,
                                format!("expected entity string id after `::`, got `{other:?}`"),
                            ));
                        }
                    };
                    Ok(Expr::Literal(Literal::Entity(EntityUid {
                        entity_type: ident,
                        id,
                    })))
                }
                other => Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("unexpected identifier `{other}` in Cedar expression"),
                )),
            },
            other => Err(GapError::new(
                ErrorCode::ValidationError,
                format!("unexpected token `{other:?}` in Cedar expression"),
            )),
        }
    }
}

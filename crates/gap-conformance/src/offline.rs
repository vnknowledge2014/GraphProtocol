//! In-process offline test server behind `--offline <DIR>` (`VER-011`, `GQL-003`).
//!
//! A fixture's optional `schema` field names a branch SDL file (relative to the fixtures directory). Each
//! distinct SDL becomes one test server: a single branch composed with `gap_snapshot::compose`, one
//! read (or mutation) operation per root `Query` (or `Mutation`) field, served through the real
//! `gap_transport` router over an in-memory NGC connection. The branch connector answers every
//! operation with a completed, empty row set, so request errors, limits and cost never reach it and an
//! executed field resolves to `null` / `[]`. Fixtures without `schema` use [`DEFAULT_SCHEMA_SDL`].
//!
//! Principals named by fixtures get a generated API key (`offline-<name>`) so the runner's
//! principal-to-token mapping works without a `--principals` file.

use std::collections::HashMap;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use apollo_compiler::schema::ExtendedType;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use futures_util::stream;
use gap_core::clock::{Clock, SystemClock, Timestamp};
use gap_core::ids::{BranchId, BranchUid, TenantId};
use gap_core::secret::SecretString;
use gap_core::snapshot::{BranchStatus, Endpoint, Protocol, Snapshot, TrustTier};
use gap_exec::ConnectorRegistry;
use gap_ngc_sdk::core::{ServerConfig, ServerCore};
use gap_ngc_sdk::mock::MockConnector;
use gap_ngc_sdk::testing::{InMemoryGrpcHandle, in_memory_grpc};
use gap_plan::PlanCache;
use gap_proto::ngc::v1::{
    Capabilities as ProtoCapabilities, Complete, ConnectorDescription, Limits as ProtoLimits,
    OpKind as ProtoOpKind, OperationDescriptor as ProtoOp, QueryResponse, RowSetChunk,
    SideEffectHint, query_response,
};
use gap_snapshot::{BranchInput, SnapshotBuilder, SnapshotHandle, compose};
use gap_transport::{ApiKeyRecord, AppState, Profile, TransportConfig, TransportMetrics, router};
use http_body_util::BodyExt as _;
use thiserror::Error;
use tower::ServiceExt as _;

use crate::driver::{ConformanceDriver, DriverError, decode_http_response};
use crate::fixture::{FixtureRequest, LoadedFixture};
use crate::matcher::ObservedResponse;

/// Branch SDL used by fixtures that name no `schema`.
pub const DEFAULT_SCHEMA_SDL: &str = r#"
type Order {
    id: ID!
    sku: String!
    totalCents: Int!
}

type Query {
    shopVersion: String!
    order(id: ID!): Order
    orders(first: Int): [Order!]!
}

type Mutation {
    createOrder(sku: String!): Order!
}
"#;

/// Fixed build timestamp so the offline snapshot is reproducible.
const BUILT_AT_UNIX_MS: i64 = 1_761_900_000_000;

/// Failure to build the offline test server.
#[derive(Debug, Error)]
pub enum OfflineError {
    /// A `schema` file could not be read.
    #[error("cannot read schema `{path}`: {source}")]
    SchemaRead {
        /// Path that was read.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The SDL could not be parsed or composed.
    #[error("schema `{name}` is not usable: {message}")]
    SchemaInvalid {
        /// Schema file name (or `<default>`).
        name: String,
        /// Human-readable reason.
        message: String,
    },
}

/// One in-process test server over one branch SDL, driven through `tower::ServiceExt::oneshot`.
pub struct OfflineDriver {
    target: String,
    app: Router,
    tokens: BTreeMap<String, SecretString>,
    _grpc: InMemoryGrpcHandle,
}

impl std::fmt::Debug for OfflineDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineDriver")
            .field("target", &self.target)
            .field("principals", &self.tokens.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl OfflineDriver {
    /// Builds a test server for `sdl` (named `schema_name` in reports) with one API key per entry of
    /// `principals`.
    ///
    /// # Errors
    ///
    /// Returns [`OfflineError::SchemaInvalid`] if the SDL has no root `Query`/`Mutation` field or does
    /// not compose.
    pub fn new(
        schema_name: &str,
        sdl: &str,
        principals: &BTreeSet<String>,
    ) -> Result<Self, OfflineError> {
        let invalid = |message: String| OfflineError::SchemaInvalid {
            name: schema_name.to_owned(),
            message,
        };
        let (queries, mutations) = root_fields(sdl).map_err(&invalid)?;
        if queries.is_empty() && mutations.is_empty() {
            return Err(invalid("no root Query or Mutation fields".to_owned()));
        }

        let branch_slug = branch_slug(schema_name);
        let uid = BranchUid::parse("018bcfe5-6c01-7010-8101-010101010101")
            .map_err(|e| invalid(e.to_string()))?;
        let branch_id = BranchId::new(branch_slug.as_str()).map_err(|e| invalid(e.to_string()))?;
        let input = BranchInput {
            id: branch_id,
            uid: uid.clone(),
            protocol: Protocol::Grpc,
            trust: TrustTier::T0,
            endpoint: Endpoint {
                uri: "https://offline.conformance.test:8443".to_owned(),
                transport: "grpc".to_owned(),
            },
            description: ConnectorDescription {
                connector_id: branch_slug,
                connector_version: "1.0.0".to_owned(),
                ngc_version: "gap/0.1".to_owned(),
                protocol: gap_proto::ngc::v1::Protocol::Grpc as i32,
                spec_hash: "11".repeat(32),
                capabilities: Some(ProtoCapabilities {
                    foreach_batch: true,
                    idempotency: true,
                    ..ProtoCapabilities::default()
                }),
                limits: Some(ProtoLimits {
                    max_batch_size: 64,
                    max_in_flight: 64,
                    max_page_size: 200,
                    default_timeout_ms: 30_000,
                    max_response_bytes: 4_194_304,
                }),
                schema_sdl: sdl.to_owned(),
                operations: queries
                    .iter()
                    .map(|name| operation(name, ProtoOpKind::Query))
                    .chain(
                        mutations
                            .iter()
                            .map(|name| operation(name, ProtoOpKind::Mutation)),
                    )
                    .collect(),
                meta: None,
                auth: None,
            },
            status: BranchStatus::Active,
        };
        let composition = compose(&[input]).map_err(|e| invalid(format!("{e:?}")))?;
        let built_at =
            Timestamp::from_unix_millis(BUILT_AT_UNIX_MS).map_err(|e| invalid(e.to_string()))?;
        let tenant = TenantId::default_tenant();
        let base = Snapshot::empty_for_tenant(0, tenant.clone());
        let snapshot = SnapshotBuilder::new(&base, composition)
            .built_at(built_at)
            .builder("gap-conformance-offline")
            .build();

        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let core = Arc::new(ServerCore::new(
            empty_connector(),
            ServerConfig::default()
                .with_clock(Arc::clone(&clock))
                .with_max_in_flight(256),
        ));
        let (client, grpc) = in_memory_grpc(core);
        let registry = Arc::new(
            ConnectorRegistry::builder()
                .register(uid, Arc::new(client), 64)
                .build(),
        );

        let mut tokens = BTreeMap::new();
        let mut config = TransportConfig::for_profile(Profile::Dev);
        for name in principals {
            let raw = format!("offline-{name}");
            config.api_keys.push(ApiKeyRecord::from_raw_key(
                name.clone(),
                &raw,
                tenant.clone(),
                BTreeSet::from(["read".to_owned()]),
            ));
            tokens.insert(name.clone(), SecretString::new(raw));
        }
        let state = AppState::try_new(
            Arc::new(SnapshotHandle::new(snapshot)),
            registry,
            Arc::new(PlanCache::default()),
            config,
            Arc::new(TransportMetrics::new()),
        )
        .map_err(|e| invalid(e.to_string()))?;

        Ok(Self {
            target: format!("offline:{schema_name}"),
            app: router(Arc::new(state)),
            tokens,
            _grpc: grpc,
        })
    }

    /// Bearer tokens generated for the fixture principals (`name -> token`).
    #[must_use]
    pub fn principal_tokens(&self) -> &BTreeMap<String, SecretString> {
        &self.tokens
    }
}

impl ConformanceDriver for OfflineDriver {
    fn target(&self) -> &str {
        &self.target
    }

    fn execute<'a>(
        &'a self,
        request: &'a FixtureRequest,
        bearer_token: Option<&'a SecretString>,
    ) -> Pin<Box<dyn Future<Output = Result<ObservedResponse, DriverError>> + Send + 'a>> {
        Box::pin(async move {
            let transport_error = |reason: String| DriverError::Transport {
                target: self.target.clone(),
                reason,
            };
            let body = serde_json::to_vec(request)
                .map_err(|e| transport_error(format!("cannot encode request: {e}")))?;
            let mut builder = Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/graphql-response+json");
            if let Some(token) = bearer_token {
                builder = builder.header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", token.expose_secret()),
                );
            }
            let http_request = builder
                .body(Body::from(body))
                .map_err(|e| transport_error(format!("cannot build request: {e}")))?;
            let response = self
                .app
                .clone()
                .oneshot(http_request)
                .await
                .map_err(|e| match e {})?;
            let status = response.status().as_u16();
            let headers: Vec<(String, String)> = response
                .headers()
                .iter()
                .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
                .collect();
            let bytes = response
                .into_body()
                .collect()
                .await
                .map_err(|e| transport_error(format!("cannot read response body: {e}")))?
                .to_bytes();
            decode_http_response(&self.target, status, &headers, &bytes)
        })
    }
}

/// Groups `fixtures` by their `schema` path (`None` = the default SDL) and reads each schema file once.
///
/// # Errors
///
/// Returns [`OfflineError::SchemaRead`] if a named schema file cannot be read.
pub fn group_by_schema<'f>(
    fixtures_dir: &Path,
    fixtures: &'f [LoadedFixture],
) -> Result<BTreeMap<Option<String>, SchemaGroup<'f>>, OfflineError> {
    let mut groups: BTreeMap<Option<String>, SchemaGroup<'f>> = BTreeMap::new();
    for fixture in fixtures {
        if !groups.contains_key(&fixture.schema) {
            let sdl = match &fixture.schema {
                None => DEFAULT_SCHEMA_SDL.to_owned(),
                Some(rel) => {
                    let path = fixtures_dir.join(rel);
                    std::fs::read_to_string(&path).map_err(|source| OfflineError::SchemaRead {
                        path: path.display().to_string(),
                        source,
                    })?
                }
            };
            groups.insert(
                fixture.schema.clone(),
                SchemaGroup {
                    name: fixture
                        .schema
                        .clone()
                        .unwrap_or_else(|| "<default>".to_owned()),
                    sdl,
                    fixtures: Vec::new(),
                },
            );
        }
        if let Some(group) = groups.get_mut(&fixture.schema) {
            group.fixtures.push(fixture);
        }
    }
    Ok(groups)
}

/// Fixtures sharing one branch SDL.
#[derive(Debug)]
pub struct SchemaGroup<'f> {
    /// Schema path as written in the fixtures (`<default>` when absent).
    pub name: String,
    /// Branch SDL text.
    pub sdl: String,
    /// Fixtures to run against the server built from `sdl`, in load order.
    pub fixtures: Vec<&'f LoadedFixture>,
}

fn root_fields(sdl: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let schema =
        apollo_compiler::Schema::parse(sdl, "branch.graphql").map_err(|e| e.errors.to_string())?;
    let fields = |type_name: &str| -> Vec<String> {
        match schema.types.get(type_name) {
            Some(ExtendedType::Object(object)) => object
                .fields
                .keys()
                .map(|n| n.as_str().to_owned())
                .collect(),
            _ => Vec::new(),
        }
    };
    Ok((fields("Query"), fields("Mutation")))
}

fn operation(name: &str, kind: ProtoOpKind) -> ProtoOp {
    ProtoOp {
        name: name.to_owned(),
        kind: kind as i32,
        description: format!("offline {name}"),
        input_schema: None,
        output_schema: None,
        side_effect_hint: if kind == ProtoOpKind::Query {
            SideEffectHint::SeNone as i32
        } else {
            SideEffectHint::SeReversible as i32
        },
        idempotent_hint: kind == ProtoOpKind::Query,
        supports_dry_run: false,
        compensation_operation: String::new(),
        examples: Vec::new(),
        cost_weight: 1,
        pagination: 0,
        key_fields: Vec::new(),
        annotations: HashMap::new(),
    }
}

fn empty_connector() -> MockConnector {
    MockConnector::new().on_query(|_req, _scope| {
        let rows = QueryResponse {
            chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                foreach_index: 0,
                rows: Vec::new(),
                last_for_index: true,
                error: None,
            })),
        };
        let complete = QueryResponse {
            chunk: Some(query_response::Chunk::Complete(Complete {
                upstream_calls: 1,
                ..Complete::default()
            })),
        };
        Ok(Box::pin(stream::iter(vec![Ok(rows), Ok(complete)])))
    })
}

/// Derives a valid `BranchId` slug (`[a-z][a-z0-9_-]{1,62}`) from a schema file name.
fn branch_slug(schema_name: &str) -> String {
    let stem = Path::new(schema_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("shop");
    let mut slug: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    if !slug.starts_with(|c: char| c.is_ascii_lowercase()) {
        slug.insert(0, 'b');
    }
    while slug.len() < 2 {
        slug.push('x');
    }
    slug.truncate(63);
    slug
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(query: &str) -> FixtureRequest {
        FixtureRequest {
            query: query.to_owned(),
            variables: None,
            operation_name: None,
            extensions: None,
        }
    }

    // spec: GQL-003, SEC-010
    #[tokio::test]
    async fn generated_principal_keys_authenticate_and_unknown_keys_are_refused() {
        let principals = BTreeSet::from(["agent-readonly".to_owned()]);
        let driver = OfflineDriver::new("shop.graphql", DEFAULT_SCHEMA_SDL, &principals).unwrap();
        let token = driver.principal_tokens().get("agent-readonly").unwrap();
        let ok = driver
            .execute(&request("{ shopVersion }"), Some(token))
            .await
            .unwrap();
        assert_eq!(ok.status, 200, "{ok:?}");
        let bogus = SecretString::new("offline-not-a-key".to_owned());
        let refused = driver
            .execute(&request("{ shopVersion }"), Some(&bogus))
            .await
            .unwrap();
        assert_eq!(refused.status, 401, "{refused:?}");
        let anonymous = driver
            .execute(&request("{ shopVersion }"), None)
            .await
            .unwrap();
        assert_eq!(
            anonymous.status, 200,
            "dev profile allows anonymous: {anonymous:?}"
        );
    }

    // spec: GQL-003
    #[test]
    fn a_schema_without_root_fields_or_with_broken_sdl_is_refused() {
        let none = BTreeSet::new();
        for (name, sdl) in [
            ("empty.graphql", "type Foo { id: ID }"),
            ("broken.graphql", "type Query {"),
        ] {
            let err = OfflineDriver::new(name, sdl, &none).unwrap_err();
            assert!(
                matches!(err, OfflineError::SchemaInvalid { .. }),
                "{name}: {err}"
            );
        }
    }

    // spec: GQL-003
    #[tokio::test]
    async fn each_schema_gets_its_own_server() {
        let none = BTreeSet::new();
        let a = OfflineDriver::new("a.graphql", "type Query { alpha: String }", &none).unwrap();
        let b = OfflineDriver::new("b.graphql", "type Query { beta: String }", &none).unwrap();
        for (driver, own, other) in [(&a, "alpha", "beta"), (&b, "beta", "alpha")] {
            let ok = driver
                .execute(&request(&format!("{{ {own} }}")), None)
                .await
                .unwrap();
            assert_eq!(ok.status, 200, "{own}: {ok:?}");
            let unknown = driver
                .execute(&request(&format!("{{ {other} }}")), None)
                .await
                .unwrap();
            assert_eq!(unknown.status, 422, "{other}: {unknown:?}");
        }
        assert_ne!(a.target(), b.target());
    }

    // spec: GQL-003
    #[test]
    fn group_by_schema_reads_each_schema_once_and_reports_unreadable_files() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.graphql"), "type Query { alpha: String }").unwrap();
        let mk = |name: &str, schema: Option<&str>| {
            let doc = serde_json::json!({
                "name": name, "class": ["Core"], "spec": ["GQL-003"],
                "request": {"query": "{ alpha }"}, "expect": {"status": 200}
            });
            let mut doc = doc;
            if let Some(schema) = schema {
                doc["schema"] = serde_json::json!(schema);
            }
            std::fs::write(dir.path().join(format!("{name}.json")), doc.to_string()).unwrap();
        };
        mk("01_with", Some("a.graphql"));
        mk("02_with_again", Some("a.graphql"));
        mk("03_default", None);
        let fixtures = crate::fixture::load_fixtures_dir(dir.path()).unwrap();
        let groups = group_by_schema(dir.path(), &fixtures).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[&Some("a.graphql".to_owned())].fixtures.len(), 2);
        assert_eq!(groups[&None].fixtures.len(), 1);
        assert_eq!(groups[&None].sdl, DEFAULT_SCHEMA_SDL);

        mk("04_missing", Some("missing.graphql"));
        let fixtures = crate::fixture::load_fixtures_dir(dir.path()).unwrap();
        let err = group_by_schema(dir.path(), &fixtures).unwrap_err();
        assert!(matches!(err, OfflineError::SchemaRead { .. }), "{err}");
    }

    // spec: GQL-003
    #[test]
    fn branch_slug_is_always_a_valid_branch_id() {
        for name in [
            "billing.graphql",
            "9lives.graphql",
            "A.graphql",
            "x",
            "",
            "a b/c.d.graphql",
        ] {
            let slug = branch_slug(name);
            assert!(BranchId::new(slug.as_str()).is_ok(), "{name:?} -> {slug:?}");
        }
    }
}

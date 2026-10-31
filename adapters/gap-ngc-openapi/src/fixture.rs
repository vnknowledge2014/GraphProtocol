//! Deterministic in-memory REST fixture service (`FixtureService`) and `axum` router (`NGC-091`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::http::header::{CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::routing::any;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::upstream::{Upstream, UpstreamError, UpstreamRequest, UpstreamResponse};

/// Number of deterministic customer rows served by [`FixtureService`].
pub const FIXTURE_CUSTOMER_COUNT: usize = 12;

/// Number of deterministic order rows served by [`FixtureService`].
pub const FIXTURE_ORDER_COUNT: usize = 8;

/// Number of ~1 KiB blob rows served by `GET /blobs` (`NGC-027`).
pub const FIXTURE_BLOB_COUNT: usize = 40;

/// Exact OpenAPI path templates implemented by [`FixtureService`] (`shop.openapi.json`).
const SUPPORTED_PATHS: &[&str] = &[
    "/blobs",
    "/customers",
    "/customers/{id}",
    "/orders",
    "/orders/{id}",
    "/products/{sku}",
    "/slow",
];

/// Recorded outbound request captured by [`FixtureService`] for black-box assertions (`CMP-030`, `INV-15`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    /// Exact relative `path_and_query` received by the fixture.
    pub path_and_query: String,
    /// Lowercase header name and raw UTF-8 header value pairs received by the fixture.
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CustomerRow {
    id: i64,
    name: String,
    region: String,
    credit_cents: i64,
    active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderRow {
    id: i64,
    customer_id: i64,
    total_cents: i64,
    status: String,
    metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProductRow {
    sku: String,
    title: String,
    price_cents: i64,
    tags: Vec<String>,
}

#[derive(Debug, Clone)]
enum ScriptedStep {
    Response(UpstreamResponse),
    Error(UpstreamError),
    Status {
        status: StatusCode,
        retry_after: Option<String>,
    },
}

#[derive(Debug)]
struct FixtureState {
    rate_limit: Option<String>,
    rate_remaining: Option<String>,
    handler_delay: Duration,
    scripted: VecDeque<ScriptedStep>,
    recorded: Vec<RecordedRequest>,
}

impl Default for FixtureState {
    fn default() -> Self {
        Self {
            rate_limit: Some("100".to_owned()),
            rate_remaining: Some("80".to_owned()),
            handler_delay: Duration::ZERO,
            scripted: VecDeque::new(),
            recorded: Vec::new(),
        }
    }
}

struct InFlightGuard<'a> {
    active: &'a AtomicUsize,
}

impl<'a> InFlightGuard<'a> {
    fn enter(active: &'a AtomicUsize, max_seen: &AtomicUsize) -> Self {
        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = max_seen.fetch_max(current, Ordering::SeqCst);
        Self { active }
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn seed_customers() -> Vec<CustomerRow> {
    // Non-monotonic `credit_cents` and `name` so `sort` ascending/descending meaningfully re-orders rows.
    let specs: [(i64, &str, &str, i64, bool); FIXTURE_CUSTOMER_COUNT] = [
        (1, "Zoe Laurent", "eu", 4200, true),
        (2, "Arthur Dent", "eu", 1500, true),
        (3, "Maya Lin", "us", 8900, true),
        (4, "Bramker Noor", "eu", 300, false),
        (5, "Clara Oswald", "apac", 6100, true),
        (6, "Devon Reyes", "eu", 2700, true),
        (7, "Elena Rostova", "us", 9400, true),
        (8, "Farid Alvi", "eu", 800, false),
        (9, "Greta Holm", "apac", 5300, true),
        (10, "Hugo Silva", "eu", 1900, true),
        (11, "Iris Chen", "us", 7600, true),
        (12, "Jonas Berg", "apac", 1100, true),
    ];
    specs
        .into_iter()
        .map(|(id, name, region, credit_cents, active)| CustomerRow {
            id,
            name: name.to_owned(),
            region: region.to_owned(),
            credit_cents,
            active,
        })
        .collect()
}

fn seed_orders() -> Vec<OrderRow> {
    let statuses = ["pending", "paid", "shipped", "paid", "pending", "shipped", "paid", "pending"];
    (1..=FIXTURE_ORDER_COUNT)
        .map(|idx| {
            let id = i64::try_from(idx).unwrap_or(0);
            OrderRow {
                id,
                customer_id: ((id - 1) % 6) + 1,
                total_cents: id * 1250,
                status: statuses[idx - 1].to_owned(),
                metadata: json!({ "source": "fixture", "seq": id }),
            }
        })
        .collect()
}

fn seed_blobs() -> Vec<Value> {
    (1..=FIXTURE_BLOB_COUNT)
        .map(|idx| {
            json!({
                "id": idx,
                "payload": format!("blob-{idx:02}-{}", "x".repeat(1016))
            })
        })
        .collect()
}

fn parse_query_pairs(query: Option<&str>) -> Vec<(String, String)> {
    let Some(q) = query.filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    q.split('&')
        .map(|pair| {
            let (raw_k, raw_v) = pair.split_once('=').unwrap_or((pair, ""));
            let k = percent_decode_str(raw_k).decode_utf8_lossy().into_owned();
            let v = percent_decode_str(raw_v).decode_utf8_lossy().into_owned();
            (k, v)
        })
        .collect()
}

/// Deterministic in-memory REST service matching `tests/fixtures/openapi/shop.openapi.json` (`NGC-091`).
#[derive(Debug)]
pub struct FixtureService {
    customers: Vec<CustomerRow>,
    orders: Vec<OrderRow>,
    blobs: Vec<Value>,
    requests_total: AtomicU32,
    active: AtomicUsize,
    max_concurrent: AtomicUsize,
    state: Mutex<FixtureState>,
}

impl Default for FixtureService {
    fn default() -> Self {
        Self::new()
    }
}

impl FixtureService {
    /// Constructs a fresh [`FixtureService`] with default rate-limit headers (`100` limit, `80` remaining).
    #[must_use]
    pub fn new() -> Self {
        Self {
            customers: seed_customers(),
            orders: seed_orders(),
            blobs: seed_blobs(),
            requests_total: AtomicU32::new(0),
            active: AtomicUsize::new(0),
            max_concurrent: AtomicUsize::new(0),
            state: Mutex::new(FixtureState::default()),
        }
    }

    /// Canonical OpenAPI path templates served by [`FixtureService`].
    #[must_use]
    pub fn supported_paths() -> &'static [&'static str] {
        SUPPORTED_PATHS
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, FixtureState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Configures `X-RateLimit-Limit` and `X-RateLimit-Remaining` headers attached to responses (`NGC-072`).
    pub fn set_rate_limit_headers(&self, limit: Option<&str>, remaining: Option<&str>) {
        let mut st = self.lock_state();
        st.rate_limit = limit.map(str::to_owned);
        st.rate_remaining = remaining.map(str::to_owned);
    }

    /// Configures an artificial `tokio::time::sleep` delay on normal request handling (for concurrency tests).
    pub fn set_handler_delay(&self, delay: Duration) {
        self.lock_state().handler_delay = delay;
    }

    /// Instructs the service to answer `status` (e.g. `429` or `503`) with optional `Retry-After`
    /// for the next `count` requests (`CMP-033`).
    pub fn fail_next_with_status(
        &self,
        status: StatusCode,
        retry_after: Option<&str>,
        count: usize,
    ) {
        let mut st = self.lock_state();
        for _ in 0..count {
            st.scripted.push_back(ScriptedStep::Status {
                status,
                retry_after: retry_after.map(str::to_owned),
            });
        }
    }

    /// Queues a single custom [`UpstreamResponse`] to be returned on the next call.
    pub fn push_scripted_response(&self, response: UpstreamResponse) {
        self.lock_state()
            .scripted
            .push_back(ScriptedStep::Response(response));
    }

    /// Queues a single [`UpstreamError`] (e.g. [`UpstreamError::Connect`]) for the next call.
    pub fn push_scripted_error(&self, error: UpstreamError) {
        self.lock_state()
            .scripted
            .push_back(ScriptedStep::Error(error));
    }

    /// Total number of requests received (including fault-injected attempts).
    #[must_use]
    pub fn request_count(&self) -> u32 {
        self.requests_total.load(Ordering::SeqCst)
    }

    /// Snapshot of all recorded requests in arrival order.
    #[must_use]
    pub fn recorded_requests(&self) -> Vec<RecordedRequest> {
        self.lock_state().recorded.clone()
    }

    /// Clears recorded requests and resets the request counter.
    pub fn clear_recorded(&self) {
        self.requests_total.store(0, Ordering::SeqCst);
        self.lock_state().recorded.clear();
    }

    /// Number of requests currently executing inside [`FixtureService::handle`].
    #[must_use]
    pub fn active_in_flight(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    /// High-water mark of concurrent in-flight requests observed since creation.
    #[must_use]
    pub fn max_concurrent_seen(&self) -> usize {
        self.max_concurrent.load(Ordering::SeqCst)
    }

    fn base_headers(rate_limit: Option<&str>, rate_remaining: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(lim) = rate_limit
            && let Ok(hv) = HeaderValue::from_str(lim)
        {
            headers.insert(HeaderName::from_static("x-ratelimit-limit"), hv);
        }
        if let Some(rem) = rate_remaining
            && let Ok(hv) = HeaderValue::from_str(rem)
        {
            headers.insert(HeaderName::from_static("x-ratelimit-remaining"), hv);
        }
        headers
    }

    fn json_response<T: Serialize>(
        status: StatusCode,
        headers: HeaderMap,
        value: &T,
    ) -> UpstreamResponse {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        UpstreamResponse {
            status,
            headers,
            body,
        }
    }

    fn record_and_pop(&self, req: &UpstreamRequest) -> (Option<ScriptedStep>, Duration, Option<String>, Option<String>) {
        self.requests_total.fetch_add(1, Ordering::SeqCst);
        let recorded_headers = req
            .headers
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_owned(),
                    v.to_str().unwrap_or("<non-utf8>").to_owned(),
                )
            })
            .collect();
        let mut st = self.lock_state();
        st.recorded.push(RecordedRequest {
            path_and_query: req.path_and_query.clone(),
            headers: recorded_headers,
        });
        let step = st.scripted.pop_front();
        let delay = st.handler_delay;
        let rl = st.rate_limit.clone();
        let rr = st.rate_remaining.clone();
        (step, delay, rl, rr)
    }

    fn route_sync(&self, path_and_query: &str, mut headers: HeaderMap) -> UpstreamResponse {
        let (path, opt_query) = match path_and_query.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (path_and_query, None),
        };
        let query_pairs = parse_query_pairs(opt_query);
        let segs: Vec<&str> = path
            .strip_prefix('/')
            .map_or_else(Vec::new, |rest| {
                if rest.is_empty() {
                    Vec::new()
                } else {
                    rest.split('/').collect()
                }
            });

        match segs.as_slice() {
            ["customers"] => {
                let mut rows = self.customers.clone();
                // Native batch filter `ids=1,2,3` (or repeated `ids`)
                let mut wanted_ids: Vec<i64> = Vec::new();
                let mut saw_ids = false;
                for (k, v) in &query_pairs {
                    if k == "ids" {
                        saw_ids = true;
                        for part in v.split(',').filter(|s| !s.is_empty()) {
                            let Ok(id) = part.parse::<i64>() else {
                                return Self::json_response(
                                    StatusCode::BAD_REQUEST,
                                    headers,
                                    &json!({ "error": "invalid integer in ids" }),
                                );
                            };
                            wanted_ids.push(id);
                        }
                    }
                }
                if saw_ids {
                    rows.retain(|c| wanted_ids.contains(&c.id));
                }
                if let Some((_, region)) = query_pairs.iter().find(|(k, _)| k == "region") {
                    rows.retain(|c| c.region == *region);
                }
                if let Some((_, sort)) = query_pairs.iter().find(|(k, _)| k == "sort") {
                    match sort.as_str() {
                        "id" => rows.sort_by_key(|c| c.id),
                        "-id" => rows.sort_by(|a, b| b.id.cmp(&a.id)),
                        "name" => rows.sort_by(|a, b| a.name.cmp(&b.name)),
                        "-name" => rows.sort_by(|a, b| b.name.cmp(&a.name)),
                        "credit_cents" => rows.sort_by_key(|c| c.credit_cents),
                        "-credit_cents" => rows.sort_by(|a, b| b.credit_cents.cmp(&a.credit_cents)),
                        _ => {
                            return Self::json_response(
                                StatusCode::BAD_REQUEST,
                                headers,
                                &json!({ "error": "unsupported sort field" }),
                            );
                        }
                    }
                }
                let offset = query_pairs
                    .iter()
                    .find(|(k, _)| k == "offset")
                    .and_then(|(_, v)| v.parse::<usize>().ok())
                    .unwrap_or(0);
                let limit = query_pairs
                    .iter()
                    .find(|(k, _)| k == "limit")
                    .and_then(|(_, v)| v.parse::<usize>().ok());
                let sliced: Vec<CustomerRow> = rows
                    .into_iter()
                    .skip(offset)
                    .take(limit.unwrap_or(usize::MAX))
                    .collect();
                Self::json_response(StatusCode::OK, headers, &sliced)
            }
            ["customers", raw_id] => {
                let decoded = percent_decode_str(raw_id).decode_utf8_lossy();
                let Ok(id) = decoded.parse::<i64>() else {
                    return Self::json_response(
                        StatusCode::BAD_REQUEST,
                        headers,
                        &json!({ "error": "customer id must be an integer" }),
                    );
                };
                match self.customers.iter().find(|c| c.id == id) {
                    Some(row) => Self::json_response(StatusCode::OK, headers, row),
                    None => Self::json_response(
                        StatusCode::NOT_FOUND,
                        headers,
                        &json!({ "error": "customer not found" }),
                    ),
                }
            }
            ["orders"] => {
                let mut rows = self.orders.clone();
                let statuses: Vec<&str> = query_pairs
                    .iter()
                    .filter(|(k, _)| k == "status")
                    .map(|(_, v)| v.as_str())
                    .collect();
                if !statuses.is_empty() {
                    rows.retain(|o| statuses.contains(&o.status.as_str()));
                }
                let offset = query_pairs
                    .iter()
                    .find(|(k, _)| k == "offset")
                    .and_then(|(_, v)| v.parse::<usize>().ok())
                    .unwrap_or(0);
                let limit = query_pairs
                    .iter()
                    .find(|(k, _)| k == "limit")
                    .and_then(|(_, v)| v.parse::<usize>().ok());
                let sliced: Vec<OrderRow> = rows
                    .into_iter()
                    .skip(offset)
                    .take(limit.unwrap_or(usize::MAX))
                    .collect();
                Self::json_response(StatusCode::OK, headers, &sliced)
            }
            ["orders", raw_id] => {
                let decoded = percent_decode_str(raw_id).decode_utf8_lossy();
                let Ok(id) = decoded.parse::<i64>() else {
                    return Self::json_response(
                        StatusCode::BAD_REQUEST,
                        headers,
                        &json!({ "error": "order id must be an integer" }),
                    );
                };
                match self.orders.iter().find(|o| o.id == id) {
                    Some(row) => Self::json_response(StatusCode::OK, headers, row),
                    None => Self::json_response(
                        StatusCode::NOT_FOUND,
                        headers,
                        &json!({ "error": "order not found" }),
                    ),
                }
            }
            ["products", raw_sku] => {
                let sku = percent_decode_str(raw_sku)
                    .decode_utf8_lossy()
                    .into_owned();
                if sku == "MISSING-SKU" {
                    return Self::json_response(
                        StatusCode::NOT_FOUND,
                        headers,
                        &json!({ "error": "product not found" }),
                    );
                }
                let product = ProductRow {
                    title: format!("Fixture Product ({} chars)", sku.chars().count()),
                    sku,
                    price_cents: 1999,
                    tags: vec!["fixture".to_owned(), "v0".to_owned()],
                };
                Self::json_response(StatusCode::OK, headers, &product)
            }
            ["blobs"] => {
                let offset = query_pairs
                    .iter()
                    .find(|(k, _)| k == "offset")
                    .and_then(|(_, v)| v.parse::<usize>().ok())
                    .unwrap_or(0);
                let limit = query_pairs
                    .iter()
                    .find(|(k, _)| k == "limit")
                    .and_then(|(_, v)| v.parse::<usize>().ok());
                let sliced: Vec<Value> = self
                    .blobs
                    .iter()
                    .skip(offset)
                    .take(limit.unwrap_or(usize::MAX))
                    .cloned()
                    .collect();
                Self::json_response(StatusCode::OK, headers, &sliced)
            }
            _ => {
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                Self::json_response(
                    StatusCode::NOT_FOUND,
                    headers,
                    &json!({ "error": "route not found" }),
                )
            }
        }
    }

    async fn dispatch(&self, req: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let _guard = InFlightGuard::enter(&self.active, &self.max_concurrent);
        let (step, delay, rl, rr) = self.record_and_pop(&req);

        if let Some(scripted) = step {
            return match scripted {
                ScriptedStep::Error(err) => Err(err),
                ScriptedStep::Response(resp) => Ok(resp),
                ScriptedStep::Status {
                    status,
                    retry_after,
                } => {
                    let mut headers = Self::base_headers(rl.as_deref(), rr.as_deref());
                    if let Some(ra) = retry_after
                        && let Ok(hv) = HeaderValue::from_str(&ra)
                    {
                        headers.insert(RETRY_AFTER, hv);
                    }
                    Ok(Self::json_response(
                        status,
                        headers,
                        &json!({ "error": "scripted status" }),
                    ))
                }
            };
        }

        let path_only = req
            .path_and_query
            .split_once('?')
            .map_or(req.path_and_query.as_str(), |(p, _)| p);
        if path_only == "/slow" {
            return std::future::pending::<Result<UpstreamResponse, UpstreamError>>().await;
        }

        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }

        let headers = Self::base_headers(rl.as_deref(), rr.as_deref());
        Ok(self.route_sync(&req.path_and_query, headers))
    }

    /// Handles a single in-memory `GET` request without sockets (`NGC-091`).
    pub async fn handle(&self, req: UpstreamRequest) -> UpstreamResponse {
        match self.dispatch(req).await {
            Ok(resp) => resp,
            Err(_) => UpstreamResponse {
                status: StatusCode::BAD_GATEWAY,
                headers: HeaderMap::new(),
                body: br#"{"error":"scripted transport error"}"#.to_vec(),
            },
        }
    }
}

impl Upstream for FixtureService {
    async fn send(&self, req: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        tokio::time::timeout(req.timeout, self.dispatch(req))
            .await
            .map_err(|_| UpstreamError::Timeout)?
    }
}

/// Wraps `service` in an [`axum::Router`] for real loopback socket tests (`NGC-091`).
pub fn router(service: Arc<FixtureService>) -> Router {
    Router::new().fallback(any(move |req: Request| {
        let svc = Arc::clone(&service);
        async move {
            if req.method() != Method::GET {
                return (
                    StatusCode::METHOD_NOT_ALLOWED,
                    HeaderMap::new(),
                    Vec::<u8>::new(),
                );
            }
            let path_and_query = req
                .uri()
                .path_and_query()
                .map_or_else(|| "/".to_owned(), |pq| pq.as_str().to_owned());
            let headers = req
                .headers()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let resp = svc
                .handle(UpstreamRequest {
                    path_and_query,
                    headers,
                    timeout: Duration::from_secs(60),
                })
                .await;
            (resp.status, resp.headers, resp.body)
        }
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use serde_json::Value;

    use super::*;
    use crate::document::OpenApiDocument;

    fn req(path_and_query: &str) -> UpstreamRequest {
        UpstreamRequest {
            path_and_query: path_and_query.to_owned(),
            headers: Vec::new(),
            timeout: Duration::from_secs(5),
        }
    }

    // spec: NGC-091, CMP-030, CMP-032, NGC-027, NGC-072
    #[tokio::test(start_paused = true)]
    async fn fixture_paths_match_shop_document_and_serve_all_resources_and_controls() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/openapi/shop.openapi.json");
        let bytes = std::fs::read(p).expect("read shop");
        let doc = OpenApiDocument::parse(&bytes).expect("parse shop");
        let doc_paths: BTreeSet<String> = doc
            .root()
            .get("paths")
            .and_then(Value::as_object)
            .expect("paths")
            .keys()
            .cloned()
            .collect();
        let svc_paths: BTreeSet<String> = FixtureService::supported_paths()
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(
            svc_paths, doc_paths,
            "FixtureService paths must stay in exact 1-to-1 sync with shop.openapi.json"
        );

        let svc = FixtureService::new();

        // Customers list (12 rows), native batch `ids=3,1`, region filter, and sort
        let all_resp = svc.handle(req("/customers")).await;
        assert_eq!(all_resp.status, StatusCode::OK);
        assert_eq!(
            all_resp
                .headers
                .get("x-ratelimit-limit")
                .and_then(|v| v.to_str().ok()),
            Some("100")
        );
        let all_rows: Vec<Value> = serde_json::from_slice(&all_resp.body).expect("json");
        assert_eq!(all_rows.len(), FIXTURE_CUSTOMER_COUNT);

        let batch_resp = svc.handle(req("/customers?ids=3,1")).await;
        let batch_rows: Vec<Value> = serde_json::from_slice(&batch_resp.body).expect("json");
        assert_eq!(batch_rows.len(), 2);

        let miss_resp = svc.handle(req("/customers/999")).await;
        assert_eq!(miss_resp.status, StatusCode::NOT_FOUND);

        // Products with percent-encoded `.`, `..`, and `/` inside single `{sku}` segment
        let dot_resp = svc.handle(req("/products/%2E%2E")).await;
        assert_eq!(dot_resp.status, StatusCode::OK);
        let dot_prod: Value = serde_json::from_slice(&dot_resp.body).expect("json");
        assert_eq!(dot_prod["sku"], "..");

        // Blobs (40 rows of ~1 KiB, paginated)
        let blobs_resp = svc.handle(req("/blobs?offset=35&limit=10")).await;
        let blobs: Vec<Value> = serde_json::from_slice(&blobs_resp.body).expect("json");
        assert_eq!(blobs.len(), 5);
        assert!(blobs[0]["payload"].as_str().expect("str").len() >= 1000);

        // Fault injection: 429 with Retry-After for 2 requests, then 200
        svc.fail_next_with_status(StatusCode::TOO_MANY_REQUESTS, Some("2"), 2);
        let r1 = svc.handle(req("/orders/1")).await;
        let r2 = svc.handle(req("/orders/1")).await;
        let r3 = svc.handle(req("/orders/1")).await;
        assert_eq!(r1.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            r1.headers.get("retry-after").and_then(|v| v.to_str().ok()),
            Some("2")
        );
        assert_eq!(r2.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(r3.status, StatusCode::OK);

        // Slow endpoint holds in-flight slot until dropped, then releases via RAII
        let svc_arc = Arc::new(svc);
        let slow_svc = Arc::clone(&svc_arc);
        let handle = tokio::spawn(async move {
            let _ = slow_svc.handle(req("/slow")).await;
        });
        tokio::task::yield_now().await;
        assert_eq!(svc_arc.active_in_flight(), 1);
        assert!(svc_arc.max_concurrent_seen() >= 1);
        handle.abort();
        let _ = handle.await;
        assert_eq!(svc_arc.active_in_flight(), 0);
    }
}

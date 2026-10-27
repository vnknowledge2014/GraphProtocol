//! Scoped `tracing-subscriber` capture layer for deterministic event and span inspection (`INV-15`).

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};

use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

/// Recorded metadata and fields of a `tracing` span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedSpan {
    /// Span name from static metadata.
    pub name: String,
    /// Span target (module path or explicit target).
    pub target: String,
    /// Span verbosity level.
    pub level: tracing::Level,
    /// Formatted span fields (populated at creation and updated on `span.record(...)`).
    pub fields: BTreeMap<String, String>,
}

/// Recorded metadata, fields, and enclosing span context of a `tracing` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedEvent {
    /// Event verbosity level.
    pub level: tracing::Level,
    /// Event target (module path or explicit target).
    pub target: String,
    /// Event `message` field (empty string when absent).
    pub message: String,
    /// All formatted event fields (including `"message"` when present).
    pub fields: BTreeMap<String, String>,
    /// Names of enclosing spans ordered from root to leaf.
    pub span_path: Vec<String>,
    /// Merged formatted fields from enclosing spans (outer-to-inner).
    pub span_fields: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
struct CaptureBuffer {
    spans: Vec<CapturedSpan>,
    events: Vec<CapturedEvent>,
}

#[derive(Debug, Clone)]
struct SpanExtensionData {
    buffer_index: usize,
    fields: BTreeMap<String, String>,
}

#[derive(Default)]
struct FieldVisitor {
    fields: BTreeMap<String, String>,
    message: Option<String>,
}

impl FieldVisitor {
    fn insert_field(&mut self, field: &Field, rendered: String) {
        if field.name() == "message" {
            self.message = Some(rendered.clone());
        }
        self.fields.insert(field.name().to_owned(), rendered);
    }
}

fn format_error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut current = err.source();
    while let Some(cause) = current {
        let piece = cause.to_string();
        if !piece.is_empty() && !out.contains(&piece) {
            out.push_str(": ");
            out.push_str(&piece);
        }
        current = cause.source();
    }
    out
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert_field(field, value.to_owned());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert_field(field, value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert_field(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert_field(field, value.to_string());
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        self.insert_field(field, value.to_string());
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.insert_field(field, value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert_field(field, value.to_string());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.insert_field(field, format_error_chain(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.insert_field(field, format!("{value:?}"));
    }
}

/// Shared, cloneable `tracing_subscriber::Layer` collecting spans and events in memory.
#[derive(Debug, Clone, Default)]
pub struct TracingCapture {
    inner: Arc<Mutex<CaptureBuffer>>,
}

/// RAII guard keeping a [`TracingCapture`] subscriber active on the current thread.
pub struct TracingCaptureGuard {
    _guard: tracing::subscriber::DefaultGuard,
    capture: TracingCapture,
}

impl fmt::Debug for TracingCaptureGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TracingCaptureGuard")
            .field("capture", &self.capture)
            .finish_non_exhaustive()
    }
}

impl TracingCaptureGuard {
    /// Borrows the underlying [`TracingCapture`].
    #[must_use]
    pub const fn capture(&self) -> &TracingCapture {
        &self.capture
    }

    /// Snapshot of events captured so far.
    #[must_use]
    pub fn events(&self) -> Vec<CapturedEvent> {
        self.capture.events()
    }

    /// Snapshot of spans captured so far.
    #[must_use]
    pub fn spans(&self) -> Vec<CapturedSpan> {
        self.capture.spans()
    }
}

impl TracingCapture {
    /// Creates an empty [`TracingCapture`] layer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a snapshot of all captured events in emission order.
    #[must_use]
    pub fn events(&self) -> Vec<CapturedEvent> {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.events.clone()
    }

    /// Returns a snapshot of all captured spans in creation order.
    #[must_use]
    pub fn spans(&self) -> Vec<CapturedSpan> {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.spans.clone()
    }

    /// Clears all captured spans and events.
    pub fn clear(&self) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.spans.clear();
        guard.events.clear();
    }

    /// Builds a standalone `tracing::Subscriber` backed by `Registry` and a clone of `self`.
    #[must_use]
    pub fn subscriber(&self) -> impl Subscriber + Send + Sync + 'static {
        Registry::default().with(self.clone())
    }

    /// Installs this capture subscriber as the current thread's default until the returned guard drops.
    #[must_use]
    pub fn enter_guard(&self) -> TracingCaptureGuard {
        let guard = tracing::subscriber::set_default(self.subscriber());
        TracingCaptureGuard {
            _guard: guard,
            capture: self.clone(),
        }
    }

    /// Runs `f` with a fresh [`TracingCapture`] installed as the current thread's default subscriber.
    pub fn with_captured<F, R>(f: F) -> (R, Self)
    where
        F: FnOnce() -> R,
    {
        let capture = Self::new();
        let out = tracing::subscriber::with_default(capture.subscriber(), f);
        (out, capture)
    }

    /// Runs async future `fut` instrumented with a fresh [`TracingCapture`] subscriber across `.await` points.
    pub async fn with_captured_async<Fut, R>(fut: Fut) -> (R, Self)
    where
        Fut: Future<Output = R>,
    {
        use tracing::instrument::WithSubscriber;
        let capture = Self::new();
        let out = fut.with_subscriber(capture.subscriber()).await;
        (out, capture)
    }
}

impl<S> Layer<S> for TracingCapture
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor::default();
        attrs.record(&mut visitor);

        let buffer_index = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let idx = guard.spans.len();
            guard.spans.push(CapturedSpan {
                name: attrs.metadata().name().to_owned(),
                target: attrs.metadata().target().to_owned(),
                level: *attrs.metadata().level(),
                fields: visitor.fields.clone(),
            });
            idx
        };

        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanExtensionData {
                buffer_index,
                fields: visitor.fields,
            });
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor::default();
        values.record(&mut visitor);

        let maybe_index = if let Some(span) = ctx.span(id) {
            let mut extensions = span.extensions_mut();
            if let Some(ext) = extensions.get_mut::<SpanExtensionData>() {
                ext.fields
                    .extend(visitor.fields.iter().map(|(k, v)| (k.clone(), v.clone())));
                Some(ext.buffer_index)
            } else {
                None
            }
        } else {
            None
        };

        if let Some(idx) = maybe_index {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(stored) = guard.spans.get_mut(idx) {
                stored.fields.extend(visitor.fields);
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let mut span_path = Vec::new();
        let mut span_fields = BTreeMap::new();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                span_path.push(span.name().to_owned());
                if let Some(ext) = span.extensions().get::<SpanExtensionData>() {
                    for (k, v) in &ext.fields {
                        span_fields.insert(k.clone(), v.clone());
                    }
                }
            }
        }

        let captured = CapturedEvent {
            level: *event.metadata().level(),
            target: event.metadata().target().to_owned(),
            message: visitor.message.unwrap_or_default(),
            fields: visitor.fields,
            span_path,
            span_fields,
        };

        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.events.push(captured);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec: INV-15
    #[test]
    fn captures_events_nested_spans_recorded_fields_and_respects_guard_scope() {
        tracing::info!("outside before scope");

        let (value, capture) = TracingCapture::with_captured(|| {
            let outer = tracing::info_span!(
                "outer_op",
                tenant = "acme",
                late_field = tracing::field::Empty
            );
            let _outer_enter = outer.enter();
            outer.record("late_field", "recorded_later");

            let inner = tracing::debug_span!("inner_step", attempt = 2_u64, ok = true);
            let _inner_enter = inner.enter();

            tracing::warn!(code = 429_i64, detail = "throttled", "retrying request");
            42_usize
        });

        tracing::info!("outside after scope");

        assert_eq!(value, 42);

        let spans = capture.spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].name, "outer_op");
        assert_eq!(
            spans[0].fields.get("tenant").map(String::as_str),
            Some("acme")
        );
        assert_eq!(
            spans[0].fields.get("late_field").map(String::as_str),
            Some("recorded_later")
        );
        assert_eq!(spans[1].name, "inner_step");
        assert_eq!(
            spans[1].fields.get("attempt").map(String::as_str),
            Some("2")
        );
        assert_eq!(spans[1].fields.get("ok").map(String::as_str), Some("true"));

        let events = capture.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, tracing::Level::WARN);
        assert_eq!(events[0].message, "retrying request");
        assert_eq!(
            events[0].fields.get("code").map(String::as_str),
            Some("429")
        );
        assert_eq!(
            events[0].fields.get("detail").map(String::as_str),
            Some("throttled")
        );
        assert_eq!(events[0].span_path, vec!["outer_op", "inner_step"]);
        assert_eq!(
            events[0].span_fields.get("tenant").map(String::as_str),
            Some("acme")
        );
        assert_eq!(
            events[0].span_fields.get("late_field").map(String::as_str),
            Some("recorded_later")
        );
        assert_eq!(
            events[0].span_fields.get("attempt").map(String::as_str),
            Some("2")
        );
    }

    // spec: INV-15
    #[tokio::test(start_paused = true)]
    async fn captures_events_across_async_await_points_and_guard_drop_stops_capture() {
        let capture = TracingCapture::new();
        {
            let guard = capture.enter_guard();
            tracing::info!(phase = "sync_guard", "inside guard");
            assert_eq!(guard.events().len(), 1);
        }
        tracing::info!("after guard dropped");
        assert_eq!(capture.events().len(), 1);

        let (res, async_capture) = TracingCapture::with_captured_async(async {
            let span = tracing::info_span!("async_op", req_id = "r-1");
            tracing::Instrument::instrument(
                async {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    tracing::info!(step = 2_u64, "after await");
                    "done"
                },
                span,
            )
            .await
        })
        .await;

        assert_eq!(res, "done");
        let events = async_capture.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, "after await");
        assert_eq!(events[0].span_path, vec!["async_op"]);
        assert_eq!(
            events[0].span_fields.get("req_id").map(String::as_str),
            Some("r-1")
        );
    }

    // spec: INV-15
    #[test]
    fn f6a_clear_removes_captured_spans() {
        let ((), capture) = TracingCapture::with_captured(|| {
            let span = tracing::info_span!("to_be_cleared");
            let _enter = span.enter();
            tracing::info!("event inside span");
        });
        assert_eq!(capture.spans().len(), 1);
        capture.clear();
        assert!(capture.spans().is_empty());
    }

    // spec: INV-15
    #[test]
    fn f6b_clear_removes_captured_events() {
        let ((), capture) = TracingCapture::with_captured(|| {
            let span = tracing::info_span!("to_be_cleared");
            let _enter = span.enter();
            tracing::info!("event inside span");
        });
        assert_eq!(capture.events().len(), 1);
        capture.clear();
        assert!(capture.events().is_empty());
    }

    // spec: INV-15
    #[test]
    fn f6c_records_two_level_error_source_chain_in_order() {
        #[derive(Debug)]
        struct InnerError;
        impl fmt::Display for InnerError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("inner root cause")
            }
        }
        impl std::error::Error for InnerError {}

        #[derive(Debug)]
        struct OuterError(InnerError);
        impl fmt::Display for OuterError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("outer wrapper error")
            }
        }
        impl std::error::Error for OuterError {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let err = OuterError(InnerError);
        let ((), capture) = TracingCapture::with_captured(|| {
            tracing::error!(error = &err as &dyn std::error::Error, "operation failed");
        });

        let events = capture.events();
        assert_eq!(events.len(), 1);
        let recorded = events[0]
            .fields
            .get("error")
            .expect("captured error field must exist");
        assert_eq!(recorded, "outer wrapper error: inner root cause");
    }
}

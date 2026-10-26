//! Structured `tracing` subscriber setup and optional OpenTelemetry (`otel` feature) layer
//! (`spec/08-operations.md` §§2–4, `OPS-010`, `OPS-011`, `OPS-020`, `RUN-090`, `INV-15`).

use tracing::Subscriber;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;

use crate::config::TelemetryConfig;
use crate::error::{ErrorCode, GapError, GapResult};
use crate::ids::TenantId;

/// Normative root request span name (`spec/08-operations.md` §2).
pub const SPAN_REQUEST: &str = "gap.request";
/// Normative authentication span name (`spec/08-operations.md` §2).
pub const SPAN_GATEWAY_AUTHN: &str = "gap.gateway.authn";
/// Normative GAP-QL parse/validation span name (`spec/08-operations.md` §2).
pub const SPAN_OPERATION_VALIDATE: &str = "gap.operation.validate";
/// Normative Cedar authorization span name (`spec/08-operations.md` §2).
pub const SPAN_AUTHZ: &str = "gap.authz";
/// Normative query planning span name (`spec/08-operations.md` §2).
pub const SPAN_PLAN: &str = "gap.plan";
/// Normative query execution span name (`spec/08-operations.md` §2).
pub const SPAN_EXEC: &str = "gap.exec";
/// Normative branch fetch span name (`spec/08-operations.md` §2).
pub const SPAN_EXEC_FETCH: &str = "gap.exec.fetch";
/// Normative cross-branch join span name (`spec/08-operations.md` §2).
pub const SPAN_EXEC_JOIN: &str = "gap.exec.join";
/// Normative downstream NGC connector call span name (`spec/08-operations.md` §2).
pub const SPAN_CONNECTOR_CALL: &str = "gap.connector.call";
/// Normative response assembly span name (`spec/08-operations.md` §2).
pub const SPAN_ASSEMBLE: &str = "gap.assemble";
/// Normative progressive-disclosure discovery span name (`spec/08-operations.md` §2).
pub const SPAN_DISCOVERY_RESOLVE: &str = "gap.discovery.resolve";
/// Normative snapshot publish span name (`spec/08-operations.md` §2).
pub const SPAN_SNAPSHOT_PUBLISH: &str = "gap.snapshot.publish";

/// Number of bounded tenant hash buckets for metric/log labels (`OPS-020`).
pub const TENANT_BUCKET_COUNT: u8 = 16;

/// Hashes `tenant` deterministically into `0..16` (`OPS-020`) so telemetry labels stay bounded
/// without exposing raw tenant slugs in high-cardinality sinks.
#[must_use]
pub fn tenant_bucket(tenant: &TenantId) -> u8 {
    // 64-bit FNV-1a (deterministic across runs and platforms).
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in tenant.as_str().as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    (hash % u64::from(TENANT_BUCKET_COUNT)) as u8
}

/// Lifecycle guard that flushes and shuts down OpenTelemetry span processors on drop or explicit
/// [`TelemetryGuard::shutdown`] (`RUN-090`).
#[derive(Debug, Default)]
pub struct TelemetryGuard {
    #[cfg(feature = "otel")]
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl TelemetryGuard {
    /// Returns `true` when an OpenTelemetry `SdkTracerProvider` is active on this guard.
    #[must_use]
    pub const fn has_otel_provider(&self) -> bool {
        #[cfg(feature = "otel")]
        {
            self.provider.is_some()
        }
        #[cfg(not(feature = "otel"))]
        {
            false
        }
    }

    /// Explicitly flushes and shuts down any active OpenTelemetry tracer provider (`RUN-090`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if flushing or shutting down the OpenTelemetry SDK fails.
    pub fn shutdown(&mut self) -> GapResult<()> {
        #[cfg(feature = "otel")]
        if let Some(provider) = self.provider.take() {
            let _ = provider.force_flush();
            provider.shutdown().map_err(|e| {
                GapError::new(ErrorCode::Internal, format!("otel shutdown failed: {e}"))
            })?;
        }
        Ok(())
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Parses and validates `config.log_level` as a `tracing_subscriber::EnvFilter` and verifies
/// `head_sample_ratio` (`OPS-010`).
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] if `log_level` or `head_sample_ratio` is invalid.
pub fn parse_env_filter(config: &TelemetryConfig) -> GapResult<EnvFilter> {
    if !(0.0..=1.0).contains(&config.head_sample_ratio) {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "telemetry.head_sample_ratio must lie within 0.0..=1.0",
        ));
    }
    EnvFilter::try_new(&config.log_level).map_err(|e| {
        GapError::new(
            ErrorCode::ValidationError,
            format!("invalid telemetry.log_level `{}`: {e}", config.log_level),
        )
    })
}

#[cfg(feature = "otel")]
fn build_sdk_provider_from_otlp(
    config: &TelemetryConfig,
    endpoint: &str,
) -> GapResult<opentelemetry_sdk::trace::SdkTracerProvider> {
    use opentelemetry_otlp::WithExportConfig as _;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};

    // Validate URI scheme upfront before tonic constructs an endpoint.
    if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "telemetry.otlp_endpoint must start with http:// or https://",
        ));
    }

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.to_owned())
        .build()
        .map_err(|e| {
            GapError::new(
                ErrorCode::ValidationError,
                format!("failed to build OTLP gRPC span exporter: {e}"),
            )
        })?;

    let resource = Resource::builder()
        .with_service_name(config.service_name.clone())
        .build();
    let sampler = Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
        config.head_sample_ratio,
    )));

    Ok(SdkTracerProvider::builder()
        .with_resource(resource)
        .with_sampler(sampler)
        .with_batch_exporter(exporter)
        .build())
}

/// Builds a boxed [`Subscriber`] writing formatted logs (`JSON` or compact) to `make_writer`.
///
/// - Without `--features otel`: configures `EnvFilter` + `tracing_subscriber::fmt` (`JSON` when
///   `config.log_json == true`, compact otherwise). If `config.otlp_endpoint` is set while `otel`
///   is compiled out, returns [`ErrorCode::UnsupportedFeature`] so operators know OTel was not
///   compiled in.
/// - With `--features otel`: additionally attaches `tracing_opentelemetry::layer()` backed by an
///   OTLP gRPC exporter (`OPS-010`) whenever `config.otlp_endpoint` is `Some(...)`.
///
/// # Errors
///
/// Returns [`GapError`] if filter directives, sampling ratio, or OTLP endpoint settings are invalid.
pub fn build_subscriber_with_writer<W>(
    config: &TelemetryConfig,
    make_writer: W,
) -> GapResult<(Box<dyn Subscriber + Send + Sync>, TelemetryGuard)>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let env_filter = parse_env_filter(config)?;

    #[cfg(not(feature = "otel"))]
    {
        if config.otlp_endpoint.is_some() {
            return Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                "telemetry.otlp_endpoint is set, but gap-core was built without the `otel` feature",
            ));
        }
        let base = tracing_subscriber::registry().with(env_filter);
        let subscriber: Box<dyn Subscriber + Send + Sync> = if config.log_json {
            Box::new(
                base.with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_current_span(true)
                        .with_writer(make_writer),
                ),
            )
        } else {
            Box::new(
                base.with(
                    tracing_subscriber::fmt::layer()
                        .compact()
                        .with_writer(make_writer),
                ),
            )
        };
        Ok((subscriber, TelemetryGuard::default()))
    }

    #[cfg(feature = "otel")]
    {
        use opentelemetry::trace::TracerProvider as _;

        let provider = match config.otlp_endpoint.as_deref() {
            Some(endpoint) => Some(build_sdk_provider_from_otlp(config, endpoint)?),
            None => None,
        };
        let otel_layer = provider
            .as_ref()
            .map(|p| tracing_opentelemetry::layer().with_tracer(p.tracer("gap-core")));

        let base = tracing_subscriber::registry()
            .with(otel_layer)
            .with(env_filter);
        let subscriber: Box<dyn Subscriber + Send + Sync> = if config.log_json {
            Box::new(
                base.with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_current_span(true)
                        .with_writer(make_writer),
                ),
            )
        } else {
            Box::new(
                base.with(
                    tracing_subscriber::fmt::layer()
                        .compact()
                        .with_writer(make_writer),
                ),
            )
        };
        Ok((subscriber, TelemetryGuard { provider }))
    }
}

/// Builds a boxed [`Subscriber`] wired to a caller-supplied OpenTelemetry
/// [`opentelemetry_sdk::trace::SpanExporter`] using `SimpleSpanProcessor` (`--features otel`).
///
/// # Errors
///
/// Returns [`GapError`] if `config` filter or sampling parameters are invalid.
#[cfg(feature = "otel")]
pub fn build_subscriber_with_span_exporter<W, E>(
    config: &TelemetryConfig,
    make_writer: W,
    exporter: E,
) -> GapResult<(Box<dyn Subscriber + Send + Sync>, TelemetryGuard)>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
    E: opentelemetry_sdk::trace::SpanExporter + 'static,
{
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};

    let env_filter = parse_env_filter(config)?;
    let resource = Resource::builder()
        .with_service_name(config.service_name.clone())
        .build();
    let sampler = Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
        config.head_sample_ratio,
    )));
    let provider = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_sampler(sampler)
        .with_simple_exporter(exporter)
        .build();

    let otel_layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("gap-core"));
    let base = tracing_subscriber::registry()
        .with(otel_layer)
        .with(env_filter);
    let subscriber: Box<dyn Subscriber + Send + Sync> = if config.log_json {
        Box::new(
            base.with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_current_span(true)
                    .with_writer(make_writer),
            ),
        )
    } else {
        Box::new(
            base.with(
                tracing_subscriber::fmt::layer()
                    .compact()
                    .with_writer(make_writer),
            ),
        )
    };
    Ok((
        subscriber,
        TelemetryGuard {
            provider: Some(provider),
        },
    ))
}

/// Initializes the process-wide global `tracing` subscriber writing to `stderr`.
///
/// # Errors
///
/// Returns [`GapError`] if configuration is invalid or a global subscriber is already installed.
pub fn init_global_telemetry(config: &TelemetryConfig) -> GapResult<TelemetryGuard> {
    let (subscriber, guard) = build_subscriber_with_writer(config, std::io::stderr)?;
    tracing::subscriber::set_global_default(subscriber).map_err(|e| {
        GapError::new(
            ErrorCode::Internal,
            format!("global tracing subscriber already set: {e}"),
        )
    })?;
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use crate::secret::{REDACTED_MARKER, SecretString};

    use super::*;

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn snapshot_string(&self) -> String {
            let bytes = self.0.lock().expect("buffer lock").clone();
            String::from_utf8(bytes).expect("valid utf8 logs")
        }
    }

    struct BufferGuard(Arc<Mutex<Vec<u8>>>);

    impl Write for BufferGuard {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("buffer lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for SharedBuffer {
        type Writer = BufferGuard;

        fn make_writer(&'a self) -> Self::Writer {
            BufferGuard(Arc::clone(&self.0))
        }
    }

    // spec: OPS-011, OPS-020, INV-15
    #[test]
    fn inv_15_structured_json_logs_redact_secrets_and_bound_tenant_buckets() {
        let canary = "sk-live-canary-log-test-0001";
        let secret = SecretString::new(canary.to_owned());
        let tenant: TenantId = "acme-corp".parse().expect("valid tenant");
        let bucket = tenant_bucket(&tenant);
        assert!(bucket < TENANT_BUCKET_COUNT);
        assert_eq!(bucket, tenant_bucket(&tenant));

        let buf = SharedBuffer::default();
        let cfg = TelemetryConfig {
            log_level: "info".to_owned(),
            log_json: true,
            ..TelemetryConfig::default()
        };
        let (subscriber, mut guard) =
            build_subscriber_with_writer(&cfg, buf.clone()).expect("build subscriber");
        assert!(!guard.has_otel_provider());

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                SPAN_REQUEST,
                gap.generation = 3_u64,
                tenant_bucket = bucket,
                credential = %secret
            );
            let _enter = span.enter();
            tracing::info!(debug_secret = ?secret, "processed request");
            tracing::debug!("filtered debug event");
        });
        guard.shutdown().expect("clean shutdown");

        let output = buf.snapshot_string();
        assert!(output.contains("processed request"));
        assert!(output.contains(REDACTED_MARKER));
        assert!(
            !output.contains(canary),
            "structured JSON log must never contain secret canary (INV-15)"
        );
        assert!(!output.contains("filtered debug event"));

        let bad_cfg = TelemetryConfig {
            log_level: "info,[unclosed".to_owned(),
            ..TelemetryConfig::default()
        };
        assert!(parse_env_filter(&bad_cfg).is_err());
    }

    #[cfg(not(feature = "otel"))]
    // spec: OPS-010
    #[test]
    fn otlp_endpoint_without_otel_feature_returns_unsupported_feature() {
        let cfg = TelemetryConfig {
            otlp_endpoint: Some("http://127.0.0.1:4317".to_owned()),
            ..TelemetryConfig::default()
        };
        let res = build_subscriber_with_writer(&cfg, io::sink);
        let err = match res {
            Ok(_) => panic!("expected UnsupportedFeature when otel feature is disabled"),
            Err(e) => e,
        };
        assert_eq!(err.code(), ErrorCode::UnsupportedFeature);
    }

    #[cfg(feature = "otel")]
    mod otel_tests {
        use std::future::{Future, ready};

        use opentelemetry_sdk::error::OTelSdkResult;
        use opentelemetry_sdk::trace::{SpanData, SpanExporter};

        use super::*;

        #[derive(Debug, Clone, Default)]
        struct RecordingExporter {
            spans: Arc<Mutex<Vec<SpanData>>>,
        }

        impl SpanExporter for RecordingExporter {
            fn export(&self, batch: Vec<SpanData>) -> impl Future<Output = OTelSdkResult> + Send {
                self.spans.lock().expect("spans lock").extend(batch);
                ready(Ok(()))
            }
        }

        // spec: OPS-010, OPS-011, RUN-090, INV-15
        #[test]
        fn inv_15_otel_layer_exports_spans_with_redacted_secrets_and_validates_otlp_uri() {
            let exporter = RecordingExporter::default();
            let buf = SharedBuffer::default();
            let cfg = TelemetryConfig {
                log_level: "info".to_owned(),
                log_json: false,
                service_name: "gap-test-node".to_owned(),
                head_sample_ratio: 1.0,
                ..TelemetryConfig::default()
            };

            let (subscriber, mut guard) =
                build_subscriber_with_span_exporter(&cfg, buf.clone(), exporter.clone())
                    .expect("build otel subscriber");
            assert!(guard.has_otel_provider());

            let canary = "sk-live-otel-span-canary-9999";
            let secret = SecretString::new(canary.to_owned());

            tracing::subscriber::with_default(subscriber, || {
                let span = tracing::info_span!(
                    "gap.request",
                    gap.generation = 11_u64,
                    secret_attr = %secret
                );
                let _enter = span.enter();
                tracing::info!("inside otel span");
            });

            guard.shutdown().expect("otel guard shutdown");
            assert!(!guard.has_otel_provider());

            let recorded = exporter.spans.lock().expect("spans lock");
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].name, "gap.request");
            for kv in &recorded[0].attributes {
                assert!(
                    !kv.value.as_str().contains(canary),
                    "OTel span attribute leaked canary secret (INV-15)"
                );
            }
            drop(recorded);

            // Invalid OTLP gRPC scheme is rejected cleanly with GAP_VALIDATION_ERROR.
            let bad_otlp = TelemetryConfig {
                otlp_endpoint: Some("ftp://127.0.0.1:4317".to_owned()),
                ..TelemetryConfig::default()
            };
            let err = match build_subscriber_with_writer(&bad_otlp, io::sink) {
                Ok(_) => panic!("expected invalid OTLP scheme to fail"),
                Err(e) => e,
            };
            assert_eq!(err.code(), ErrorCode::ValidationError);
        }
    }
}

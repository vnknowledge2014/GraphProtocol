// Canary-secret leakage tests across logs, traces, errors, metrics, and Enricher inputs (`WP-M6-07`).
// spec: INV-15, SEC-040, SEC-041, SEC-042
// TM-18

use std::collections::BTreeMap;

use gap_core::error::{ErrorCode, GapError, NgcErrorCode, NgcErrorInfo};
use gap_core::snapshot::Protocol;
use gap_ingest::cisg::{
    Cisg, DocBlob, ExtractorInfo, FieldDef, SourceRef, TypeDef, TypeRef, UntrustedText,
};
use gap_ingest::enrich::{EnrichmentConfig, build_isolated_enrichment_prompt};
use gap_sec::vault::{Redactor, SecretString};

const CANARIES: &[(&str, &str)] = &[
    (
        "vault_branch_secret",
        "GAP_CANARY_VAULT_SECRET_A9F1C2D3E4B5",
    ),
    (
        "bearer_jwt",
        "eyJhbGciOiJFZERTQSJ9.eyJzdWIiOiJjYW5hcnkifQ.SignatureCanary998877",
    ),
    (
        "pem_private_key",
        "-----BEGIN PRIVATE KEY-----\nMIIBCANARYSECRETKEYDATA0123456789\n-----END PRIVATE KEY-----",
    ),
    ("aws_access_key", "AKIAIOSFODNN7CANARY0"),
    ("github_pat", "ghp_CanarySecretToken1234567890abcdef"),
    ("url_userinfo", "canaryuser:CanaryPass1234"),
    ("openai_key", "sk-canarySecretKey9876543210abcd"),
];

fn assert_no_canaries(sink_name: &str, text: &str) {
    for (label, canary) in CANARIES {
        assert!(
            !text.contains(canary),
            "canary `{label}` leaked into {sink_name}: {text}"
        );
    }
    assert!(
        !text.contains("MIIBCANARYSECRETKEYDATA0123456789"),
        "PEM body leaked into {sink_name}: {text}"
    );
}

// spec: INV-15, SEC-041, SEC-042
// TM-18
#[test]
fn secret_canary_never_appears_in_logs_traces_errors_metrics_or_enricher_inputs() {
    let redactor = Redactor::new();
    let vault_secret = SecretString::from_slice(CANARIES[0].1);
    redactor.register_secret(&vault_secret);

    let hostile_payload = format!(
        "request auth=Bearer {jwt} custom={vault} aws={aws} gh={gh} oai={oai} endpoint=https://{ui}@upstream.internal:8443/v1 key={pem} password=\"CanaryPass1234\"",
        vault = vault_secret.expose_secret(),
        jwt = CANARIES[1].1,
        pem = CANARIES[2].1,
        aws = CANARIES[3].1,
        gh = CANARIES[4].1,
        ui = CANARIES[5].1,
        oai = CANARIES[6].1,
    );

    // 1. SecretString Debug/Display never reveals secret bytes
    assert_no_canaries("SecretString::Debug", &format!("{vault_secret:?}"));
    assert_no_canaries("SecretString::Display", &format!("{vault_secret}"));

    // 2. Log sink
    let log_out = redactor.redact_log(&hostile_payload);
    assert_no_canaries("log_sink", &log_out);

    // 3. OpenTelemetry span attribute sink
    let span_in = BTreeMap::from([
        (
            "http.request.header.authorization".to_owned(),
            format!("Bearer {}", CANARIES[1].1),
        ),
        (
            "authorization".to_owned(),
            vault_secret.expose_secret().to_owned(),
        ),
        (
            "branch.endpoint".to_owned(),
            format!("https://{}@db.corp:5432", CANARIES[5].1),
        ),
        ("upstream.detail".to_owned(), hostile_payload.clone()),
    ]);
    let span_out = redactor.redact_trace_attributes(&span_in);
    for (k, v) in &span_out {
        assert_no_canaries(&format!("trace_attr[{k}]"), v);
    }

    // 4. Error formatting sinks (GapError & NgcErrorInfo)
    let gap_err = GapError::new(
        ErrorCode::BranchError,
        redactor.redact_error_message(&hostile_payload),
    );
    assert_no_canaries("GapError", &gap_err.to_string());

    let ngc_err = NgcErrorInfo::new(
        NgcErrorCode::Unavailable,
        redactor.redact_error_message(&hostile_payload),
    );
    assert_no_canaries("NgcErrorInfo", ngc_err.message());

    // 5. Metric label sink
    let metrics_in = BTreeMap::from([
        ("branch".to_owned(), "billing".to_owned()),
        ("token".to_owned(), CANARIES[4].1.to_owned()),
        (
            "error.kind".to_owned(),
            format!("failed for {}", CANARIES[3].1),
        ),
    ]);
    let metrics_out = redactor.redact_metric_labels(&metrics_in);
    for (k, v) in &metrics_out {
        assert_no_canaries(&format!("metric_label[{k}]"), v);
    }

    // 6. Enricher / model prompt input sink (SEC-042 + gap-ingest build_isolated_enrichment_prompt)
    let enricher_text = redactor.sanitize_enricher_input(&hostile_payload, 32_768);
    assert_no_canaries("Redactor::sanitize_enricher_input", &enricher_text);

    let cisg = Cisg {
        source: SourceRef {
            kind: Protocol::Rest,
            location_canonical: format!("https://{}@internal.corp/spec.json", CANARIES[5].1),
            source_hash: "a".repeat(64),
        },
        extractor: ExtractorInfo {
            id: "gap-ingest-openapi".to_owned(),
            version: "0.1.0".to_owned(),
        },
        types: vec![TypeDef {
            name: "Invoice".to_owned(),
            kind: "OBJECT".to_owned(),
            fields: vec![FieldDef {
                name: "owner".to_owned(),
                field_type: TypeRef::Named("String".to_owned()),
                is_pii: true,
                description: UntrustedText::from_raw(format!("api_key={}", CANARIES[4].1)),
            }],
            enum_values: Vec::new(),
            json_schema: None,
            description: UntrustedText::from_raw(hostile_payload),
        }],
        operations: Vec::new(),
        node_categories: Vec::new(),
        edge_predicates: Vec::new(),
        keys: Vec::new(),
        candidate_joins: Vec::new(),
        docs: vec![DocBlob {
            id: "doc-api-guide".to_owned(),
            source: "README.md".to_owned(),
            content: UntrustedText::from_raw(format!(
                "Use sk-canarySecretKey9876543210abcd or {} and ghp_CanarySecretToken1234567890abcdef",
                CANARIES[1].1
            )),
        }],
    };

    let raw_prompt = build_isolated_enrichment_prompt(&cisg, &EnrichmentConfig::default());
    let final_prompt = redactor.sanitize_enricher_input(&raw_prompt, 32_768);
    assert_no_canaries("Enricher prompt", &final_prompt);
}

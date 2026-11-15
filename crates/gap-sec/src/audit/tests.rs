//! Unit tests for `gap-sec::audit` (`SEC-091`, `SEC-092`, `SEC-093`, `INV-14`, `TM-17`).

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use gap_core::clock::Timestamp;

use crate::authn::crypto::{Ed25519SigningKey, blake3_hash, hex_encode, sha256};
use crate::authn::json::JsonValue;

use super::*;

struct TempTestDir(PathBuf);

impl TempTestDir {
    fn create(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp test dir");
        Self(dir)
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// spec: SEC-091, TM-17, INV-14
#[test]
fn audit_anchor_ed25519_sinks_cadence_and_truncation_detection() {
    let seed = [0x42u8; 32];
    let signing = Ed25519SigningKey::from_seed(seed).expect("signing key");
    let verifying = signing.verifying_key().expect("verifying key");

    let t0 = Timestamp::from_unix_millis(1_700_000_000_000).expect("t0");
    let head_hash = blake3_hash(b"chain-head-at-seq-42");

    // Scheduler enforces default 10-minute (600 s) cadence.
    let mut scheduler = AnchorScheduler::default();
    assert_eq!(scheduler.interval_secs(), DEFAULT_ANCHOR_INTERVAL_SECS);
    assert!(scheduler.is_due("tenant:acme", t0));
    scheduler.record_anchored("tenant:acme", t0);

    let t_plus_5m = Timestamp::from_unix_millis(1_700_000_300_000).expect("5m");
    assert!(!scheduler.is_due("tenant:acme", t_plus_5m));

    let t_plus_10m = Timestamp::from_unix_millis(1_700_000_600_000).expect("10m");
    assert!(scheduler.is_due("tenant:acme", t_plus_10m));

    // Sign and JSON-roundtrip anchor.
    let anchor = AuditAnchor::sign("tenant:acme", 42, head_hash, t0, "gw-key-1", &signing)
        .expect("sign anchor");
    anchor.verify_signature(&verifying).expect("verify sig");

    let json = anchor.to_json_string();
    let parsed = AuditAnchor::from_json_str(&json).expect("parse anchor");
    assert_eq!(parsed, anchor);

    // Emit to File, Syslog (RFC 5424), and S3-compatible sinks.
    let temp = TempTestDir::create("gap-audit-anchor");
    let anchor_file = temp.0.join("anchors.ndjson");
    let file_sink = FileAnchorSink::new(&anchor_file);
    assert_eq!(file_sink.sink_kind(), "file");
    file_sink.emit_anchor(&anchor).expect("file emit");
    let file_contents = fs::read_to_string(file_sink.path()).expect("read anchor file");
    assert!(file_contents.contains("\"chain_id\":\"tenant:acme\""));

    let syslog_sink = SyslogAnchorSink::new("gw-edge-01", InMemorySyslogTransport::new());
    assert_eq!(syslog_sink.sink_kind(), "syslog");
    syslog_sink.emit_anchor(&anchor).expect("syslog emit");
    let syslog_lines = syslog_sink.transport().lines();
    assert_eq!(syslog_lines.len(), 1);
    assert!(syslog_lines[0].starts_with("<134>1 "));
    assert!(syslog_lines[0].contains("chain=\"tenant:acme\""));
    assert!(syslog_lines[0].contains("seq=\"42\""));

    let s3_sink = S3CompatibleAnchorSink::new(
        "gap-audit-worm",
        "prod/anchors",
        InMemoryObjectBucket::new(),
    );
    assert_eq!(s3_sink.sink_kind(), "s3");
    s3_sink.emit_anchor(&anchor).expect("s3 emit");
    let objs = s3_sink.transport().objects();
    assert_eq!(objs.len(), 1);
    assert!(
        objs[0]
            .object_key
            .starts_with("prod/anchors/tenant/acme/00000000000000000042-")
    );
    assert_eq!(objs[0].sha256_hex, hex_encode(&sha256(&objs[0].body)));

    // Truncation / hash rewrite detection against anchor (`TM-17`).
    assert_eq!(
        verify_chain_against_anchor(&anchor, &verifying, "tenant:acme", 50, Some(head_hash)),
        AnchorVerifyStatus::MatchesAnchor
    );
    assert_eq!(
        verify_chain_against_anchor(&anchor, &verifying, "tenant:acme", 41, None),
        AnchorVerifyStatus::Truncated {
            anchor_seq: 42,
            observed_head_seq: 41,
        }
    );
    assert_eq!(
        verify_chain_against_anchor(&anchor, &verifying, "tenant:acme", 42, Some([0xFF; 32])),
        AnchorVerifyStatus::HashRewritten { anchor_seq: 42 }
    );

    // Wrong key rejects forged anchor.
    let other_verifying = Ed25519SigningKey::from_seed([0x99u8; 32])
        .expect("other")
        .verifying_key()
        .expect("vk");
    assert!(matches!(
        verify_chain_against_anchor(
            &anchor,
            &other_verifying,
            "tenant:acme",
            42,
            Some(head_hash)
        ),
        AnchorVerifyStatus::SignatureInvalid(_)
    ));
}

// spec: SEC-093, SEC-092
#[test]
fn audit_prov_jsonld_export_maps_activities_agents_delegation_and_entities() {
    let ts1 = Timestamp::from_unix_millis(1_700_000_001_000).expect("ts1");
    let ts2 = Timestamp::from_unix_millis(1_700_000_002_000).expect("ts2");
    let records = vec![
        ProvAuditRecord {
            chain_id: "tenant:acme".to_owned(),
            seq: 1,
            ts: ts1,
            actor_chain: vec!["agent:planner".to_owned(), "user:alice".to_owned()],
            action: "execute.getOrder".to_owned(),
            resource: "crm.Order/101".to_owned(),
            decision: "ALLOW".to_owned(),
            entity_hash_hex: hex_encode(&blake3_hash(b"order-101")),
            record_hash_hex: hex_encode(&blake3_hash(b"rec-1")),
        },
        ProvAuditRecord {
            chain_id: "tenant:acme".to_owned(),
            seq: 2,
            ts: ts2,
            actor_chain: vec!["agent:planner".to_owned(), "user:alice".to_owned()],
            action: "mutate.refundOrder".to_owned(),
            resource: "crm.Order/101".to_owned(),
            decision: "ALLOW".to_owned(),
            entity_hash_hex: hex_encode(&blake3_hash(b"refund-101")),
            record_hash_hex: hex_encode(&blake3_hash(b"rec-2")),
        },
    ];

    let jsonld = export_prov_jsonld(&records);
    let parsed = JsonValue::parse(&jsonld).expect("valid PROV-O JSON-LD");
    let ctx = parsed.get("@context").expect("@context");
    assert_eq!(ctx.get_str("prov"), Some("http://www.w3.org/ns/prov#"));
    assert_eq!(ctx.get_str("gap"), Some("https://gap.protocol/ns/audit#"));

    let graph = parsed
        .get("@graph")
        .and_then(JsonValue::as_array)
        .expect("@graph array");
    let activity_count = graph
        .iter()
        .filter(|n| n.get_str("@type") == Some("prov:Activity"))
        .count();
    let entity_count = graph
        .iter()
        .filter(|n| n.get_str("@type") == Some("prov:Entity"))
        .count();
    let delegated_agents = graph
        .iter()
        .filter(|n| {
            n.get_str("@type") == Some("prov:Agent")
                && n.get("prov:actedOnBehalfOf").and_then(|d| d.get_str("@id"))
                    == Some("urn:gap:agent:user:alice")
        })
        .count();

    assert_eq!(activity_count, 2);
    assert_eq!(entity_count, 2);
    assert_eq!(delegated_agents, 1);
}

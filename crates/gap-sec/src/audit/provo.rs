//! W3C `PROV-O` JSON-LD audit exporter (`SEC-093`).

use std::collections::{BTreeMap, BTreeSet};

use gap_core::clock::Timestamp;

use crate::authn::json::JsonValue;

/// Audit record projection exported to W3C `PROV-O` (`prov-jsonld`, `SEC-093`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvAuditRecord {
    /// Chain identifier (`"system"` or `"tenant:<id>"`).
    pub chain_id: String,
    /// Strictly monotonic per-chain sequence number.
    pub seq: u64,
    /// UTC timestamp of the recorded operation.
    pub ts: Timestamp,
    /// Principal delegation chain (`actor_chain`, outermost actor first, `SEC-012`, `SEC-093`).
    pub actor_chain: Vec<String>,
    /// Operation or control-plane verb (`prov:Activity`).
    pub action: String,
    /// Target resource identifier (`prov:Entity`).
    pub resource: String,
    /// Deterministic policy/execution decision (`ALLOW`, `DENY`, etc.).
    pub decision: String,
    /// Lowercase hex cryptographic digest of the referenced entity/payload (`SEC-093`).
    pub entity_hash_hex: String,
    /// Lowercase hex digest of this audit record in its hash chain.
    pub record_hash_hex: String,
}

/// Exports `records` as deterministic W3C `PROV-O` JSON-LD (`gap audit export --format prov-jsonld`, `SEC-093`).
///
/// Maps each record to:
/// - `prov:Activity` (the operation/action with `prov:startedAtTime`, `prov:wasAssociatedWith`, and `prov:generated`/`prov:used`),
/// - `prov:Agent` (each principal in `actor_chain`, linked via `prov:actedOnBehalfOf` across delegations), and
/// - `prov:Entity` (resources/results identified by cryptographic hash).
#[must_use]
pub fn export_prov_jsonld(records: &[ProvAuditRecord]) -> String {
    let mut context = BTreeMap::new();
    context.insert(
        "gap".to_owned(),
        JsonValue::String("https://gap.protocol/ns/audit#".to_owned()),
    );
    context.insert(
        "prov".to_owned(),
        JsonValue::String("http://www.w3.org/ns/prov#".to_owned()),
    );
    context.insert(
        "xsd".to_owned(),
        JsonValue::String("http://www.w3.org/2001/XMLSchema#".to_owned()),
    );

    let mut graph_nodes: Vec<JsonValue> = Vec::new();
    let mut emitted_agents: BTreeSet<String> = BTreeSet::new();
    let mut emitted_entities: BTreeSet<String> = BTreeSet::new();

    for rec in records {
        // Emit Agents and `prov:actedOnBehalfOf` delegation edges (`outermost -> subject`).
        let mut agent_uris = Vec::with_capacity(rec.actor_chain.len());
        for (idx, actor) in rec.actor_chain.iter().enumerate() {
            let agent_uri = format!("urn:gap:agent:{}", sanitize_urn_segment(actor));
            let delegate_of_uri = rec
                .actor_chain
                .get(idx + 1)
                .map(|parent| format!("urn:gap:agent:{}", sanitize_urn_segment(parent)));
            let agent_dedup_key =
                format!("{agent_uri}|{}", delegate_of_uri.as_deref().unwrap_or(""));
            if emitted_agents.insert(agent_dedup_key) {
                let mut agent_obj = BTreeMap::new();
                agent_obj.insert("@id".to_owned(), JsonValue::String(agent_uri.clone()));
                agent_obj.insert(
                    "@type".to_owned(),
                    JsonValue::String("prov:Agent".to_owned()),
                );
                agent_obj.insert("gap:principal".to_owned(), JsonValue::String(actor.clone()));
                if let Some(ref parent_uri) = delegate_of_uri {
                    let mut ref_obj = BTreeMap::new();
                    ref_obj.insert("@id".to_owned(), JsonValue::String(parent_uri.clone()));
                    agent_obj.insert(
                        "prov:actedOnBehalfOf".to_owned(),
                        JsonValue::Object(ref_obj),
                    );
                }
                graph_nodes.push(JsonValue::Object(agent_obj));
            }
            agent_uris.push(agent_uri);
        }

        // Emit Entity keyed by resource + hash (`SEC-093`).
        let entity_uri = format!(
            "urn:gap:entity:{}:{}",
            sanitize_urn_segment(&rec.resource),
            sanitize_urn_segment(&rec.entity_hash_hex)
        );
        if emitted_entities.insert(entity_uri.clone()) {
            let mut entity_obj = BTreeMap::new();
            entity_obj.insert("@id".to_owned(), JsonValue::String(entity_uri.clone()));
            entity_obj.insert(
                "@type".to_owned(),
                JsonValue::String("prov:Entity".to_owned()),
            );
            entity_obj.insert(
                "gap:digest".to_owned(),
                JsonValue::String(rec.entity_hash_hex.clone()),
            );
            entity_obj.insert(
                "gap:resource".to_owned(),
                JsonValue::String(rec.resource.clone()),
            );
            graph_nodes.push(JsonValue::Object(entity_obj));
        }

        // Emit Activity for this audit record.
        let activity_uri = format!(
            "urn:gap:activity:{}:{}",
            sanitize_urn_segment(&rec.chain_id),
            rec.seq
        );
        let mut act_obj = BTreeMap::new();
        act_obj.insert("@id".to_owned(), JsonValue::String(activity_uri));
        act_obj.insert(
            "@type".to_owned(),
            JsonValue::String("prov:Activity".to_owned()),
        );
        act_obj.insert(
            "gap:action".to_owned(),
            JsonValue::String(rec.action.clone()),
        );
        act_obj.insert(
            "gap:chainId".to_owned(),
            JsonValue::String(rec.chain_id.clone()),
        );
        act_obj.insert(
            "gap:decision".to_owned(),
            JsonValue::String(rec.decision.clone()),
        );
        act_obj.insert(
            "gap:recordHash".to_owned(),
            JsonValue::String(rec.record_hash_hex.clone()),
        );
        act_obj.insert(
            "gap:seq".to_owned(),
            JsonValue::Int(i64::try_from(rec.seq).unwrap_or(i64::MAX)),
        );
        act_obj.insert(
            "prov:startedAtTime".to_owned(),
            JsonValue::String(rec.ts.to_string()),
        );

        let entity_ref = {
            let mut m = BTreeMap::new();
            m.insert("@id".to_owned(), JsonValue::String(entity_uri));
            JsonValue::Object(m)
        };
        act_obj.insert("prov:used".to_owned(), entity_ref);

        let assoc_refs: Vec<JsonValue> = agent_uris
            .into_iter()
            .map(|uri| {
                let mut m = BTreeMap::new();
                m.insert("@id".to_owned(), JsonValue::String(uri));
                JsonValue::Object(m)
            })
            .collect();
        act_obj.insert(
            "prov:wasAssociatedWith".to_owned(),
            JsonValue::Array(assoc_refs),
        );

        graph_nodes.push(JsonValue::Object(act_obj));
    }

    let mut root = BTreeMap::new();
    root.insert("@context".to_owned(), JsonValue::Object(context));
    root.insert("@graph".to_owned(), JsonValue::Array(graph_nodes));
    JsonValue::Object(root).to_jcs_string()
}

fn sanitize_urn_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

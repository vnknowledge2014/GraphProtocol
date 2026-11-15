//! Injection linting, `SecurityLintHook` integration, `S1`/Enricher input filtering, and
//! spotlight delimiting (`SEC-080`..`SEC-085`, `WP-M6-10`).

pub mod classifier;
pub mod rules;
pub mod spotlight;

#[cfg(test)]
mod tests;

pub use classifier::{DEFAULT_CLASSIFIER_THRESHOLD_MILLI, InjectionLinter, LintReport};
pub use rules::{
    HeuristicMatchReport, HiddenUnicodeSummary, evaluate_heuristics, strip_hidden_unicode,
};
pub use spotlight::{
    ApprovalSummarySpec, DEFAULT_MODEL_ITEM_MAX_BYTES, FilteredModelInputs, ModelInputSlice,
    ProvenanceWrappedResult, SPOTLIGHT_STATIC_DESCRIPTION, SpotlightConfig,
    filter_for_enricher_input, filter_for_s1_input, format_result_with_provenance,
    is_lexically_searchable, neutralize_delimiter_tags, render_approval_summary,
};

use gap_core::snapshot::SideEffectLevel;

use crate::cisg::{Cisg, UntrustedText};
use crate::gates::{DefaultSecurityLintStub, G7SecurityConfig, SecurityLintHook};

/// Pinned element description hash paired with immutable `SideEffectLevel` (`SEC-082`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedOperationDescription {
    /// Operation identifier.
    pub operation_name: String,
    /// `desc_hash` stored at admin approval (`SEC-082`).
    pub desc_hash: String,
    /// Verifiably pinned side-effect classification (immutable without approval, `SEC-082`).
    pub side_effect: SideEffectLevel,
}

/// Complete audit summary produced by [`DescriptionInjectionLinter::audit_cisg`] for admin review (`SEC-081`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CisgLintAudit {
    /// Clean element identifiers safe for S1/Enricher.
    pub clean_ids: Vec<String>,
    /// Flagged element `(id, reasons, sanitized_preview)` withheld for admin review (`SEC-081`).
    pub flagged_for_admin_review: Vec<(String, Vec<String>, String)>,
}

/// Production Gate G7 security linter integrating heuristic + classifier injection checks (`SEC-081`, `ING-001`).
#[derive(Debug, Clone, Default)]
pub struct DescriptionInjectionLinter {
    /// Underlying injection heuristic + linear classifier engine.
    pub engine: InjectionLinter,
}

impl DescriptionInjectionLinter {
    /// Constructs a linter with default weights and thresholds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Produces a structured admin-review partition across all untrusted text fields in `cisg` (`SEC-081`).
    #[must_use]
    pub fn audit_cisg(&self, cisg: &Cisg) -> CisgLintAudit {
        let mut audit = CisgLintAudit::default();
        self.record_text(&cisg.source.location_canonical, "source.uri", &mut audit);

        for ty in &cisg.types {
            let id = format!("type:{}", ty.name);
            self.record_untrusted(&ty.description, &id, &mut audit);
            for field in &ty.fields {
                let fid = format!("field:{}.{}", ty.name, field.name);
                self.record_untrusted(&field.description, &fid, &mut audit);
            }
        }

        for op in &cisg.operations {
            let oid = format!("op:{}", op.name);
            self.record_untrusted(&op.description, &oid, &mut audit);
            for arg in &op.args {
                let aid = format!("arg:{}.{}", op.name, arg.name);
                self.record_untrusted(&arg.description, &aid, &mut audit);
            }
            for (idx, ex) in op.examples.iter().enumerate() {
                let eid = format!("example:{}[{idx}]", op.name);
                self.record_untrusted(&ex.summary, &eid, &mut audit);
            }
        }

        for doc in &cisg.docs {
            let did = format!("doc:{}", doc.id);
            self.record_untrusted(&doc.content, &did, &mut audit);
        }

        audit
    }

    fn record_untrusted(&self, ut: &UntrustedText, id: &str, audit: &mut CisgLintAudit) {
        self.record_text(&ut.text, id, audit);
    }

    fn record_text(&self, raw: &str, id: &str, audit: &mut CisgLintAudit) {
        let report = self.engine.analyze(raw);
        if report.is_flagged() {
            audit
                .flagged_for_admin_review
                .push((id.to_owned(), report.reasons, report.sanitized));
        } else {
            audit.clean_ids.push(id.to_owned());
        }
    }
}

impl SecurityLintHook for DescriptionInjectionLinter {
    fn check(&self, cisg: &Cisg, cfg: &G7SecurityConfig) -> Vec<String> {
        let mut findings = DefaultSecurityLintStub.check(cisg, cfg);
        let audit = self.audit_cisg(cisg);
        for (elem_id, reasons, _sanitized) in audit.flagged_for_admin_review {
            let entry = format!(
                "SEC-081 injection linter flagged `{elem_id}`: {}",
                reasons.join(", ")
            );
            if !findings
                .iter()
                .any(|existing| existing.contains(&elem_id) && existing.contains(&reasons[0]))
            {
                findings.push(entry);
            }
        }
        findings
    }
}

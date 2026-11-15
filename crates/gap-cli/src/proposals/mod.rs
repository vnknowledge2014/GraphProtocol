//! Ingestion proposal review CLI (`list`, `show`, `approve`, `deny`) (`WP-M4-07`;
//! `ING-001`..`ING-003`, `ING-051`, `spec/02-gap-ql.md` §3).

use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use gap_core::clock::SystemClock;
use gap_core::cursor::CursorKey;
use gap_core::ids::{ProposalId, TenantId, TenantScope};
use gap_ingest::proposals::{
    ADMIN_INGEST_SCOPE, ADMIN_SCHEMA_SCOPE, AdminCaller, ApprovalDecision, ApprovalPolicyConfig,
    Proposal, ProposalError, ProposalFilter, ProposalService, ProposalStatus, format_protocol,
    format_trust_tier, render_proposal_cisg, render_proposal_connection_cisg,
};
use gap_store::sqlite::{SqliteConfig, SqliteContextStore};

/// Exit code `0` for successful proposal CLI execution.
pub const EXIT_PROPOSALS_OK: u8 = 0;
/// Exit code `1` for domain or storage execution errors.
pub const EXIT_PROPOSALS_RUNTIME_ERROR: u8 = 1;
/// Exit code `2` for invalid CLI arguments or validation usage errors.
pub const EXIT_PROPOSALS_USAGE_ERROR: u8 = 2;

/// Local operator CLI for inspecting and deciding ingestion proposals (`gap proposals ...`).
#[derive(Debug, Clone, Parser)]
#[command(
    name = "gap proposals",
    about = "Inspect and approve or deny ingestion proposals"
)]
pub struct ProposalsCli {
    /// Path to the GAP SQLite context store file (`proposals` and `audit` chains).
    #[arg(
        long,
        global = true,
        default_value = ".gap/context.sqlite",
        value_name = "PATH"
    )]
    pub db: PathBuf,
    /// Tenant slug owning the proposal ledger (`INV-09`).
    #[arg(long, global = true, default_value = "default", value_name = "TENANT")]
    pub tenant: String,
    /// Administrative principal recorded on approvals/denials and audit entries (`INV-14`).
    #[arg(
        long,
        global = true,
        default_value = "cli-admin",
        value_name = "PRINCIPAL"
    )]
    pub principal: String,
    /// Emit structured JSON instead of human-readable text output.
    #[arg(long, global = true)]
    pub json: bool,
    /// Selected subcommand (`list`, `show`, `approve`, `deny`).
    #[command(subcommand)]
    pub command: ProposalsCommand,
}

/// Subcommands supported by `gap proposals` (`WP-M4-07`).
#[derive(Debug, Clone, Subcommand)]
pub enum ProposalsCommand {
    /// List proposals filtered by status with signed cursor pagination.
    List {
        /// Optional status filter(s) (repeatable: `PROPOSED`, `AWAITING_APPROVAL`, etc.).
        #[arg(long = "status", value_name = "STATUS")]
        statuses: Vec<String>,
        /// Maximum number of proposals to return (`1..=1000`).
        #[arg(long, default_value_t = 20, value_name = "N")]
        first: u32,
        /// Signed pagination continuation cursor returned by a prior `list --json`.
        #[arg(long, value_name = "CURSOR")]
        after: Option<String>,
    },
    /// Show detailed status, approvals, and verification report for a proposal.
    Show {
        /// Proposal UUIDv7 identifier.
        #[arg(value_name = "ID")]
        id: String,
    },
    /// Approve a proposal at `AWAITING_APPROVAL` (`admin:ingest`, `ING-051`).
    Approve {
        /// Proposal UUIDv7 identifier.
        #[arg(value_name = "ID")]
        id: String,
        /// Explicit acknowledgement required when verification flagged a breaking change (`ING-051`).
        #[arg(long)]
        breaking_change_ack: bool,
        /// Optional rationale persisted to notes and the tamper-evident audit log.
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
    },
    /// Deny a proposal (`PROPOSED` or `AWAITING_APPROVAL` -> `REJECTED`).
    Deny {
        /// Proposal UUIDv7 identifier.
        #[arg(value_name = "ID")]
        id: String,
        /// Optional denial rationale persisted to notes and audit log.
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
    },
}

/// Executes `gap proposals <subcommand>` in-process and writes output to `stdout`/`stderr`.
///
/// Returns exit code `0` on success, `1` on runtime/domain failure, or `2` on invalid CLI syntax
/// or validation input (`Brief §3.4`).
pub fn run_proposals_cli<I, T>(args: I, stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cli = match ProposalsCli::try_parse_from(args) {
        Ok(parsed) => parsed,
        Err(err) => {
            if err.use_stderr() {
                let _ = write!(stderr, "{err}");
                return EXIT_PROPOSALS_USAGE_ERROR;
            }
            let _ = write!(stdout, "{err}");
            return EXIT_PROPOSALS_OK;
        }
    };

    run_with_config(
        cli,
        ApprovalPolicyConfig::default(),
        default_cli_cursor_key(),
        stdout,
        stderr,
    )
}

fn default_cli_cursor_key() -> CursorKey {
    // Deterministic fallback key for stateless single-process / local SQLite CLI invocations.
    CursorKey::from_array([0x50; 32])
}

fn run_with_config(
    cli: ProposalsCli,
    policy: ApprovalPolicyConfig,
    cursor_key: CursorKey,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let tenant_id = match TenantId::new(&cli.tenant) {
        Ok(t) => t,
        Err(err) => {
            let _ = writeln!(stderr, "error: invalid tenant `{}`: {err}", cli.tenant);
            return EXIT_PROPOSALS_USAGE_ERROR;
        }
    };
    let tenant: TenantScope = tenant_id.scope();

    let clock = Arc::new(SystemClock);
    let store = match SqliteContextStore::open(SqliteConfig::new(&cli.db), clock.clone()) {
        Ok(s) => Arc::new(s),
        Err(err) => {
            let _ = writeln!(
                stderr,
                "error: failed to open store `{}`: {err}",
                cli.db.display()
            );
            return EXIT_PROPOSALS_RUNTIME_ERROR;
        }
    };
    let service = match ProposalService::with_store_audit(store, clock, policy, cursor_key) {
        Ok(s) => s,
        Err(err) => {
            let _ = writeln!(
                stderr,
                "error: failed to initialize proposal service: {err}"
            );
            return EXIT_PROPOSALS_RUNTIME_ERROR;
        }
    };
    let caller = AdminCaller::new(&cli.principal, [ADMIN_INGEST_SCOPE, ADMIN_SCHEMA_SCOPE]);

    match cli.command {
        ProposalsCommand::List {
            statuses,
            first,
            after,
        } => {
            let parsed_statuses = if statuses.is_empty() {
                None
            } else {
                let mut out = Vec::with_capacity(statuses.len());
                for raw in statuses {
                    match ProposalStatus::from_str(&raw.trim().to_ascii_uppercase()) {
                        Ok(st) => out.push(st),
                        Err(err) => {
                            let _ = writeln!(stderr, "error: {err}");
                            return EXIT_PROPOSALS_USAGE_ERROR;
                        }
                    }
                }
                Some(out)
            };
            let filter = ProposalFilter {
                statuses: parsed_statuses,
                first,
                after,
            };
            match service.list_proposals(&tenant, filter) {
                Ok(conn) => {
                    if cli.json {
                        match render_proposal_connection_cisg(&conn).to_canonical_json() {
                            Ok(json_str) => {
                                let _ = writeln!(stdout, "{json_str}");
                                EXIT_PROPOSALS_OK
                            }
                            Err(err) => {
                                let _ = writeln!(stderr, "error: failed to render JSON: {err}");
                                EXIT_PROPOSALS_RUNTIME_ERROR
                            }
                        }
                    } else {
                        for p in &conn.nodes {
                            let _ = writeln!(
                                stdout,
                                "{}\t{}\t{}\t{}\t{}",
                                p.id,
                                p.status.as_str(),
                                format_protocol(p.kind),
                                format_trust_tier(p.trust),
                                p.location
                            );
                        }
                        if conn.page_info.has_next_page
                            && let Some(cursor) = &conn.page_info.end_cursor
                        {
                            let _ = writeln!(stdout, "next_cursor: {cursor}");
                        }
                        EXIT_PROPOSALS_OK
                    }
                }
                Err(err) => emit_proposal_error(&err, stderr),
            }
        }
        ProposalsCommand::Show { id } => {
            let proposal_id = match parse_proposal_id(&id, stderr) {
                Ok(pid) => pid,
                Err(code) => return code,
            };
            match service.get_proposal(&tenant, proposal_id) {
                Ok(Some(proposal)) => emit_proposal(&proposal, cli.json, stdout, stderr),
                Ok(None) => {
                    let _ = writeln!(stderr, "error: proposal `{proposal_id}` not found");
                    EXIT_PROPOSALS_RUNTIME_ERROR
                }
                Err(err) => emit_proposal_error(&err, stderr),
            }
        }
        ProposalsCommand::Approve {
            id,
            breaking_change_ack,
            reason,
        } => {
            let proposal_id = match parse_proposal_id(&id, stderr) {
                Ok(pid) => pid,
                Err(code) => return code,
            };
            match service.decide_proposal(
                &tenant,
                &caller,
                proposal_id,
                ApprovalDecision::Approve,
                breaking_change_ack,
                reason,
            ) {
                Ok(proposal) => emit_proposal(&proposal, cli.json, stdout, stderr),
                Err(err) => emit_proposal_error(&err, stderr),
            }
        }
        ProposalsCommand::Deny { id, reason } => {
            let proposal_id = match parse_proposal_id(&id, stderr) {
                Ok(pid) => pid,
                Err(code) => return code,
            };
            match service.decide_proposal(
                &tenant,
                &caller,
                proposal_id,
                ApprovalDecision::Deny,
                false,
                reason,
            ) {
                Ok(proposal) => emit_proposal(&proposal, cli.json, stdout, stderr),
                Err(err) => emit_proposal_error(&err, stderr),
            }
        }
    }
}

fn parse_proposal_id(raw: &str, stderr: &mut dyn Write) -> Result<ProposalId, u8> {
    ProposalId::parse(raw.trim()).map_err(|err| {
        let _ = writeln!(stderr, "error: invalid proposal id `{raw}`: {err}");
        EXIT_PROPOSALS_USAGE_ERROR
    })
}

fn emit_proposal(
    proposal: &Proposal,
    as_json: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    if as_json {
        match render_proposal_cisg(proposal).to_canonical_json() {
            Ok(json_str) => {
                let _ = writeln!(stdout, "{json_str}");
                EXIT_PROPOSALS_OK
            }
            Err(err) => {
                let _ = writeln!(stderr, "error: failed to render JSON: {err}");
                EXIT_PROPOSALS_RUNTIME_ERROR
            }
        }
    } else {
        let _ = writeln!(stdout, "id: {}", proposal.id);
        let _ = writeln!(stdout, "tenant: {}", proposal.tenant);
        let _ = writeln!(stdout, "status: {}", proposal.status.as_str());
        let _ = writeln!(stdout, "kind: {}", format_protocol(proposal.kind));
        let _ = writeln!(stdout, "trust: {}", format_trust_tier(proposal.trust));
        let _ = writeln!(stdout, "location: {}", proposal.location);
        if let Some(branch) = &proposal.branch {
            let _ = writeln!(stdout, "branch: {branch}");
        }
        if !proposal.approvals.is_empty() {
            let _ = writeln!(stdout, "approvals: {}", proposal.approvals.join(", "));
        }
        if let Some(shadow) = &proposal.shadow_snapshot_id {
            let _ = writeln!(stdout, "shadow_snapshot_id: {shadow}");
        }
        if let Some(notes) = &proposal.notes {
            let _ = writeln!(stdout, "notes: {notes}");
        }
        EXIT_PROPOSALS_OK
    }
}

fn emit_proposal_error(err: &ProposalError, stderr: &mut dyn Write) -> u8 {
    let _ = writeln!(stderr, "error: {err}");
    match err {
        ProposalError::Validation(_) => EXIT_PROPOSALS_USAGE_ERROR,
        _ => EXIT_PROPOSALS_RUNTIME_ERROR,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Arc;

    use gap_core::clock::SystemClock;
    use gap_core::ids::TenantId;
    use gap_core::snapshot::{Protocol, TrustTier};
    use gap_ingest::proposals::{
        ADMIN_INGEST_SCOPE, AdminCaller, ApprovalPolicyConfig, GateCheckResult, ProposalReport,
        ProposalService, ProposalStatus, ProposeSourceInput,
    };
    use gap_store::sqlite::{SqliteConfig, SqliteContextStore};
    use tempfile::TempDir;

    use super::{
        EXIT_PROPOSALS_OK, EXIT_PROPOSALS_RUNTIME_ERROR, EXIT_PROPOSALS_USAGE_ERROR,
        default_cli_cursor_key, run_proposals_cli,
    };

    fn seed_awaiting_proposal(
        db_path: &Path,
        trust: TrustTier,
        breaking: bool,
    ) -> gap_core::ids::ProposalId {
        let clock = Arc::new(SystemClock);
        let store =
            Arc::new(SqliteContextStore::open(SqliteConfig::new(db_path), clock.clone()).unwrap());
        let svc = ProposalService::with_store_audit(
            store,
            clock,
            ApprovalPolicyConfig::default(),
            default_cli_cursor_key(),
        )
        .unwrap();
        let tenant = TenantId::new("acme").unwrap().scope();
        let admin = AdminCaller::new("seeder", [ADMIN_INGEST_SCOPE]);
        let p = svc
            .propose_source(
                &tenant,
                "seeder",
                ProposeSourceInput {
                    kind: Protocol::Rest,
                    location: format!("https://service-{breaking}.example/spec.json"),
                    branch: None,
                    trust,
                    notes: Some("seeded".to_owned()),
                },
            )
            .unwrap();
        svc.begin_extracting(&tenant, Some(&admin), p.id).unwrap();
        svc.begin_enriching(&tenant, "worker", p.id).unwrap();
        svc.begin_verifying(&tenant, "worker", p.id).unwrap();
        let report = ProposalReport {
            source: "openapi".to_owned(),
            spec_hash: "b".repeat(64),
            tool_versions: BTreeMap::new(),
            model_ids: vec![],
            gate_results: vec![GateCheckResult {
                gate: "G1".to_owned(),
                passed: true,
                evidence: "ok".to_owned(),
            }],
            diffs_summary: None,
            enrichment_degraded: false,
            breaking,
            mutating: false,
            cross_trust_join: false,
            manifest_verified: false,
        };
        let staged = svc
            .stage_verified(&tenant, "worker", p.id, "shadow-cli".to_owned(), report)
            .unwrap();
        assert_eq!(staged.status, ProposalStatus::AwaitingApproval);
        staged.id
    }

    // spec: ING-001, ING-051
    #[test]
    fn proposals_cli_list_show_approve_two_person_and_breaking_ack() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("proposals.sqlite");
        let id = seed_awaiting_proposal(&db_path, TrustTier::T2, true);
        let db_str = db_path.to_str().unwrap();

        // list --json returns seeded proposal
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_proposals_cli(
            [
                "proposals",
                "--db",
                db_str,
                "--tenant",
                "acme",
                "--json",
                "list",
                "--status",
                "AWAITING_APPROVAL",
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(
            code,
            EXIT_PROPOSALS_OK,
            "stderr: {}",
            String::from_utf8_lossy(&err)
        );
        let listed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(listed["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(listed["nodes"][0]["id"], id.to_string());

        // show plain text
        out.clear();
        err.clear();
        let code = run_proposals_cli(
            [
                "proposals",
                "--db",
                db_str,
                "--tenant",
                "acme",
                "show",
                &id.to_string(),
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(code, EXIT_PROPOSALS_OK);
        let shown_txt = String::from_utf8(out.clone()).unwrap();
        assert!(shown_txt.contains("status: AWAITING_APPROVAL"));

        // approve without --breaking-change-ack fails cleanly
        out.clear();
        err.clear();
        let code = run_proposals_cli(
            [
                "proposals",
                "--db",
                db_str,
                "--tenant",
                "acme",
                "--principal",
                "admin-1",
                "approve",
                &id.to_string(),
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(code, EXIT_PROPOSALS_RUNTIME_ERROR);
        assert!(String::from_utf8_lossy(&err).contains("breaking"));

        // First admin approves with --breaking-change-ack => stays AWAITING_APPROVAL
        out.clear();
        err.clear();
        let code = run_proposals_cli(
            [
                "proposals",
                "--db",
                db_str,
                "--tenant",
                "acme",
                "--principal",
                "admin-1",
                "--json",
                "approve",
                &id.to_string(),
                "--breaking-change-ack",
                "--reason",
                "first signoff",
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(code, EXIT_PROPOSALS_OK);
        let after_one: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(after_one["status"], "AWAITING_APPROVAL");

        // Distinct second admin promotes to PROMOTING
        out.clear();
        err.clear();
        let code = run_proposals_cli(
            [
                "proposals",
                "--db",
                db_str,
                "--tenant",
                "acme",
                "--principal",
                "admin-2",
                "--json",
                "approve",
                &id.to_string(),
                "--breaking-change-ack",
                "--reason",
                "second signoff",
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(code, EXIT_PROPOSALS_OK);
        let after_two: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(after_two["status"], "PROMOTING");
    }

    // spec: ING-001, ING-051
    #[test]
    fn proposals_cli_deny_and_usage_errors() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("proposals-deny.sqlite");
        let id = seed_awaiting_proposal(&db_path, TrustTier::T1, false);
        let db_str = db_path.to_str().unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_proposals_cli(
            [
                "proposals",
                "--db",
                db_str,
                "--tenant",
                "acme",
                "--principal",
                "admin-deny",
                "--json",
                "deny",
                &id.to_string(),
                "--reason",
                "policy rejected",
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(
            code,
            EXIT_PROPOSALS_OK,
            "stderr: {}",
            String::from_utf8_lossy(&err)
        );
        let denied: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(denied["status"], "REJECTED");

        // Invalid UUID returns EXIT_PROPOSALS_USAGE_ERROR (2)
        out.clear();
        err.clear();
        let code = run_proposals_cli(
            [
                "proposals",
                "--db",
                db_str,
                "--tenant",
                "acme",
                "show",
                "not-a-uuid",
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(code, EXIT_PROPOSALS_USAGE_ERROR);

        // Missing valid UUIDv7 returns EXIT_PROPOSALS_RUNTIME_ERROR (1)
        let missing = gap_core::ids::ProposalId::new_v7();
        out.clear();
        err.clear();
        let code = run_proposals_cli(
            [
                "proposals",
                "--db",
                db_str,
                "--tenant",
                "acme",
                "show",
                &missing.to_string(),
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(code, EXIT_PROPOSALS_RUNTIME_ERROR);
    }
}

//! Command-line parser and mode dispatch for `gap-conformance` (`VER-011`).

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Parser;
use clap::error::ErrorKind;

use crate::driver::{ConformanceDriver, HttpDriver, PrincipalsMap};
use crate::fixture::{ConformanceClass, LoadedFixture, load_fixtures_dir};
use crate::offline::{OfflineDriver, group_by_schema};
use crate::report::ConformanceReport;
use crate::runner::{RunnerConfig, run_suite};
use crate::spawn::{Deployment, SpawnSpec};

/// Exit code when all executed fixtures passed (`VER-011`).
pub const EXIT_OK: u8 = 0;
/// Exit code when at least one fixture had verdict `fail` or `error` (`VER-011`).
pub const EXIT_CONFORMANCE_FAILURE: u8 = 1;
/// Exit code for CLI usage, configuration, or unsupported-mode errors (`VER-011`).
pub const EXIT_USAGE_OR_UNSUPPORTED: u8 = 2;

/// Black-box GAP protocol conformance runner (`VER-010`, `VER-011`).
#[derive(Debug, Clone, Parser)]
#[command(
    name = "gap-conformance",
    version,
    about = "Black-box GAP protocol conformance suite runner"
)]
pub struct CliArgs {
    /// Live GAP HTTP endpoint URL (used verbatim without appending a path).
    #[arg(long = "target", value_name = "URL")]
    pub target: Option<String>,

    /// Directory searched recursively for `*.json` conformance fixtures.
    #[arg(long = "fixtures", value_name = "DIR", default_value = "conformance")]
    pub fixtures: PathBuf,

    /// Filter fixtures by conformance class (repeatable: `--class Core --class Secure`).
    #[arg(long = "class", value_name = "CLASS")]
    pub classes: Vec<ConformanceClass>,

    /// JSON file mapping fixture `principal` names to bearer tokens (`INV-15`).
    #[arg(long = "principals", value_name = "FILE")]
    pub principals: Option<PathBuf>,

    /// Write deterministic JSON report (with spec-ID coverage) to `<PATH>`.
    #[arg(long = "report", value_name = "PATH")]
    pub report: Option<PathBuf>,

    /// In-process server mode over `<DIR>` (provided in WP-M1-02 / WP-M1-08).
    #[arg(long = "offline", value_name = "DIR")]
    pub offline: Option<PathBuf>,

    /// Spawned example mode (provided in WP-M1-12 / WP-M1-13).
    #[arg(long = "spawn", value_name = "EXAMPLE")]
    pub spawn: Option<String>,

    /// NGC connector conformance kit mode (`NGC-091`): needs `--connector` and `--profile`; `--report` writes the JSON report.
    #[arg(long = "connector-kit")]
    pub connector_kit: bool,

    /// Address of the connector under test for `--connector-kit` (gRPC, plain text: `host:port` or `http://host:port`).
    #[arg(long = "connector", value_name = "ADDR")]
    pub connector: Option<String>,

    /// Kit profile for `--connector-kit` (`conformance/ngc/<name>.kit.toml`, see `conformance/ngc/README.md`).
    #[arg(long = "profile", value_name = "FILE")]
    pub profile: Option<PathBuf>,
}

/// Parses `args` and executes the requested `gap-conformance` CLI mode, writing human/help output
/// to `stdout`/`stderr` and returning the process exit code (`0`, `1`, or `2`).
pub async fn run_cli_from_args<I, T>(args: I, stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match CliArgs::try_parse_from(args) {
        Ok(parsed) => parsed,
        Err(err) => match err.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                let _ = write!(stdout, "{err}");
                return EXIT_OK;
            }
            _ => {
                let _ = write!(stderr, "{err}");
                return EXIT_USAGE_OR_UNSUPPORTED;
            }
        },
    };

    if cli.connector_kit {
        return execute_connector_kit(&cli, stdout, stderr).await;
    }

    if let Some(ref dir) = cli.offline {
        return execute_offline_mode(&cli, dir, stdout, stderr).await;
    }

    if let Some(ref example) = cli.spawn {
        return execute_spawn_mode(&cli, Path::new(example), stdout, stderr).await;
    }

    let Some(ref target_url) = cli.target else {
        let _ = writeln!(
            stderr,
            "error: missing execution mode; pass --target <URL> (or --offline, --spawn, --connector-kit)."
        );
        return EXIT_USAGE_OR_UNSUPPORTED;
    };

    let driver = match HttpDriver::new(target_url) {
        Ok(drv) => drv,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return EXIT_USAGE_OR_UNSUPPORTED;
        }
    };

    execute_target_mode(&driver, &cli, stdout, stderr).await
}

/// Runs the NGC connector kit (`NGC-091`): exit `0` if the connector is NGC-compatible, `1` if a check failed or an advertised
/// capability could not be probed, `2` on a usage or profile error.
async fn execute_connector_kit(
    cli: &CliArgs,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let (Some(addr), Some(profile_path)) = (cli.connector.as_deref(), cli.profile.as_deref())
    else {
        let _ = writeln!(
            stderr,
            "error: --connector-kit needs --connector <ADDR> and --profile <FILE>."
        );
        return EXIT_USAGE_OR_UNSUPPORTED;
    };
    let profile = match crate::ngc_kit::Profile::from_path(profile_path) {
        Ok(profile) => profile,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return EXIT_USAGE_OR_UNSUPPORTED;
        }
    };
    let client = match crate::ngc_kit::connect(addr) {
        Ok(client) => client,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return EXIT_USAGE_OR_UNSUPPORTED;
        }
    };
    let report = crate::ngc_kit::run_kit(&client, &profile).await;
    let _ = write!(stdout, "{}", report.render_text());
    if let Some(path) = &cli.report {
        let json = serde_json::to_string_pretty(&report.to_json()).unwrap_or_default();
        if let Err(err) = std::fs::write(path, format!("{json}\n")) {
            let _ = writeln!(
                stderr,
                "error: cannot write report `{}`: {err}",
                path.display()
            );
            return EXIT_USAGE_OR_UNSUPPORTED;
        }
    }
    if report.compatible() {
        EXIT_OK
    } else {
        EXIT_CONFORMANCE_FAILURE
    }
}

/// Executes a parsed `--target`-style run with an injected [`ConformanceDriver`] (used by both
/// [`run_cli_from_args`] and deterministic unit tests).
pub async fn execute_target_mode(
    driver: &dyn ConformanceDriver,
    cli: &CliArgs,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let principals = if let Some(ref principals_path) = cli.principals {
        match PrincipalsMap::load_file(principals_path) {
            Ok(map) => map,
            Err(err) => {
                let _ = writeln!(stderr, "error: {err}");
                return EXIT_USAGE_OR_UNSUPPORTED;
            }
        }
    } else {
        PrincipalsMap::empty()
    };

    let fixtures = match load_fixtures_dir(&cli.fixtures) {
        Ok(list) => list,
        Err(err) => {
            let _ = writeln!(stderr, "error: {}", principals.scrub_text(&err.to_string()));
            return EXIT_USAGE_OR_UNSUPPORTED;
        }
    };

    let config = RunnerConfig {
        classes: cli.classes.clone(),
        principals,
    };

    let report = run_suite(driver, &fixtures, &config, None).await;

    emit_report(&report, cli, &cli.fixtures, stdout, stderr)
}

/// Writes the optional JSON report and the one-line summary, and maps the report to the exit code.
fn emit_report(
    report: &ConformanceReport,
    cli: &CliArgs,
    fixtures_dir: &Path,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    if let Some(ref report_path) = cli.report {
        let serialized = match report.to_json_pretty() {
            Ok(json) => json,
            Err(err) => {
                let _ = writeln!(stderr, "error: failed to serialize report: {err}");
                return EXIT_USAGE_OR_UNSUPPORTED;
            }
        };
        if let Some(parent) = report_path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            let _ = writeln!(
                stderr,
                "error: failed to create report directory `{}`: {err}",
                parent.display()
            );
            return EXIT_USAGE_OR_UNSUPPORTED;
        }
        if let Err(err) = std::fs::write(report_path, serialized) {
            let _ = writeln!(
                stderr,
                "error: failed to write report `{}`: {err}",
                report_path.display()
            );
            return EXIT_USAGE_OR_UNSUPPORTED;
        }
    }

    if report.summary.total == 0 {
        let class_filter = if cli.classes.is_empty() {
            "all".to_owned()
        } else {
            cli.classes
                .iter()
                .copied()
                .map(ConformanceClass::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let _ = writeln!(
            stderr,
            "error: no fixtures selected (directory: `{}`, class filter: [{class_filter}])",
            fixtures_dir.display(),
        );
        return EXIT_USAGE_OR_UNSUPPORTED;
    }

    let _ = writeln!(
        stdout,
        "gap-conformance: total={} passed={} failed={} errors={} spec_covered={}/{}",
        report.summary.total,
        report.summary.passed,
        report.summary.failed,
        report.summary.errors,
        report.summary.spec_ids_covered,
        report.summary.spec_ids_total,
    );

    if report.all_passed() {
        EXIT_OK
    } else {
        EXIT_CONFORMANCE_FAILURE
    }
}

/// Starts the deployment described by `<example>/spawn.toml`, runs its fixtures over HTTP and stops it again
/// (`--spawn`, see [`crate::spawn`]). The children are terminated whatever the verdict.
async fn execute_spawn_mode(
    cli: &CliArgs,
    example: &Path,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let spec = match SpawnSpec::load(example) {
        Ok(spec) => spec,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return EXIT_USAGE_OR_UNSUPPORTED;
        }
    };
    let fixtures_dir = spec.fixtures_dir(example);
    let fixtures = match load_fixtures_dir(&fixtures_dir) {
        Ok(list) => list,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return EXIT_USAGE_OR_UNSUPPORTED;
        }
    };
    let driver = match HttpDriver::new(&spec.target) {
        Ok(driver) => driver,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return EXIT_USAGE_OR_UNSUPPORTED;
        }
    };
    let mut deployment = match Deployment::start(&spec, example).await {
        Ok(deployment) => deployment,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return EXIT_USAGE_OR_UNSUPPORTED;
        }
    };
    let config = RunnerConfig {
        classes: cli.classes.clone(),
        principals: PrincipalsMap::from_entries(spec.principals.clone()),
    };
    let report = run_suite(&driver, &fixtures, &config, None).await;
    deployment.shutdown().await;
    emit_report(&report, cli, &fixtures_dir, stdout, stderr)
}

/// Runs the fixtures under `dir` against in-process test servers built from their `schema` SDL files
/// (`--offline`, see [`crate::offline`]).
async fn execute_offline_mode(
    cli: &CliArgs,
    dir: &Path,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let fixtures = match load_fixtures_dir(dir) {
        Ok(list) => list,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return EXIT_USAGE_OR_UNSUPPORTED;
        }
    };
    let groups = match group_by_schema(dir, &fixtures) {
        Ok(groups) => groups,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return EXIT_USAGE_OR_UNSUPPORTED;
        }
    };
    let principal_names: BTreeSet<String> = fixtures
        .iter()
        .filter_map(|fixture| fixture.principal.clone())
        .collect();

    let mut results = Vec::new();
    for group in groups.values() {
        let driver = match OfflineDriver::new(&group.name, &group.sdl, &principal_names) {
            Ok(driver) => driver,
            Err(err) => {
                let _ = writeln!(stderr, "error: {err}");
                return EXIT_USAGE_OR_UNSUPPORTED;
            }
        };
        let principals = PrincipalsMap::from_entries(
            driver
                .principal_tokens()
                .iter()
                .map(|(name, token)| (name.clone(), token.expose_secret().clone())),
        );
        let config = RunnerConfig {
            classes: cli.classes.clone(),
            principals,
        };
        let owned: Vec<LoadedFixture> = group.fixtures.iter().map(|f| (*f).clone()).collect();
        results.extend(run_suite(&driver, &owned, &config, None).await.fixtures);
    }
    results.sort_by(|a, b| a.file.cmp(&b.file));
    let report =
        ConformanceReport::from_results(format!("offline:{}", dir.display()), results, None);
    emit_report(&report, cli, dir, stdout, stderr)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use gap_core::secret::SecretString;
    use gap_core::traits::BoxFuture;
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::*;
    use crate::driver::DriverError;
    use crate::fixture::FixtureRequest;
    use crate::matcher::ObservedResponse;

    #[derive(Debug)]
    struct StaticOkDriver;

    impl ConformanceDriver for StaticOkDriver {
        fn target(&self) -> &str {
            "http://127.0.0.1:18080/gap"
        }

        fn execute<'a>(
            &'a self,
            request: &'a FixtureRequest,
            _bearer_token: Option<&'a SecretString>,
        ) -> BoxFuture<'a, Result<ObservedResponse, DriverError>> {
            let is_ok = request.query.contains("ok");
            Box::pin(async move {
                let body = if is_ok {
                    json!({"data": {"ok": true}})
                } else {
                    json!({"data": null, "errors": [{"extensions": {"code": "GAP_UNKNOWN_FIELD"}}]})
                };
                Ok(ObservedResponse::from_parts(
                    200,
                    std::iter::empty::<(&str, &str)>(),
                    body.as_object(),
                ))
            })
        }
    }

    // spec: NGC-091 (VER-011)
    #[tokio::test]
    async fn cli_connector_kit_rejects_missing_arguments_bad_profiles_and_tls_addresses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bad_profile = dir.path().join("bad.kit.toml");
        std::fs::write(&bad_profile, "name = \"x\"\nbogus = 1\n").expect("write profile");
        let bad_profile = bad_profile.to_string_lossy().into_owned();
        let good_profile = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../conformance/ngc/sdk-reference.kit.toml"
        );
        let cases: Vec<Vec<&str>> = vec![
            vec!["gap-conformance", "--connector-kit"],
            vec![
                "gap-conformance",
                "--connector-kit",
                "--connector",
                "127.0.0.1:1",
            ],
            vec![
                "gap-conformance",
                "--connector-kit",
                "--profile",
                good_profile,
            ],
            vec![
                "gap-conformance",
                "--connector-kit",
                "--connector",
                "127.0.0.1:1",
                "--profile",
                "/no/such/profile.kit.toml",
            ],
            vec![
                "gap-conformance",
                "--connector-kit",
                "--connector",
                "127.0.0.1:1",
                "--profile",
                bad_profile.as_str(),
            ],
            vec![
                "gap-conformance",
                "--connector-kit",
                "--connector",
                "https://127.0.0.1:1",
                "--profile",
                good_profile,
            ],
            vec![
                "gap-conformance",
                "--connector-kit",
                "--connector",
                "ftp://127.0.0.1:1",
                "--profile",
                good_profile,
            ],
        ];
        for args in cases {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = run_cli_from_args(args.clone(), &mut out, &mut err).await;
            assert_eq!(code, 2, "args {args:?} must exit 2");
            let msg = String::from_utf8(err).expect("utf8 stderr");
            assert!(msg.starts_with("error: "), "{args:?}: {msg:?}");
        }
    }

    // spec: NGC-091 (VER-011)
    #[tokio::test]
    async fn cli_connector_kit_exits_1_when_nothing_answers_and_writes_the_report() {
        // A port nobody listens on: Describe fails, so the connector is not NGC-compatible.
        let free = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = free.local_addr().expect("addr").to_string();
        drop(free);
        let dir = tempfile::tempdir().expect("tempdir");
        let report = dir.path().join("report.json");
        let args = [
            "gap-conformance",
            "--connector-kit",
            "--connector",
            addr.as_str(),
            "--profile",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../conformance/ngc/sdk-reference.kit.toml"
            ),
            "--report",
            report.to_str().expect("utf8 path"),
        ];
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_cli_from_args(args, &mut out, &mut err).await;
        assert_eq!(code, 1, "stderr: {}", String::from_utf8_lossy(&err));
        assert!(String::from_utf8_lossy(&out).contains("NOT NGC-compatible"));
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&report).expect("report written"))
                .expect("json");
        assert_eq!(json["compatible"], false);
        assert_eq!(json["profile"], "sdk-reference");
        assert_eq!(json["failed"], serde_json::json!(["describe.valid"]));
    }

    // spec: GQL-002, NGC-091 (VER-011)
    #[tokio::test]
    async fn cli_help_exits_0_and_unusable_modes_exit_2() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code_help = run_cli_from_args(["gap-conformance", "--help"], &mut out, &mut err).await;
        assert_eq!(code_help, 0);
        assert!(String::from_utf8_lossy(&out).contains("gap-conformance"));
        assert!(err.is_empty());

        // `--spawn` needs a description: a missing `spawn.toml` is a usage error (exit 2, one line).
        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();
        let code = run_cli_from_args(
            ["gap-conformance", "--spawn", "no/such/example"],
            &mut out_buf,
            &mut err_buf,
        )
        .await;
        assert_eq!(code, 2);
        let msg = String::from_utf8(err_buf).expect("utf8 stderr");
        assert_eq!(msg.lines().count(), 1, "{msg:?}");
        assert!(msg.contains("spawn.toml"), "{msg}");

        // Unknown `--class` value, missing mode, and `https://` target must all exit 2.
        for bad_args in [
            vec!["gap-conformance"],
            vec![
                "gap-conformance",
                "--target",
                "http://127.0.0.1:8080",
                "--class",
                "NotAClass",
            ],
            vec!["gap-conformance", "--target", "https://example.test/gap"],
            vec![
                "gap-conformance",
                "--target",
                "http://127.0.0.1:8080",
                "--offline",
                "conformance",
            ],
        ] {
            let mut out_buf = Vec::new();
            let mut err_buf = Vec::new();
            let code = run_cli_from_args(bad_args.clone(), &mut out_buf, &mut err_buf).await;
            assert_eq!(code, 2, "bad args {bad_args:?} must return exit code 2");
            assert!(!err_buf.is_empty());
        }
    }

    // spec: GQL-003, GQL-070
    #[tokio::test]
    async fn offline_mode_runs_fixtures_against_the_in_process_server() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path();
        fs::write(
            dir.join("unknown_field.json"),
            json!({
                "name": "unknown field is a validation error",
                "class": ["Core"],
                "spec": ["GQL-070"],
                "principal": "agent-readonly",
                "request": {"query": "{ order(id: \"1\") { nope } }"},
                "expect": {"status": 422, "dataNull": true,
                           "errors": [{"extensions": {"code": "GAP_UNKNOWN_FIELD"}}]}
            })
            .to_string(),
        )
        .expect("write fixture");
        fs::write(
            dir.join("wrong_expectation.json"),
            json!({
                "name": "deliberately wrong expectation",
                "class": ["Core"],
                "spec": ["GQL-003"],
                "request": {"query": "{ shopVersion }"},
                "expect": {"status": 418}
            })
            .to_string(),
        )
        .expect("write fixture");

        let offline = dir.to_str().expect("utf8 path");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_cli_from_args(
            ["gap-conformance", "--offline", offline],
            &mut out,
            &mut err,
        )
        .await;
        let stdout = String::from_utf8_lossy(&out);
        assert_eq!(
            code,
            EXIT_CONFORMANCE_FAILURE,
            "stdout: {stdout} stderr: {}",
            String::from_utf8_lossy(&err)
        );
        assert!(stdout.contains("total=2 passed=1 failed=1"), "{stdout}");

        // Only the passing fixture's class/spec selection: exit 0.
        fs::remove_file(dir.join("wrong_expectation.json")).expect("remove fixture");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_cli_from_args(
            ["gap-conformance", "--offline", offline],
            &mut out,
            &mut err,
        )
        .await;
        assert_eq!(code, EXIT_OK, "{}", String::from_utf8_lossy(&err));

        // A directory that does not exist is a usage error.
        let missing = tmp.path().join("missing");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_cli_from_args(
            [
                "gap-conformance",
                "--offline",
                missing.to_str().expect("utf8"),
            ],
            &mut out,
            &mut err,
        )
        .await;
        assert_eq!(code, EXIT_USAGE_OR_UNSUPPORTED);
    }

    // spec: GQL-002, INV-15 (VER-010, VER-011)
    #[tokio::test]
    async fn execute_target_mode_writes_report_and_returns_0_or_1_or_2() {
        let tmp = TempDir::new().expect("tempdir");
        let fixtures_dir = tmp.path().join("fixtures");
        fs::create_dir_all(&fixtures_dir).expect("mkdir fixtures");

        fs::write(
            fixtures_dir.join("01_pass.json"),
            json!({
                "name": "pass fixture",
                "class": ["Core"],
                "spec": ["GQL-002"],
                "principal": "agent-ok",
                "request": {"query": "{ ok }"},
                "expect": {"status": 200, "data": {"ok": true}, "dataNull": false}
            })
            .to_string(),
        )
        .expect("write pass fixture");

        fs::write(
            fixtures_dir.join("02_fail_other_class.json"),
            json!({
                "name": "failing discovery fixture",
                "class": ["Discovery"],
                "spec": ["GQL-070"],
                "request": {"query": "{ bad }"},
                "expect": {"status": 200, "dataNull": false}
            })
            .to_string(),
        )
        .expect("write fail fixture");

        let principals_path = tmp.path().join("principals.json");
        fs::write(
            &principals_path,
            json!({"agent-ok": "tok-cli-secret-111"}).to_string(),
        )
        .expect("write principals");

        let report_path = tmp.path().join("out/report.json");
        let cli_core_only = CliArgs::try_parse_from([
            "gap-conformance",
            "--target",
            "http://127.0.0.1:18080/gap",
            "--fixtures",
            fixtures_dir.to_str().expect("utf8"),
            "--class",
            "Core",
            "--principals",
            principals_path.to_str().expect("utf8"),
            "--report",
            report_path.to_str().expect("utf8"),
        ])
        .expect("parse cli");

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code_pass =
            execute_target_mode(&StaticOkDriver, &cli_core_only, &mut out, &mut err).await;
        assert_eq!(code_pass, 0);
        assert!(err.is_empty());

        let report_json: Value =
            serde_json::from_str(&fs::read_to_string(&report_path).expect("read report"))
                .expect("parse report JSON");
        assert_eq!(report_json["summary"]["passed"], 1);
        assert_eq!(report_json["summary"]["failed"], 0);
        assert_eq!(report_json["coverage"]["GQL-002"]["covered"], true);

        // Running both Core and Discovery includes the failing fixture -> exit code 1.
        let cli_both = CliArgs::try_parse_from([
            "gap-conformance",
            "--target",
            "http://127.0.0.1:18080/gap",
            "--fixtures",
            fixtures_dir.to_str().expect("utf8"),
            "--class",
            "Core",
            "--class",
            "Discovery",
            "--principals",
            principals_path.to_str().expect("utf8"),
        ])
        .expect("parse cli both");
        let code_fail = execute_target_mode(&StaticOkDriver, &cli_both, &mut out, &mut err).await;
        assert_eq!(code_fail, 1);

        // Non-existent fixtures directory -> exit code 2.
        let cli_bad_dir = CliArgs::try_parse_from([
            "gap-conformance",
            "--target",
            "http://127.0.0.1:18080/gap",
            "--fixtures",
            tmp.path().join("does-not-exist").to_str().expect("utf8"),
        ])
        .expect("parse bad dir");
        let code_cfg_err =
            execute_target_mode(&StaticOkDriver, &cli_bad_dir, &mut out, &mut err).await;
        assert_eq!(code_cfg_err, 2);
    }

    // spec: GQL-002 (VER-010, VER-011)
    #[tokio::test]
    async fn execute_target_mode_empty_dir_writes_report_and_exits_2() {
        let tmp = TempDir::new().expect("tempdir");
        let empty_dir = tmp.path().join("empty_fixtures");
        fs::create_dir_all(&empty_dir).expect("mkdir empty_fixtures");
        let report_path = tmp.path().join("empty_report.json");

        let cli = CliArgs::try_parse_from([
            "gap-conformance",
            "--target",
            "http://127.0.0.1:18080/gap",
            "--fixtures",
            empty_dir.to_str().expect("utf8"),
            "--report",
            report_path.to_str().expect("utf8"),
        ])
        .expect("parse cli");

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = execute_target_mode(&StaticOkDriver, &cli, &mut out, &mut err).await;
        assert_eq!(code, 2, "empty fixtures dir must exit 2 (no vacuous pass)");

        let stderr_str = String::from_utf8(err).expect("utf8 stderr");
        assert!(
            stderr_str.contains("error: no fixtures selected"),
            "stderr must contain `error: no fixtures selected`: {stderr_str}"
        );
        assert!(
            stderr_str.contains(&empty_dir.display().to_string()),
            "stderr must name the fixtures directory: {stderr_str}"
        );
        assert!(
            stderr_str.contains("all"),
            "stderr must name the active class filter: {stderr_str}"
        );

        let report_raw =
            fs::read_to_string(&report_path).expect("report file must still be written first");
        let report_json: Value = serde_json::from_str(&report_raw).expect("valid report JSON");
        assert_eq!(report_json["summary"]["total"], 0);
        assert_eq!(report_json["fixtures"], json!([]));
    }

    // spec: GQL-002 (VER-010, VER-011)
    #[tokio::test]
    async fn execute_target_mode_filter_excluding_everything_writes_report_and_exits_2() {
        let tmp = TempDir::new().expect("tempdir");
        let fixtures_dir = tmp.path().join("fixtures");
        fs::create_dir_all(&fixtures_dir).expect("mkdir fixtures");
        fs::write(
            fixtures_dir.join("01_core.json"),
            json!({
                "name": "core fixture",
                "class": ["Core"],
                "spec": ["GQL-002"],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200, "data": {"ok": true}}
            })
            .to_string(),
        )
        .expect("write core fixture");

        let report_path = tmp.path().join("filtered_empty_report.json");
        let cli = CliArgs::try_parse_from([
            "gap-conformance",
            "--target",
            "http://127.0.0.1:18080/gap",
            "--fixtures",
            fixtures_dir.to_str().expect("utf8"),
            "--class",
            "Tasks",
            "--class",
            "NGC",
            "--report",
            report_path.to_str().expect("utf8"),
        ])
        .expect("parse cli");

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = execute_target_mode(&StaticOkDriver, &cli, &mut out, &mut err).await;
        assert_eq!(code, 2, "class filter matching 0 fixtures must exit 2");

        let stderr_str = String::from_utf8(err).expect("utf8 stderr");
        assert!(
            stderr_str.contains("error: no fixtures selected"),
            "stderr must contain `error: no fixtures selected`: {stderr_str}"
        );
        assert!(
            stderr_str.contains(&fixtures_dir.display().to_string()),
            "stderr must name the fixtures directory: {stderr_str}"
        );
        assert!(
            stderr_str.contains("Tasks") && stderr_str.contains("NGC"),
            "stderr must name the class filter: {stderr_str}"
        );

        let report_raw =
            fs::read_to_string(&report_path).expect("report file must still be written first");
        let report_json: Value = serde_json::from_str(&report_raw).expect("valid report JSON");
        assert_eq!(report_json["summary"]["total"], 0);
        assert_eq!(report_json["fixtures"], json!([]));
    }

    // spec: GQL-002 (VER-010, VER-011)
    #[tokio::test]
    async fn execute_target_mode_single_passing_fixture_exits_0() {
        let tmp = TempDir::new().expect("tempdir");
        let fixtures_dir = tmp.path().join("single_ok");
        fs::create_dir_all(&fixtures_dir).expect("mkdir single_ok");
        fs::write(
            fixtures_dir.join("01_ok.json"),
            json!({
                "name": "single passing fixture",
                "class": ["Core"],
                "spec": ["GQL-002"],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200, "data": {"ok": true}}
            })
            .to_string(),
        )
        .expect("write 01_ok.json");

        let report_path = tmp.path().join("single_ok_report.json");
        let cli = CliArgs::try_parse_from([
            "gap-conformance",
            "--target",
            "http://127.0.0.1:18080/gap",
            "--fixtures",
            fixtures_dir.to_str().expect("utf8"),
            "--class",
            "Core",
            "--report",
            report_path.to_str().expect("utf8"),
        ])
        .expect("parse cli");

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = execute_target_mode(&StaticOkDriver, &cli, &mut out, &mut err).await;
        assert_eq!(code, 0, "single passing fixture must exit 0");
        assert!(err.is_empty());

        let report_json: Value =
            serde_json::from_str(&fs::read_to_string(&report_path).expect("read report"))
                .expect("valid report JSON");
        assert_eq!(report_json["summary"]["total"], 1);
        assert_eq!(report_json["summary"]["passed"], 1);
    }

    #[derive(Debug)]
    struct AlwaysTransportErrDriver;

    impl ConformanceDriver for AlwaysTransportErrDriver {
        fn target(&self) -> &str {
            "http://127.0.0.1:18080/gap"
        }

        fn execute<'a>(
            &'a self,
            _request: &'a FixtureRequest,
            _bearer_token: Option<&'a SecretString>,
        ) -> BoxFuture<'a, Result<ObservedResponse, DriverError>> {
            Box::pin(async {
                Err(DriverError::Transport {
                    target: "http://127.0.0.1:18080/gap".to_owned(),
                    reason: "connection reset".to_owned(),
                })
            })
        }
    }

    // spec: GQL-002 (VER-011)
    #[tokio::test]
    async fn b1b_execute_target_mode_with_transport_error_only_exits_1() {
        let tmp = TempDir::new().expect("tempdir");
        let fixtures_dir = tmp.path().join("err_only");
        fs::create_dir_all(&fixtures_dir).expect("mkdir err_only");
        fs::write(
            fixtures_dir.join("01_err.json"),
            json!({
                "name": "transport error fixture",
                "class": ["Core"],
                "spec": ["GQL-002"],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200}
            })
            .to_string(),
        )
        .expect("write 01_err.json");

        let cli = CliArgs::try_parse_from([
            "gap-conformance",
            "--target",
            "http://127.0.0.1:18080/gap",
            "--fixtures",
            fixtures_dir.to_str().expect("utf8"),
        ])
        .expect("parse cli");

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = execute_target_mode(&AlwaysTransportErrDriver, &cli, &mut out, &mut err).await;
        assert_eq!(code, 1);
    }

    // spec: INV-15, SEC-040 (VER-010, VER-011)
    #[tokio::test]
    async fn d5_execute_target_mode_scrubs_principal_token_from_fixture_load_error() {
        let tmp = TempDir::new().expect("tempdir");
        let principals_path = tmp.path().join("principals.json");
        fs::write(
            &principals_path,
            json!({"alice": "tok-load-err-secret-555"}).to_string(),
        )
        .expect("write principals.json");

        let fixtures_dir = tmp.path().join("leaky_fixtures");
        fs::create_dir_all(&fixtures_dir).expect("mkdir leaky_fixtures");
        fs::write(
            fixtures_dir.join("01_bad.json"),
            json!({
                "name": "unknown key equals token",
                "class": ["Core"],
                "spec": ["GQL-002"],
                "request": {"query": "{ ok }"},
                "expect": {"status": 200},
                "tok-load-err-secret-555": true
            })
            .to_string(),
        )
        .expect("write 01_bad.json");

        let cli = CliArgs::try_parse_from([
            "gap-conformance",
            "--target",
            "http://127.0.0.1:18080/gap",
            "--fixtures",
            fixtures_dir.to_str().expect("utf8"),
            "--principals",
            principals_path.to_str().expect("utf8"),
        ])
        .expect("parse cli");

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = execute_target_mode(&StaticOkDriver, &cli, &mut out, &mut err).await;
        assert_eq!(code, 2);

        let stderr_str = String::from_utf8(err).expect("utf8 stderr");
        assert!(
            stderr_str.contains("***"),
            "stderr must contain `***`: {stderr_str}"
        );
        assert!(
            !stderr_str.contains("tok-load-err-secret-555"),
            "stderr must never contain raw principal token: {stderr_str}"
        );
    }

    // spec: GQL-002 (VER-010, VER-011)
    #[test]
    fn h3_cli_defaults_fixtures_to_conformance_when_flag_omitted() {
        let cli = CliArgs::try_parse_from([
            "gap-conformance",
            "--target",
            "http://127.0.0.1:8080/graphql",
        ])
        .expect("CLI parse without --fixtures must succeed");
        assert_eq!(cli.fixtures, PathBuf::from("conformance"));
        assert_eq!(cli.fixtures.to_str(), Some("conformance"));

        let bare = CliArgs::try_parse_from(["gap-conformance"])
            .expect("bare CLI parse with default flags must succeed");
        assert_eq!(bare.fixtures, PathBuf::from("conformance"));
    }
}

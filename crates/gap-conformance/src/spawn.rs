//! `--spawn <EXAMPLE_DIR>`: start an example deployment, run the Core fixtures against it over real HTTP, stop it
//! (`VER-011`, `WP-M1-13`).
//!
//! `<EXAMPLE_DIR>/spawn.toml` describes the deployment:
//!
//! ```toml
//! target = "http://127.0.0.1:8080/graphql"      # GraphQL endpoint the fixtures are sent to
//! ready_timeout_ms = 30000                       # whole-deployment readiness budget (default 30 s)
//! fixtures = "conformance"                       # fixtures directory, relative to the example (default)
//!
//! [principals]                                   # fixture principal name -> bearer token (dev tokens only)
//! dev-agent = "gap_dev_skeleton_key_01"
//!
//! [[process]]                                    # started in order, stopped in reverse order
//! name = "rest"
//! command = ["python3", "fixtures/rest_fixture.py", "--port", "9101"]   # run with the example as working directory
//! ready_stdout = "listening on"                  # ready when this text appears on stdout or stderr ...
//! # ready_http = "http://127.0.0.1:8080/readyz"  # ... or when this URL answers 200
//! ```
//!
//! `command[0]` may be `${GAP_BIN}`: the `gap` binary, taken from the `GAP_BIN` environment variable or found
//! at `target/{release,debug}/gap` of the enclosing workspace. Children are started without a shell, inherit no
//! secrets beyond the process environment of the runner, and are always terminated (`SIGTERM`, then `SIGKILL` after
//! a grace period) even when a step fails.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, timeout};

/// How long a child gets between `SIGTERM` and `SIGKILL`.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// Failure to describe, start or stop a spawned deployment.
#[derive(Debug, Error)]
pub enum SpawnError {
    /// `spawn.toml` is missing or unreadable.
    #[error("cannot read `{path}`: {source}")]
    Read {
        /// File path.
        path: String,
        /// I/O error.
        #[source]
        source: std::io::Error,
    },
    /// `spawn.toml` is not valid for this runner.
    #[error("invalid spawn description `{path}`: {reason}")]
    Invalid {
        /// File path.
        path: String,
        /// What is wrong.
        reason: String,
    },
    /// The `gap` binary could not be found for `${GAP_BIN}`.
    #[error(
        "`${{GAP_BIN}}` is used but no gap binary was found: set GAP_BIN or run `cargo build -p gap-cli` first"
    )]
    GapBinaryMissing,
    /// A process could not be started.
    #[error("cannot start process `{name}`: {source}")]
    Start {
        /// Process name from `spawn.toml`.
        name: String,
        /// OS error.
        #[source]
        source: std::io::Error,
    },
    /// A process exited or stayed silent before it was ready.
    #[error("process `{name}` was not ready: {reason}")]
    NotReady {
        /// Process name from `spawn.toml`.
        name: String,
        /// Why.
        reason: String,
    },
}

/// One `[[process]]` of `spawn.toml`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProcessSpec {
    /// Short unique name (used in diagnostics).
    pub name: String,
    /// Program and arguments, no shell.
    pub command: Vec<String>,
    /// Ready when this text shows up on the child's stdout or stderr.
    #[serde(default)]
    pub ready_stdout: Option<String>,
    /// Ready when this URL answers `200`.
    #[serde(default)]
    pub ready_http: Option<String>,
}

/// `spawn.toml`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpawnSpec {
    /// GraphQL endpoint the fixtures are sent to.
    pub target: String,
    /// Whole-deployment readiness budget in milliseconds (default 30,000).
    #[serde(default = "default_ready_timeout_ms")]
    pub ready_timeout_ms: u64,
    /// Fixtures directory relative to the example (default `conformance`).
    #[serde(default = "default_fixtures")]
    pub fixtures: String,
    /// Fixture principal name -> bearer token.
    #[serde(default)]
    pub principals: std::collections::BTreeMap<String, String>,
    /// Processes, started in order.
    #[serde(default, rename = "process")]
    pub processes: Vec<ProcessSpec>,
}

fn default_ready_timeout_ms() -> u64 {
    30_000
}

fn default_fixtures() -> String {
    "conformance".to_owned()
}

impl SpawnSpec {
    /// Reads and validates `<dir>/spawn.toml`.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError`] if the file is unreadable, not valid TOML for this schema, or inconsistent (no
    /// process, duplicate names, a process without any readiness condition, an empty command).
    pub fn load(dir: &Path) -> Result<Self, SpawnError> {
        let path = dir.join("spawn.toml");
        let shown = path.display().to_string();
        let text = std::fs::read_to_string(&path).map_err(|source| SpawnError::Read {
            path: shown.clone(),
            source,
        })?;
        let spec: Self = toml::from_str(&text).map_err(|e| SpawnError::Invalid {
            path: shown.clone(),
            reason: e.message().to_owned(),
        })?;
        let invalid = |reason: String| SpawnError::Invalid {
            path: shown.clone(),
            reason,
        };
        if spec.processes.is_empty() {
            return Err(invalid("at least one [[process]] is required".to_owned()));
        }
        if spec.ready_timeout_ms == 0 {
            return Err(invalid("ready_timeout_ms must be positive".to_owned()));
        }
        if Path::new(&spec.fixtures).is_absolute() || spec.fixtures.split('/').any(|s| s == "..") {
            return Err(invalid(
                "fixtures must be a relative path without `..`".to_owned(),
            ));
        }
        let mut names = std::collections::BTreeSet::new();
        for p in &spec.processes {
            if p.name.is_empty() || !names.insert(p.name.as_str()) {
                return Err(invalid(format!(
                    "process name `{}` is empty or duplicated",
                    p.name
                )));
            }
            if p.command.is_empty() || p.command[0].is_empty() {
                return Err(invalid(format!(
                    "process `{}` has an empty command",
                    p.name
                )));
            }
            if p.ready_stdout.is_none() && p.ready_http.is_none() {
                return Err(invalid(format!(
                    "process `{}` needs ready_stdout or ready_http",
                    p.name
                )));
            }
        }
        Ok(spec)
    }

    /// Fixtures directory of this example.
    #[must_use]
    pub fn fixtures_dir(&self, example_dir: &Path) -> PathBuf {
        example_dir.join(&self.fixtures)
    }
}

/// A running deployment; every child is terminated when it is dropped or [`Self::shutdown`] is called.
#[derive(Debug)]
pub struct Deployment {
    children: Vec<(String, Child)>,
}

/// Finds the `gap` binary for `${GAP_BIN}`.
fn find_gap_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GAP_BIN") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    ["target/release/gap", "target/debug/gap"]
        .iter()
        .map(|rel| root.join(rel))
        .find(|p| p.is_file())
}

fn resolve_program(program: &str) -> Result<String, SpawnError> {
    if program == "${GAP_BIN}" {
        return find_gap_binary()
            .map(|p| p.display().to_string())
            .ok_or(SpawnError::GapBinaryMissing);
    }
    Ok(program.to_owned())
}

async fn http_ready(client: &reqwest::Client, url: &str) -> bool {
    client
        .get(url)
        .send()
        .await
        .is_ok_and(|r| r.status().as_u16() == 200)
}

impl Deployment {
    /// Starts every process of `spec` in order (working directory `dir`) and waits until all are ready.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError`] if a process cannot be started, exits before it is ready, or the readiness budget
    /// runs out; already started children are terminated first.
    pub async fn start(spec: &SpawnSpec, dir: &Path) -> Result<Self, SpawnError> {
        let mut deployment = Self {
            children: Vec::new(),
        };
        let budget = Duration::from_millis(spec.ready_timeout_ms);
        let deadline = Instant::now() + budget;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| SpawnError::Invalid {
                path: "spawn.toml".to_owned(),
                reason: format!("cannot build the readiness client: {e}"),
            })?;

        for process in &spec.processes {
            let program = resolve_program(&process.command[0])?;
            let mut command = Command::new(program);
            command
                .args(&process.command[1..])
                .current_dir(dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let mut child = command.spawn().map_err(|source| SpawnError::Start {
                name: process.name.clone(),
                source,
            })?;
            let (tx, mut rx) = mpsc::unbounded_channel::<String>();
            for stream in [
                child.stdout.take().map(|s| {
                    Box::pin(BufReader::new(s))
                        as std::pin::Pin<Box<dyn tokio::io::AsyncBufRead + Send>>
                }),
                child.stderr.take().map(|s| {
                    Box::pin(BufReader::new(s))
                        as std::pin::Pin<Box<dyn tokio::io::AsyncBufRead + Send>>
                }),
            ]
            .into_iter()
            .flatten()
            {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut lines = stream.lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(tx);
            deployment.children.push((process.name.clone(), child));

            let not_ready = |reason: String| SpawnError::NotReady {
                name: process.name.clone(),
                reason,
            };
            let mut seen = Vec::new();
            loop {
                if let Some(needle) = &process.ready_stdout
                    && seen.iter().any(|l: &String| l.contains(needle.as_str()))
                {
                    break;
                }
                if let Some(url) = &process.ready_http
                    && http_ready(&client, url).await
                {
                    break;
                }
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    deployment.shutdown().await;
                    return Err(not_ready(format!(
                        "not ready within {} ms; output so far: {}",
                        spec.ready_timeout_ms,
                        tail(&seen)
                    )));
                };
                if let Some((_, child)) = deployment.children.last_mut()
                    && let Ok(Some(status)) = child.try_wait()
                {
                    let reason = format!(
                        "exited with {status} before it was ready; output: {}",
                        tail(&seen)
                    );
                    deployment.shutdown().await;
                    return Err(not_ready(reason));
                }
                match timeout(remaining.min(Duration::from_millis(100)), rx.recv()).await {
                    Ok(Some(line)) => seen.push(line),
                    Ok(None) => sleep(Duration::from_millis(50)).await,
                    Err(_) => {}
                }
            }
        }
        Ok(deployment)
    }

    /// Terminates every child, last started first: `SIGTERM`, then `SIGKILL` after a grace period.
    pub async fn shutdown(&mut self) {
        while let Some((_name, mut child)) = self.children.pop() {
            terminate(&mut child).await;
        }
    }
}

fn tail(lines: &[String]) -> String {
    let joined = lines
        .iter()
        .rev()
        .take(5)
        .rev()
        .cloned()
        .collect::<Vec<_>>()
        .join(" | ");
    joined.chars().take(400).collect()
}

#[cfg(unix)]
async fn terminate(child: &mut Child) {
    if let Some(pid) = child.id().and_then(|p| i32::try_from(p).ok()) {
        // SAFETY-free: `kill(2)` through the `kill` utility keeps this crate `forbid(unsafe_code)`.
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    if timeout(TERM_GRACE, child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
}

#[cfg(not(unix))]
async fn terminate(child: &mut Child) {
    let _ = child.kill().await;
}

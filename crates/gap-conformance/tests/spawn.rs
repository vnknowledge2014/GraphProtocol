//! `--spawn`: start a deployment from `spawn.toml`, run fixtures against it over real HTTP, stop it (`VER-011`).
//! Real child processes (`python3`, standard library only) and loopback sockets: run by the reviewer.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

use gap_conformance::cli::{
    EXIT_CONFORMANCE_FAILURE, EXIT_OK, EXIT_USAGE_OR_UNSUPPORTED, run_cli_from_args,
};
use gap_conformance::spawn::{SpawnError, SpawnSpec};
use tempfile::TempDir;

const SERVER_PY: &str = r#"
import http.server, json, os, socketserver, sys
port = int(sys.argv[1])
open("server.pid", "w").write(str(os.getpid()))
class S(http.server.ThreadingHTTPServer):
    allow_reuse_address = True
    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = str(self.server_address[0]), self.server_address[1]
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        self.send_response(200); self.send_header("Content-Length", "2"); self.end_headers(); self.wfile.write(b"ok")
    def do_POST(self):
        n = int(self.headers.get("content-length", "0")); json.loads(self.rfile.read(n) or b"{}")
        authorized = self.headers.get("authorization") == "Bearer tok-agent"
        body = json.dumps({"data": {"ok": True}, "extensions": {"gap": {"generation": 1}}} if authorized else {"errors": [{"message": "no", "extensions": {"code": "GAP_UNAUTHENTICATED"}}]}).encode()
        self.send_response(200 if authorized else 401)
        self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
srv = S(("127.0.0.1", port), H)
print("server listening on", port, flush=True)
srv.serve_forever()
"#;

const SLEEPER_PY: &str = r#"
import os, sys, time
open("sleeper.pid", "w").write(str(os.getpid()))
print("sleeper ready", flush=True)
time.sleep(300)
"#;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn alive(pid_file: &Path) -> bool {
    let Ok(text) = fs::read_to_string(pid_file) else {
        return false;
    };
    Command::new("kill")
        .args(["-0", text.trim()])
        .status()
        .is_ok_and(|s| s.success())
}

fn example(spawn_toml: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("server.py"), SERVER_PY).unwrap();
    fs::write(dir.path().join("sleeper.py"), SLEEPER_PY).unwrap();
    fs::write(dir.path().join("spawn.toml"), spawn_toml).unwrap();
    fs::create_dir_all(dir.path().join("conformance")).unwrap();
    fs::write(
        dir.path().join("conformance/01_authorized.json"),
        r#"{"name":"authorized agent gets data","class":["Core"],"spec":["GQL-003"],"principal":"agent",
            "request":{"query":"{ ok }"},
            "expect":{"status":200,"data":{"ok":true},"extensions":{"gap":{"generation":1}}}}"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("conformance/02_anonymous.json"),
        r#"{"name":"anonymous is refused","class":["Core"],"spec":["SEC-010"],
            "request":{"query":"{ ok }"},
            "expect":{"status":401,"dataNull":true,"errors":[{"extensions":{"code":"GAP_UNAUTHENTICATED"}}]}}"#,
    )
    .unwrap();
    dir
}

async fn run(dir: &Path, extra: &[&str]) -> (u8, String, String) {
    let mut args = vec![
        "gap-conformance".to_owned(),
        "--spawn".to_owned(),
        dir.display().to_string(),
    ];
    args.extend(extra.iter().map(|s| (*s).to_owned()));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_cli_from_args(args, &mut out, &mut err).await;
    (
        code,
        String::from_utf8_lossy(&out).into_owned(),
        String::from_utf8_lossy(&err).into_owned(),
    )
}

fn good_spec(port: u16) -> String {
    format!(
        r#"target = "http://127.0.0.1:{port}/graphql"
ready_timeout_ms = 20000
[principals]
agent = "tok-agent"
[[process]]
name = "sleeper"
command = ["python3", "sleeper.py"]
ready_stdout = "sleeper ready"
[[process]]
name = "server"
command = ["python3", "server.py", "{port}"]
ready_http = "http://127.0.0.1:{port}/readyz"
"#
    )
}

// spec: SEC-010
#[tokio::test]
async fn conformance_spawn_runs_the_fixtures_against_the_started_deployment_and_stops_it() {
    let port = free_port();
    let dir = example(&good_spec(port));
    let (code, out, err) = run(dir.path(), &["--class", "Core"]).await;
    assert_eq!(code, EXIT_OK, "stdout: {out} stderr: {err}");
    assert!(out.contains("total=2 passed=2 failed=0"), "{out}");
    // Every child is gone after the run, however it went.
    assert!(
        !alive(&dir.path().join("sleeper.pid")),
        "sleeper still running"
    );
    assert!(
        !alive(&dir.path().join("server.pid")),
        "server still running"
    );
}

// spec: GQL-003
#[tokio::test]
async fn conformance_spawn_reports_failing_fixtures_with_exit_1_and_still_stops_the_children() {
    let port = free_port();
    let dir = example(&good_spec(port));
    fs::write(
        dir.path().join("conformance/03_wrong.json"),
        r#"{"name":"wrong expectation","class":["Core"],"spec":["GQL-003"],"principal":"agent",
            "request":{"query":"{ ok }"},"expect":{"status":418}}"#,
    )
    .unwrap();
    let (code, out, _) = run(dir.path(), &[]).await;
    assert_eq!(code, EXIT_CONFORMANCE_FAILURE, "{out}");
    assert!(out.contains("failed=1"), "{out}");
    assert!(!alive(&dir.path().join("sleeper.pid")));
    assert!(!alive(&dir.path().join("server.pid")));
}

// spec: GQL-003
#[tokio::test]
async fn conformance_spawn_a_process_that_exits_before_ready_is_a_usage_error_and_earlier_children_are_stopped()
 {
    let dir = example(
        r#"target = "http://127.0.0.1:9/graphql"
ready_timeout_ms = 20000
[[process]]
name = "sleeper"
command = ["python3", "sleeper.py"]
ready_stdout = "sleeper ready"
[[process]]
name = "dies"
command = ["python3", "-c", "import sys; print('boom', flush=True); sys.exit(3)"]
ready_stdout = "never printed"
"#,
    );
    let (code, _, err) = run(dir.path(), &[]).await;
    assert_eq!(code, EXIT_USAGE_OR_UNSUPPORTED);
    assert!(
        err.contains("`dies`") && err.contains("exited") && err.contains("boom"),
        "{err}"
    );
    assert!(
        !alive(&dir.path().join("sleeper.pid")),
        "the already started sleeper must be stopped"
    );
}

// spec: GQL-003
#[tokio::test]
async fn conformance_spawn_readiness_timeout_is_a_usage_error_and_children_are_stopped() {
    let dir = example(
        r#"target = "http://127.0.0.1:9/graphql"
ready_timeout_ms = 1500
[[process]]
name = "sleeper"
command = ["python3", "sleeper.py"]
ready_stdout = "text that never appears"
"#,
    );
    let started = std::time::Instant::now();
    let (code, _, err) = run(dir.path(), &[]).await;
    assert_eq!(code, EXIT_USAGE_OR_UNSUPPORTED);
    assert!(err.contains("not ready within 1500 ms"), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(15));
    assert!(!alive(&dir.path().join("sleeper.pid")));
}

// spec: GQL-003
#[test]
fn conformance_spawn_description_is_validated() {
    let bad = |toml: &str| {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("spawn.toml"), toml).unwrap();
        SpawnSpec::load(dir.path()).expect_err(toml)
    };
    let ok_process = "[[process]]\nname = \"a\"\ncommand = [\"x\"]\nready_stdout = \"r\"\n";
    for (toml, needle) in [
        ("target = \"http://h/graphql\"\n".to_owned(), "at least one"),
        (format!("target = \"http://h/graphql\"\n{ok_process}{ok_process}"), "duplicated"),
        ("target = \"http://h/graphql\"\n[[process]]\nname = \"a\"\ncommand = [\"x\"]\n".to_owned(), "ready_stdout or ready_http"),
        ("target = \"http://h/graphql\"\n[[process]]\nname = \"a\"\ncommand = []\nready_stdout = \"r\"\n".to_owned(), "empty command"),
        (format!("target = \"http://h/graphql\"\nfixtures = \"../x\"\n{ok_process}"), "without `..`"),
        (format!("target = \"http://h/graphql\"\nready_timeout_ms = 0\n{ok_process}"), "positive"),
        (format!("target = \"http://h/graphql\"\nsurprise = 1\n{ok_process}"), "surprise"),
    ] {
        let err = bad(&toml).to_string();
        assert!(err.contains(needle), "{needle}: {err}");
    }
    assert!(matches!(
        SpawnSpec::load(Path::new("/definitely/not/here")),
        Err(SpawnError::Read { .. })
    ));
}

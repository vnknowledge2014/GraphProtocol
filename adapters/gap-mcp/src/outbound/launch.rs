//! Launch policy for `stdio` MCP servers (`CMP-010`, `ADR-013`, `SEC-062`).
//!
//! A child process is started **only** through an allow-list entry (absolute path + sha256 of the file); the sandbox arrives with `WP-M6-09`,
//! so non-dev profiles use [`LaunchPolicy::Refuse`]. No shell, no `$PATH` search, a cleared environment, kill on drop.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use sha2::{Digest, Sha256};
use tokio::io::BufReader;
use tokio::process::{Child, Command};

use super::transport::stdio::StdioTransport;

/// One launchable server binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedBinary {
    /// Absolute path of the executable.
    pub path: PathBuf,
    /// Lower-case hex sha256 of the file.
    pub sha256: String,
    /// Fixed arguments.
    pub args: Vec<String>,
    /// The only environment variables the child receives.
    pub env: Vec<(String, String)>,
}

/// Who may start a child process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LaunchPolicy {
    /// Never start anything (default; the only policy of non-dev profiles until `WP-M6-09`).
    #[default]
    Refuse,
    /// Dev profiles: only the listed binaries.
    DevAllowList(Vec<AllowedBinary>),
}

/// Why a launch was refused. Variants carry no paths or arguments (`INV-15`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    /// The policy is [`LaunchPolicy::Refuse`].
    #[error("launching MCP servers is not permitted by the launch policy")]
    Refused,
    /// The binary is not on the allow-list.
    #[error("binary is not on the launch allow-list")]
    NotAllowed,
    /// The allow-list entry or the request uses a relative path.
    #[error("launch path must be absolute")]
    RelativePath,
    /// The file does not exist or cannot be read.
    #[error("launch binary is missing or unreadable")]
    Missing,
    /// The path is a symlink, a directory or not a regular file.
    #[error("launch binary is not a regular file")]
    NotRegularFile,
    /// The file's sha256 differs from the allow-list entry.
    #[error("launch binary does not match its pinned sha256")]
    HashMismatch,
    /// Spawning failed.
    #[error("could not start the MCP server process")]
    Spawn,
}

/// Pure description of the command that would be run (inspectable without spawning).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Program (absolute).
    pub program: PathBuf,
    /// Arguments.
    pub args: Vec<String>,
    /// The complete child environment.
    pub env: Vec<(String, String)>,
}

impl LaunchPolicy {
    /// Looks `requested` up in the allow-list (exact, absolute path match).
    pub fn authorize(&self, requested: &Path) -> Result<&AllowedBinary, LaunchError> {
        let Self::DevAllowList(entries) = self else {
            return Err(LaunchError::Refused);
        };
        if !requested.is_absolute() {
            return Err(LaunchError::RelativePath);
        }
        entries
            .iter()
            .find(|e| e.path == requested)
            .ok_or(LaunchError::NotAllowed)
    }
}

/// Compares two hex digests without early exit.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0_u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Checks the file behind `entry` before it is executed: absolute, a regular non-symlink file, sha256 equal.
pub fn verify_binary(entry: &AllowedBinary) -> Result<(), LaunchError> {
    if !entry.path.is_absolute() {
        return Err(LaunchError::RelativePath);
    }
    let meta = std::fs::symlink_metadata(&entry.path).map_err(|_| LaunchError::Missing)?;
    if !meta.file_type().is_file() {
        return Err(LaunchError::NotRegularFile);
    }
    let bytes = std::fs::read(&entry.path).map_err(|_| LaunchError::Missing)?;
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if constant_time_eq(&digest, &entry.sha256.to_ascii_lowercase()) {
        Ok(())
    } else {
        Err(LaunchError::HashMismatch)
    }
}

/// The command for `entry` (no shell, explicit environment only).
#[must_use]
pub fn command_spec(entry: &AllowedBinary) -> CommandSpec {
    CommandSpec {
        program: entry.path.clone(),
        args: entry.args.clone(),
        env: entry.env.clone(),
    }
}

/// Builds the tokio command from a spec: cleared environment, piped stdio, killed on drop.
#[must_use]
pub fn build_command(spec: &CommandSpec) -> Command {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// Verifies and starts the allow-listed server at `path` and wires its pipes to a [`StdioTransport`].
///
/// Stderr is drained and dropped. The returned [`Child`] must be kept alive as long as the transport is used.
pub fn spawn_stdio(
    policy: &LaunchPolicy,
    path: &Path,
    max_frame_bytes: usize,
) -> Result<(StdioTransport, Child), LaunchError> {
    let entry = policy.authorize(path)?;
    verify_binary(entry)?;
    let mut child = build_command(&command_spec(entry))
        .spawn()
        .map_err(|_| LaunchError::Spawn)?;
    let stdin = child.stdin.take().ok_or(LaunchError::Spawn)?;
    let stdout = child.stdout.take().ok_or(LaunchError::Spawn)?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut sink = tokio::io::sink();
            let mut stderr = stderr;
            let _ = tokio::io::copy(&mut stderr, &mut sink).await;
        });
    }
    Ok((
        StdioTransport::new(BufReader::new(stdout), stdin, max_frame_bytes),
        child,
    ))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn temp_binary(contents: &[u8]) -> (tempfile_like::Dir, PathBuf) {
        let dir = tempfile_like::Dir::new();
        let path = dir.path().join("server.sh");
        std::fs::File::create(&path)
            .expect("create")
            .write_all(contents)
            .expect("write");
        (dir, path)
    }

    fn sha(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// A tiny self-cleaning temp dir (no extra dependency).
    mod tempfile_like {
        use std::path::{Path, PathBuf};

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new() -> Self {
                let unique = format!(
                    "gap-mcp-launch-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                );
                let path = std::env::temp_dir().join(unique.replace(['(', ')', ' '], "_"));
                let _ = std::fs::create_dir_all(&path);
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    // spec: CMP-010
    #[test]
    fn outbound_launch_refuse_policy_and_unlisted_or_relative_paths_never_authorize() {
        assert_eq!(
            LaunchPolicy::Refuse.authorize(Path::new("/bin/echo")),
            Err(LaunchError::Refused)
        );
        let policy = LaunchPolicy::DevAllowList(vec![AllowedBinary {
            path: PathBuf::from("/opt/mcp/server"),
            sha256: String::new(),
            args: vec![],
            env: vec![],
        }]);
        assert!(policy.authorize(Path::new("/opt/mcp/server")).is_ok());
        assert_eq!(
            policy.authorize(Path::new("/opt/mcp/other")),
            Err(LaunchError::NotAllowed)
        );
        assert_eq!(
            policy.authorize(Path::new("server")),
            Err(LaunchError::RelativePath)
        );
        assert_eq!(LaunchPolicy::default(), LaunchPolicy::Refuse);
    }

    // spec: CMP-010
    #[test]
    fn outbound_launch_verifies_the_sha256_before_anything_runs() {
        let (_dir, path) = temp_binary(b"#!/bin/sh\necho hi\n");
        let good = AllowedBinary {
            path: path.clone(),
            sha256: sha(b"#!/bin/sh\necho hi\n"),
            args: vec![],
            env: vec![],
        };
        assert_eq!(verify_binary(&good), Ok(()));
        let upper = AllowedBinary {
            sha256: good.sha256.to_ascii_uppercase(),
            ..good.clone()
        };
        assert_eq!(verify_binary(&upper), Ok(()));
        let bad = AllowedBinary {
            sha256: sha(b"something else"),
            ..good.clone()
        };
        assert_eq!(verify_binary(&bad), Err(LaunchError::HashMismatch));
        let empty = AllowedBinary {
            sha256: String::new(),
            ..good.clone()
        };
        assert_eq!(verify_binary(&empty), Err(LaunchError::HashMismatch));
        let missing = AllowedBinary {
            path: path.with_file_name("nope"),
            ..good.clone()
        };
        assert_eq!(verify_binary(&missing), Err(LaunchError::Missing));
        let relative = AllowedBinary {
            path: PathBuf::from("server.sh"),
            ..good
        };
        assert_eq!(verify_binary(&relative), Err(LaunchError::RelativePath));
    }

    // spec: CMP-010
    #[cfg(unix)]
    #[test]
    fn outbound_launch_refuses_symlinks_and_directories() {
        let (dir, path) = temp_binary(b"payload");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).expect("symlink");
        let entry = AllowedBinary {
            path: link,
            sha256: sha(b"payload"),
            args: vec![],
            env: vec![],
        };
        assert_eq!(verify_binary(&entry), Err(LaunchError::NotRegularFile));
        let as_dir = AllowedBinary {
            path: dir.path().to_path_buf(),
            sha256: sha(b"payload"),
            args: vec![],
            env: vec![],
        };
        assert_eq!(verify_binary(&as_dir), Err(LaunchError::NotRegularFile));
    }

    // spec: CMP-010, INV-15
    #[test]
    fn outbound_launch_command_has_no_shell_a_cleared_environment_and_only_listed_variables() {
        let entry = AllowedBinary {
            path: PathBuf::from("/opt/mcp/server"),
            sha256: "00".to_owned(),
            args: vec!["--stdio".to_owned()],
            env: vec![("MCP_MODE".to_owned(), "dev".to_owned())],
        };
        let spec = command_spec(&entry);
        assert_eq!(spec.program, PathBuf::from("/opt/mcp/server"));
        assert_eq!(spec.args, ["--stdio"]);
        assert_eq!(spec.env, [("MCP_MODE".to_owned(), "dev".to_owned())]);
        let command = build_command(&spec);
        let std_cmd = command.as_std();
        assert_eq!(std_cmd.get_program(), "/opt/mcp/server");
        let envs: Vec<_> = std_cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(envs, [("MCP_MODE".to_owned(), Some("dev".to_owned()))]);
        // the error text carries neither the path nor the arguments
        for err in [
            LaunchError::Refused,
            LaunchError::NotAllowed,
            LaunchError::HashMismatch,
            LaunchError::Spawn,
        ] {
            let text = err.to_string();
            assert!(!text.contains("/opt") && !text.contains("--stdio"));
        }
    }

    // spec: CMP-010
    #[tokio::test]
    async fn outbound_spawn_stdio_is_refused_before_any_exec_under_the_default_policy() {
        let result = spawn_stdio(&LaunchPolicy::Refuse, Path::new("/bin/sh"), 1024);
        assert!(matches!(result, Err(LaunchError::Refused)));
    }
}

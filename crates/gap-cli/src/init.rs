//! `gap init [DIR] [--force]` project scaffolding (`OPS-044`, `Brief §3.4`).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use gap_server::{EXIT_CONFIG_ERROR, EXIT_OK};
use thiserror::Error;

const TEMPLATE_GAP_TOML: &str = include_str!("../../../examples/walking-skeleton/gap.toml");
const TEMPLATE_OPENAPI_JSON: &str =
    include_str!("../../../examples/walking-skeleton/shop.openapi.json");
const TEMPLATE_README_MD: &str = include_str!("../../../examples/walking-skeleton/README.md");
const TEMPLATE_REST_FIXTURE_PY: &str =
    include_str!("../../../examples/walking-skeleton/fixtures/rest_fixture.py");
const TEMPLATE_MCP_FIXTURE_PY: &str =
    include_str!("../../../examples/walking-skeleton/fixtures/mcp_fixture.py");

struct TemplateFile {
    rel_path: &'static str,
    content: &'static str,
    executable: bool,
}

const TEMPLATE_FILES: &[TemplateFile] = &[
    TemplateFile {
        rel_path: "gap.toml",
        content: TEMPLATE_GAP_TOML,
        executable: false,
    },
    TemplateFile {
        rel_path: "shop.openapi.json",
        content: TEMPLATE_OPENAPI_JSON,
        executable: false,
    },
    TemplateFile {
        rel_path: "README.md",
        content: TEMPLATE_README_MD,
        executable: false,
    },
    TemplateFile {
        rel_path: "fixtures/rest_fixture.py",
        content: TEMPLATE_REST_FIXTURE_PY,
        executable: true,
    },
    TemplateFile {
        rel_path: "fixtures/mcp_fixture.py",
        content: TEMPLATE_MCP_FIXTURE_PY,
        executable: true,
    },
];

/// Error returned while scaffolding a project with `gap init`.
#[derive(Debug, Error)]
pub enum InitError {
    /// Target directory or an interior path is a symbolic link (`Brief §3.4`).
    #[error("refusing to follow symlink `{path}` during `gap init`")]
    SymlinkRefused {
        /// Path that was detected as a symlink.
        path: String,
    },
    /// Path escaped the canonical target directory.
    #[error("refusing to write outside target directory `{root}`: `{path}`")]
    PathEscape {
        /// Canonical target directory.
        root: String,
        /// Escaping path.
        path: String,
    },
    /// File already exists and `--force` was not passed.
    #[error("file `{path}` already exists (pass `--force` to overwrite)")]
    AlreadyExists {
        /// Existing destination file path.
        path: String,
    },
    /// Existing entry has the wrong filesystem type (e.g. directory where regular file expected).
    #[error("invalid filesystem entry at `{path}`: {reason}")]
    InvalidEntry {
        /// Offending path.
        path: String,
        /// Explanation.
        reason: &'static str,
    },
    /// Filesystem I/O failure.
    #[error("filesystem error at `{path}`: {reason}")]
    Io {
        /// Path being accessed.
        path: String,
        /// Underlying OS reason.
        reason: String,
    },
}

fn validate_relative_template_path(rel: &str) -> Result<(), InitError> {
    let p = Path::new(rel);
    if rel.is_empty() || p.is_absolute() {
        return Err(InitError::InvalidEntry {
            path: rel.to_owned(),
            reason: "template path must be non-empty and relative",
        });
    }
    for comp in p.components() {
        if !matches!(comp, Component::Normal(_)) {
            return Err(InitError::InvalidEntry {
                path: rel.to_owned(),
                reason: "template path must contain only normal path segments",
            });
        }
    }
    Ok(())
}

fn ensure_safe_target_root(target_dir: &Path) -> Result<PathBuf, InitError> {
    if target_dir.as_os_str().is_empty() {
        return Err(InitError::InvalidEntry {
            path: String::new(),
            reason: "target directory path must not be empty",
        });
    }

    match fs::symlink_metadata(target_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(InitError::SymlinkRefused {
                    path: target_dir.display().to_string(),
                });
            }
            if !meta.file_type().is_dir() {
                return Err(InitError::InvalidEntry {
                    path: target_dir.display().to_string(),
                    reason: "target path exists and is not a directory",
                });
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // Check existing ancestors for trailing symlinks before creating directories.
            let mut cursor = target_dir.parent();
            while let Some(anc) = cursor {
                if anc.as_os_str().is_empty() {
                    break;
                }
                if let Ok(anc_meta) = fs::symlink_metadata(anc) {
                    if anc_meta.file_type().is_symlink()
                        && anc != Path::new("/var")
                        && anc != Path::new("/tmp")
                        && anc != Path::new("/etc")
                    {
                        return Err(InitError::SymlinkRefused {
                            path: anc.display().to_string(),
                        });
                    }
                    break;
                }
                cursor = anc.parent();
            }

            fs::create_dir_all(target_dir).map_err(|io_err| InitError::Io {
                path: target_dir.display().to_string(),
                reason: io_err.to_string(),
            })?;
            let meta = fs::symlink_metadata(target_dir).map_err(|io_err| InitError::Io {
                path: target_dir.display().to_string(),
                reason: io_err.to_string(),
            })?;
            if meta.file_type().is_symlink() {
                return Err(InitError::SymlinkRefused {
                    path: target_dir.display().to_string(),
                });
            }
        }
        Err(err) => {
            return Err(InitError::Io {
                path: target_dir.display().to_string(),
                reason: err.to_string(),
            });
        }
    }

    target_dir.canonicalize().map_err(|err| InitError::Io {
        path: target_dir.display().to_string(),
        reason: err.to_string(),
    })
}

fn verify_confined(canon_root: &Path, candidate: &Path) -> Result<(), InitError> {
    let canon_cand = candidate.canonicalize().map_err(|err| InitError::Io {
        path: candidate.display().to_string(),
        reason: err.to_string(),
    })?;
    if !canon_cand.starts_with(canon_root) {
        return Err(InitError::PathEscape {
            root: canon_root.display().to_string(),
            path: canon_cand.display().to_string(),
        });
    }
    Ok(())
}

fn init_workspace(target_dir: &Path, force: bool) -> Result<PathBuf, InitError> {
    let canon_root = ensure_safe_target_root(target_dir)?;

    // 1. Pre-flight check subdirectories (`fixtures`) and all target files before writing anything.
    let fixtures_dir = target_dir.join("fixtures");
    match fs::symlink_metadata(&fixtures_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(InitError::SymlinkRefused {
                    path: fixtures_dir.display().to_string(),
                });
            }
            if !meta.file_type().is_dir() {
                return Err(InitError::InvalidEntry {
                    path: fixtures_dir.display().to_string(),
                    reason: "expected directory at `fixtures`",
                });
            }
            verify_confined(&canon_root, &fixtures_dir)?;
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(InitError::Io {
                path: fixtures_dir.display().to_string(),
                reason: err.to_string(),
            });
        }
    }

    for tpl in TEMPLATE_FILES {
        validate_relative_template_path(tpl.rel_path)?;
        let dest = target_dir.join(tpl.rel_path);
        match fs::symlink_metadata(&dest) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(InitError::SymlinkRefused {
                        path: dest.display().to_string(),
                    });
                }
                if !meta.file_type().is_file() {
                    return Err(InitError::InvalidEntry {
                        path: dest.display().to_string(),
                        reason: "destination exists and is not a regular file",
                    });
                }
                if !force {
                    return Err(InitError::AlreadyExists {
                        path: dest.display().to_string(),
                    });
                }
                verify_confined(&canon_root, &dest)?;
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(InitError::Io {
                    path: dest.display().to_string(),
                    reason: err.to_string(),
                });
            }
        }
    }

    // 2. Ensure `fixtures/` directory exists and remains confined.
    if fs::symlink_metadata(&fixtures_dir).is_err() {
        fs::create_dir(&fixtures_dir).map_err(|err| InitError::Io {
            path: fixtures_dir.display().to_string(),
            reason: err.to_string(),
        })?;
    }
    let fix_meta = fs::symlink_metadata(&fixtures_dir).map_err(|err| InitError::Io {
        path: fixtures_dir.display().to_string(),
        reason: err.to_string(),
    })?;
    if fix_meta.file_type().is_symlink() {
        return Err(InitError::SymlinkRefused {
            path: fixtures_dir.display().to_string(),
        });
    }
    verify_confined(&canon_root, &fixtures_dir)?;

    // 3. Write all files using `create_new(true)` (`O_CREAT | O_EXCL`, never follows symlinks).
    for tpl in TEMPLATE_FILES {
        let dest = target_dir.join(tpl.rel_path);
        if let Some(parent) = dest.parent() {
            let p_meta = fs::symlink_metadata(parent).map_err(|err| InitError::Io {
                path: parent.display().to_string(),
                reason: err.to_string(),
            })?;
            if p_meta.file_type().is_symlink() {
                return Err(InitError::SymlinkRefused {
                    path: parent.display().to_string(),
                });
            }
            verify_confined(&canon_root, parent)?;
        }

        if let Ok(meta) = fs::symlink_metadata(&dest) {
            if meta.file_type().is_symlink() {
                return Err(InitError::SymlinkRefused {
                    path: dest.display().to_string(),
                });
            }
            if !force {
                return Err(InitError::AlreadyExists {
                    path: dest.display().to_string(),
                });
            }
            fs::remove_file(&dest).map_err(|err| InitError::Io {
                path: dest.display().to_string(),
                reason: err.to_string(),
            })?;
        }

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)
            .map_err(|err| InitError::Io {
                path: dest.display().to_string(),
                reason: err.to_string(),
            })?;
        file.write_all(tpl.content.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|err| InitError::Io {
                path: dest.display().to_string(),
                reason: err.to_string(),
            })?;

        #[cfg(unix)]
        if tpl.executable {
            use std::os::unix::fs::PermissionsExt as _;
            let perms = fs::Permissions::from_mode(0o755);
            file.set_permissions(perms).map_err(|err| InitError::Io {
                path: dest.display().to_string(),
                reason: err.to_string(),
            })?;
        }
    }

    Ok(canon_root)
}

/// Runs `gap init` into `target_dir`, refusing overwrites unless `force` is `true` and refusing
/// any path or symlink escaping `target_dir`.
pub fn run_init(
    target_dir: &Path,
    force: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    match init_workspace(target_dir, force) {
        Ok(root) => {
            let _ = writeln!(
                stdout,
                "initialized GAP workspace in {} ({} files)",
                root.display(),
                TEMPLATE_FILES.len()
            );
            EXIT_OK
        }
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            EXIT_CONFIG_ERROR
        }
    }
}

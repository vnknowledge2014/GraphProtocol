//! Development-only test utilities, deterministic clocks, mock connectors, and workspace skeleton
//! policy verification for GAP (`exempt_dev`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod chaos;

#[cfg(test)]
mod workspace_policy_tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::{Path, PathBuf};

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("crates/gap-testkit sits two levels below workspace root")
            .to_path_buf()
    }

    fn discover_member_manifests(root: &Path) -> BTreeMap<String, PathBuf> {
        let mut manifests = BTreeMap::new();
        for dir_name in ["crates", "adapters", "bench"] {
            let group_dir = root.join(dir_name);
            if !group_dir.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&group_dir).expect("read member group directory") {
                let entry = entry.expect("dir entry");
                let manifest = entry.path().join("Cargo.toml");
                if manifest.is_file() {
                    let content = fs::read_to_string(&manifest).expect("read crate Cargo.toml");
                    let pkg_name = extract_package_name(&content).unwrap_or_else(|| {
                        panic!("missing package.name in {}", manifest.display())
                    });
                    manifests.insert(pkg_name, manifest);
                }
            }
        }
        manifests
    }

    fn extract_package_name(cargo_toml: &str) -> Option<String> {
        let mut in_package = false;
        for raw_line in cargo_toml.lines() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if line.starts_with('[') {
                in_package = line == "[package]";
                continue;
            }
            if in_package && let Some(rest) = line.strip_prefix("name") {
                let rest = rest.trim_start();
                if let Some(val) = rest.strip_prefix('=') {
                    return Some(val.trim().trim_matches('"').to_owned());
                }
            }
        }
        None
    }

    fn parse_layers_crates(layers_toml: &str) -> BTreeSet<String> {
        let mut expected = BTreeSet::new();
        let mut section = "";
        for raw_line in layers_toml.lines() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = &line[1..line.len() - 1];
                continue;
            }
            if section == "layers"
                && let Some((name, _)) = line.split_once('=')
            {
                expected.insert(name.trim().to_owned());
            } else if section == "exempt_dev"
                && line.starts_with("crates")
                && let Some((_, list)) = line.split_once('=')
            {
                for item in list.trim().trim_matches(['[', ']']).split(',') {
                    let cleaned = item.trim().trim_matches('"');
                    if !cleaned.is_empty() {
                        expected.insert(cleaned.to_owned());
                    }
                }
            }
        }
        expected
    }

    // spec: ARC-001
    #[test]
    fn workspace_crates_match_layers_and_workspace_deps() {
        let root = repo_root();
        let layers_text =
            fs::read_to_string(root.join("tools/layers.toml")).expect("read tools/layers.toml");
        let expected_crates = parse_layers_crates(&layers_text);
        let manifests = discover_member_manifests(&root);
        let actual_crates: BTreeSet<String> = manifests.keys().cloned().collect();

        assert_eq!(
            actual_crates, expected_crates,
            "workspace member crates must match tools/layers.toml [layers] U [exempt_dev]"
        );

        for (pkg, manifest_path) in &manifests {
            let content = fs::read_to_string(manifest_path).expect("read member Cargo.toml");
            let mut in_deps = false;
            for raw_line in content.lines() {
                let line = raw_line.split('#').next().unwrap_or("").trim();
                if line.is_empty() {
                    continue;
                }
                if line.starts_with('[') {
                    in_deps = matches!(
                        line,
                        "[dependencies]" | "[dev-dependencies]" | "[build-dependencies]"
                    );
                    continue;
                }
                if in_deps {
                    assert!(
                        line.contains(".workspace = true") || line.contains("workspace = true"),
                        "{pkg} ({}) must inherit dependency versions via workspace = true: `{line}`",
                        manifest_path.display()
                    );
                }
            }
        }
    }

    // spec: ARC-003
    #[test]
    fn workspace_crates_enforce_safety_and_clippy_headers() {
        let root = repo_root();
        let manifests = discover_member_manifests(&root);
        assert!(!manifests.is_empty(), "workspace manifests must exist");

        let required_lints = [
            "clippy::unwrap_used",
            "clippy::expect_used",
            "clippy::panic",
            "clippy::todo",
            "clippy::unimplemented",
        ];

        for (pkg, manifest_path) in &manifests {
            let crate_dir = manifest_path.parent().expect("crate directory");
            let lib_rs = crate_dir.join("src/lib.rs");
            let main_rs = crate_dir.join("src/main.rs");
            let entry_path = if lib_rs.is_file() {
                lib_rs
            } else if main_rs.is_file() {
                main_rs
            } else {
                panic!("{pkg} has neither src/lib.rs nor src/main.rs");
            };

            let src = fs::read_to_string(&entry_path).expect("read crate root source");
            assert!(
                src.lines().any(|l| l.starts_with("//! ")),
                "{pkg} ({}) must carry crate-level `//!` documentation",
                entry_path.display()
            );

            let has_forbid_unsafe_attr = src
                .lines()
                .map(str::trim)
                .any(|l| !l.starts_with("//") && l.contains("#![forbid(unsafe_code)]"));

            if pkg == "gap-s1" {
                assert!(
                    !has_forbid_unsafe_attr,
                    "gap-s1 is the sole crate exempted from #![forbid(unsafe_code)]"
                );
            } else {
                assert!(
                    has_forbid_unsafe_attr,
                    "{pkg} ({}) must carry #![forbid(unsafe_code)]",
                    entry_path.display()
                );
            }

            for lint in required_lints {
                assert!(
                    src.contains(lint),
                    "{pkg} ({}) must deny `{lint}` at crate root",
                    entry_path.display()
                );
            }
        }
    }
}

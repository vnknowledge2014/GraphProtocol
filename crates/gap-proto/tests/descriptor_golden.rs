//! Golden SHA-256 and changelog-coupling tests for compiled `FileDescriptorSet` artifacts.
#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use prost::Message as _;
use prost_types::FileDescriptorSet;
use sha2::{Digest, Sha256};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn sha256_lower_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn read_single_line_sha256(path: &Path, computed: &str) -> String {
    let prefix = &computed[..16];
    let guidance = format!(
        "To intentionally change the protobuf wire descriptor:\n\
         1. Update {} to contain exactly '{computed}' (single line, lower-case hex).\n\
         2. Add an entry to crates/gap-proto/CHANGELOG.md naming digest prefix '{prefix}'.\n\
         3. Note: breaking wire changes require a minor version bump before 1.0 (ADR-024).",
        path.display()
    );

    let raw = fs::read_to_string(path).unwrap_or_else(|err| {
        panic!(
            "missing or unreadable golden file {} ({err}).\nComputed SHA-256: {computed}\n{guidance}",
            path.display()
        )
    });

    let trimmed = raw.strip_suffix('\n').unwrap_or(&raw);
    assert!(
        !trimmed.contains('\n') && !trimmed.contains('\r'),
        "golden file {} must contain exactly one line, got {raw:?}.\n{guidance}",
        path.display()
    );
    assert!(
        trimmed.len() == 64
            && trimmed
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "golden file {} must be 64 lower-case hex characters, got {trimmed:?}.\n{guidance}",
        path.display()
    );

    trimmed.to_owned()
}

fn assert_golden_and_changelog(descriptor_bytes: &[u8], golden_rel_path: &str, label: &str) {
    let root = crate_dir();
    let golden_path = root.join(golden_rel_path);
    let changelog_path = root.join("CHANGELOG.md");

    let computed = sha256_lower_hex(descriptor_bytes);
    let prefix = &computed[..16];
    let expected = read_single_line_sha256(&golden_path, &computed);

    assert_eq!(
        computed, expected,
        "{label} descriptor SHA-256 mismatch!\n\
         Expected ({golden_rel_path}): {expected}\n\
         Actual (compiled descriptor): {computed}\n\
         Action required: update crates/gap-proto/{golden_rel_path} to '{computed}' AND add an \
         entry to crates/gap-proto/CHANGELOG.md referencing digest prefix '{prefix}'. \
         Breaking wire changes require a minor version bump before 1.0 (ADR-024)."
    );

    let changelog = fs::read_to_string(&changelog_path).unwrap_or_else(|err| {
        panic!(
            "missing crates/gap-proto/CHANGELOG.md ({err}); create it and record {label} digest \
             prefix '{prefix}' (breaking wire changes require a minor bump before 1.0, ADR-024)."
        )
    });

    assert!(
        changelog.contains(prefix),
        "crates/gap-proto/CHANGELOG.md does not mention current {label} descriptor digest prefix \
         '{prefix}' (full SHA-256: '{computed}').\n\
         Action required: update crates/gap-proto/{golden_rel_path} AND add a Keep-a-Changelog \
         entry in crates/gap-proto/CHANGELOG.md citing '{prefix}'. Breaking wire changes require \
         a minor version bump before 1.0 (ADR-024)."
    );
}

// spec: NGC-001
#[test]
fn descriptor_set_contains_only_sorted_own_files_without_source_info() {
    let fds = FileDescriptorSet::decode(gap_proto::DESCRIPTOR_SET)
        .expect("DESCRIPTOR_SET must decode as FileDescriptorSet");

    let names: Vec<&str> = fds.file.iter().map(|f| f.name()).collect();
    assert_eq!(
        names,
        vec!["gap/gateway/v1/gateway.proto", "gap/ngc/v1/ngc.proto"],
        "DESCRIPTOR_SET must contain own files only, sorted by name, with no imports"
    );

    for file in &fds.file {
        assert!(
            file.source_code_info.is_none(),
            "file {} in DESCRIPTOR_SET must omit source_code_info so comment edits never alter golden digests",
            file.name()
        );
    }
}

// spec: NGC-001
#[test]
fn descriptor_set_matches_golden_and_changelog() {
    assert_golden_and_changelog(
        gap_proto::DESCRIPTOR_SET,
        "golden/descriptor.sha256",
        "GAP/NGC",
    );
}

// spec: CMP-020
#[cfg(feature = "a2a")]
#[test]
fn a2a_descriptor_set_contains_only_a2a_without_source_info() {
    let fds = FileDescriptorSet::decode(gap_proto::A2A_DESCRIPTOR_SET)
        .expect("A2A_DESCRIPTOR_SET must decode as FileDescriptorSet");

    let names: Vec<&str> = fds.file.iter().map(|f| f.name()).collect();
    assert_eq!(
        names,
        vec!["a2a.proto"],
        "A2A_DESCRIPTOR_SET must contain a2a.proto only, without imported files"
    );

    for file in &fds.file {
        assert!(
            file.source_code_info.is_none(),
            "file {} in A2A_DESCRIPTOR_SET must omit source_code_info",
            file.name()
        );
    }
}

// spec: CMP-020
#[cfg(feature = "a2a")]
#[test]
fn a2a_descriptor_set_matches_golden_and_changelog() {
    assert_golden_and_changelog(
        gap_proto::A2A_DESCRIPTOR_SET,
        "golden/descriptor-a2a.sha256",
        "A2A",
    );
}

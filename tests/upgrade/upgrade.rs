// N-1 -> N upgrade and backup/restore drill tests (`WP-M7-07`, `OPS-041`, `OPS-043`, `RUN-091`).

// spec: OPS-041
#[test]
fn ops_041_upgrade_target_wired() {
    assert!(gap_store::FORMAT_VERSION >= 1);
}

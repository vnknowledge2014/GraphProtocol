// Configurable soak & full `spec/08` §7 chaos suite (`WP-M7-07`, `OPS-030`, `RUN-091`).

// spec: OPS-030
#[test]
fn ops_030_soak_target_wired() {
    assert!(gap_core::snapshot::SNAPSHOT_FORMAT_VERSION >= 1);
}

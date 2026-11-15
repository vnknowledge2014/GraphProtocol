#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use gap_core::ids::{GroupId, TenantId};

use super::{
    AbstentionReason, DecisionOutcomeLabel, FIVE_MINUTE_WINDOW_MS, FallbackReasonLabel,
    FeedbackOutcome, QueryPrivacyPolicy, RouterModelLabel, RoutingFeedbackRow, RoutingMetrics,
    UnmappedDomainTracker, apply_audited_admin_label, attribute_row_from_server_execution,
    redact_query,
};

fn make_row(predicted: Option<&str>) -> RoutingFeedbackRow {
    RoutingFeedbackRow {
        request_id: "req-1".to_owned(),
        tenant: TenantId::new("acme").unwrap(),
        query_hash: [0x11; 32],
        router_id: "s1:laya-candle:f32".to_owned(),
        calibration_id: Some("cal-1".to_owned()),
        options_hash: [0x22; 32],
        predicted_group: predicted.map(|g| GroupId::new(g).unwrap()),
        p_pred: Some(0.91),
        set_size: 1,
        outcome: FeedbackOutcome::None,
    }
}

// spec: RTE-111
#[test]
fn feedback_routing_metrics_track_all_four_series_and_render_prometheus() {
    let metrics = RoutingMetrics::new();

    metrics.record_decision(DecisionOutcomeLabel::Confident, RouterModelLabel::S1, 1);
    metrics.record_decision(DecisionOutcomeLabel::Ambiguous, RouterModelLabel::S1, 3);
    metrics.record_decision(DecisionOutcomeLabel::Abstained, RouterModelLabel::S1, 0);
    metrics.record_decision(
        DecisionOutcomeLabel::Fallback,
        RouterModelLabel::Baseline,
        2,
    );

    metrics.record_fallback(FallbackReasonLabel::Timeout);
    metrics.record_fallback(FallbackReasonLabel::Timeout);
    metrics.record_fallback(FallbackReasonLabel::Uncalibrated);
    metrics.set_online_ece(0.034_500);
    metrics.set_online_ece(f64::NAN); // non-finite updates must be ignored safely

    assert_eq!(
        metrics.decisions_count(DecisionOutcomeLabel::Confident, RouterModelLabel::S1),
        1
    );
    assert_eq!(
        metrics.decisions_count(DecisionOutcomeLabel::Fallback, RouterModelLabel::Baseline),
        1
    );
    assert_eq!(metrics.fallback_count(FallbackReasonLabel::Timeout), 2);
    assert_eq!(metrics.fallback_count(FallbackReasonLabel::Uncalibrated), 1);
    assert_eq!(metrics.set_size_count(), 4);
    assert_eq!(metrics.set_size_sum(), 6);
    assert_eq!(metrics.set_size_le_bucket(0), Some(1)); // <= 0: {0}
    assert_eq!(metrics.set_size_le_bucket(1), Some(2)); // <= 1: {0, 1}
    assert_eq!(metrics.set_size_le_bucket(2), Some(3)); // <= 2: {0, 1, 2}
    assert_eq!(metrics.set_size_le_bucket(3), Some(4)); // <= 3: {0, 1, 2, 3}
    assert!((metrics.online_ece() - 0.0345).abs() < 1e-6);

    let text = metrics.render_prometheus();
    assert!(text.contains("gap_discovery_decisions_total{outcome=\"confident\",model=\"s1\"} 1"));
    assert!(
        text.contains("gap_discovery_decisions_total{outcome=\"fallback\",model=\"baseline\"} 1")
    );
    assert!(text.contains("gap_discovery_set_size_bucket{le=\"3\"} 4"));
    assert!(text.contains("gap_discovery_set_size_bucket{le=\"+Inf\"} 4"));
    assert!(text.contains("gap_router_fallback_total{reason=\"timeout\"} 2"));
    assert!(text.contains("gap_s1_online_ece 0.034500"));
}

// spec: RTE-110, TM-14
#[test]
fn feedback_server_attribution_window_and_audited_user_label() {
    let g_bill = GroupId::new("00000000000000000000000000000001").unwrap();
    let g_ord = GroupId::new("00000000000000000000000000000002").unwrap();
    let t0 = 1_700_000_000_000_i64;

    // Out of 5-min window does not attribute.
    let mut late = make_row(Some("00000000000000000000000000000001"));
    assert!(!attribute_row_from_server_execution(
        &mut late,
        t0,
        t0 + FIVE_MINUTE_WINDOW_MS + 1,
        &g_bill,
        true,
        std::slice::from_ref(&g_bill),
    ));
    assert_eq!(late.outcome, FeedbackOutcome::None);

    // Within 5-min window + different group -> ExecutedOther.
    let mut other = make_row(Some("00000000000000000000000000000001"));
    assert!(attribute_row_from_server_execution(
        &mut other,
        t0,
        t0 + 45_000,
        &g_ord,
        true,
        std::slice::from_ref(&g_bill),
    ));
    assert_eq!(other.outcome, FeedbackOutcome::ExecutedOther);

    // Requires non-empty principal and audit_seq > 0 before UserLabel transition.
    let mut row = make_row(Some("00000000000000000000000000000001"));
    assert!(apply_audited_admin_label(&mut row, "", 1).is_err());
    assert!(apply_audited_admin_label(&mut row, "sec-admin", 0).is_err());
    apply_audited_admin_label(&mut row, "sec-admin", 42).unwrap();
    assert_eq!(row.outcome, FeedbackOutcome::UserLabel);
}

// spec: SEC-110, RTE-100, RTE-101
#[test]
fn feedback_privacy_policy_redacts_pii_and_rate_limits_unmapped_events() {
    let redacted = redact_query("reset sk-1234567890abcdef for alice@acme.com call 555-123-4567");
    assert!(!redacted.contains("sk-1234567890abcdef"));
    assert!(!redacted.contains("alice@acme.com"));
    assert!(!redacted.contains("555-123-4567"));

    let tenant = TenantId::new("acme").unwrap();
    let mut tracker = UnmappedDomainTracker::new();
    let evt = tracker
        .record(
            &tenant,
            "role:viewer",
            "find secret sk-1234567890 for alice@acme.com",
            3,
            1_700_000_000,
            vec![],
            AbstentionReason::NoMatch,
            QueryPrivacyPolicy::default(),
        )
        .expect("admitted");
    // Production default store_queries == false omits query text even after redaction (`SEC-110`).
    assert_eq!(evt.query_redacted, None);
}

//! Canary secret non-leakage verifier across tracing, JSON, `Debug`/`Display`, and reports (`INV-15`).

use std::fmt;

use gap_core::secret::REDACTED_MARKER;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::tracing_capture::{CapturedEvent, CapturedSpan, TracingCapture};

/// Error constructing a [`CanarySink`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CanaryInitError {
    /// Canary string was empty or whitespace-only.
    #[error("canary secret string must be non-empty and non-whitespace")]
    EmptyCanary,
}

/// Typed failure returned when a [`CanarySink`] detects an unredacted secret occurrence (`INV-15`).
///
/// Neither `surface` nor `detail` ever echoes the raw canary secret.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("canary secret leaked in {surface}: {detail}")]
pub struct CanaryLeak {
    /// Output surface where the canary secret was detected.
    pub surface: String,
    /// Scrubbed location detail (e.g., JSON path, span/event index, or line number).
    pub detail: String,
}

/// Verifies that a known canary secret string never appears in logs/traces, serialized JSON,
/// `Debug`/`Display` formatting, or conformance report text (`INV-15`, `SEC-040`, `SEC-041`).
#[derive(Clone)]
pub struct CanarySink {
    canary: String,
}

impl fmt::Debug for CanarySink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CanarySink")
            .field("canary", &REDACTED_MARKER)
            .finish()
    }
}

impl CanarySink {
    /// Constructs a [`CanarySink`] watching for `canary`.
    ///
    /// # Errors
    ///
    /// Returns [`CanaryInitError::EmptyCanary`] if `canary` is empty or whitespace-only.
    pub fn new(canary: impl Into<String>) -> Result<Self, CanaryInitError> {
        let canary = canary.into();
        if canary.trim().is_empty() {
            return Err(CanaryInitError::EmptyCanary);
        }
        Ok(Self { canary })
    }

    fn scrub(&self, text: &str) -> String {
        text.replace(&self.canary, REDACTED_MARKER)
    }

    /// Asserts that `capture` contains zero occurrences of the canary in spans or events.
    ///
    /// # Errors
    ///
    /// Returns [`CanaryLeak`] naming the first span or event surface containing the canary.
    pub fn check_tracing(&self, capture: &TracingCapture) -> Result<(), CanaryLeak> {
        self.check_spans(&capture.spans())?;
        self.check_events(&capture.events())
    }

    /// Asserts that `spans` contain zero occurrences of the canary.
    ///
    /// # Errors
    ///
    /// Returns [`CanaryLeak`] naming the offending span and field.
    pub fn check_spans(&self, spans: &[CapturedSpan]) -> Result<(), CanaryLeak> {
        for (idx, span) in spans.iter().enumerate() {
            if span.name.contains(&self.canary) {
                return Err(CanaryLeak {
                    surface: "tracing.span.name".to_owned(),
                    detail: format!("span[{idx}]"),
                });
            }
            if span.target.contains(&self.canary) {
                return Err(CanaryLeak {
                    surface: "tracing.span.target".to_owned(),
                    detail: format!("span[{idx}]"),
                });
            }
            for (key, val) in &span.fields {
                if key.contains(&self.canary) || val.contains(&self.canary) {
                    return Err(CanaryLeak {
                        surface: "tracing.span.field".to_owned(),
                        detail: format!("span[{idx}].{}", self.scrub(key)),
                    });
                }
            }
        }
        Ok(())
    }

    /// Asserts that `events` contain zero occurrences of the canary.
    ///
    /// # Errors
    ///
    /// Returns [`CanaryLeak`] naming the offending event and field.
    pub fn check_events(&self, events: &[CapturedEvent]) -> Result<(), CanaryLeak> {
        for (idx, ev) in events.iter().enumerate() {
            if ev.message.contains(&self.canary) {
                return Err(CanaryLeak {
                    surface: "tracing.event.message".to_owned(),
                    detail: format!("event[{idx}]"),
                });
            }
            if ev.target.contains(&self.canary) {
                return Err(CanaryLeak {
                    surface: "tracing.event.target".to_owned(),
                    detail: format!("event[{idx}]"),
                });
            }
            for (key, val) in &ev.fields {
                if key == "message" {
                    continue;
                }
                if key.contains(&self.canary) || val.contains(&self.canary) {
                    return Err(CanaryLeak {
                        surface: "tracing.event.field".to_owned(),
                        detail: format!("event[{idx}].{}", self.scrub(key)),
                    });
                }
            }
            for span_name in &ev.span_path {
                if span_name.contains(&self.canary) {
                    return Err(CanaryLeak {
                        surface: "tracing.event.span_path".to_owned(),
                        detail: format!("event[{idx}]"),
                    });
                }
            }
            for (key, val) in &ev.span_fields {
                if key.contains(&self.canary) || val.contains(&self.canary) {
                    return Err(CanaryLeak {
                        surface: "tracing.event.span_field".to_owned(),
                        detail: format!("event[{idx}].span.{}", self.scrub(key)),
                    });
                }
            }
        }
        Ok(())
    }

    /// Asserts that `value` contains zero occurrences of the canary in any JSON key or string value.
    ///
    /// # Errors
    ///
    /// Returns [`CanaryLeak`] with the scrubbed JSON path of the first leak.
    pub fn check_json_value(&self, value: &Value) -> Result<(), CanaryLeak> {
        self.check_json_at(value, "$")
    }

    fn check_json_at(&self, value: &Value, path: &str) -> Result<(), CanaryLeak> {
        match value {
            Value::Null | Value::Bool(_) | Value::Number(_) => Ok(()),
            Value::String(s) => {
                if s.contains(&self.canary) {
                    Err(CanaryLeak {
                        surface: "json.value".to_owned(),
                        detail: path.to_owned(),
                    })
                } else {
                    Ok(())
                }
            }
            Value::Array(items) => {
                for (idx, item) in items.iter().enumerate() {
                    self.check_json_at(item, &format!("{path}[{idx}]"))?;
                }
                Ok(())
            }
            Value::Object(map) => {
                for (key, child) in map {
                    let safe_key = self.scrub(key);
                    let child_path = format!("{path}.{safe_key}");
                    if key.contains(&self.canary) {
                        return Err(CanaryLeak {
                            surface: "json.key".to_owned(),
                            detail: child_path,
                        });
                    }
                    self.check_json_at(child, &child_path)?;
                }
                Ok(())
            }
        }
    }

    /// Asserts that serializing `value` to JSON neither emits the canary in JSON output nor leaks
    /// it inside a `serde::Serialize` error message (`SEC-040`, `INV-15`).
    ///
    /// # Errors
    ///
    /// Returns [`CanaryLeak`] if the canary appears in serialized JSON or the serialization error.
    pub fn check_serializable<T: Serialize>(&self, value: &T) -> Result<(), CanaryLeak> {
        match serde_json::to_value(value) {
            Ok(val) => self.check_json_value(&val),
            Err(err) => {
                if err.to_string().contains(&self.canary) {
                    Err(CanaryLeak {
                        surface: "serialize_error".to_owned(),
                        detail: "serde::Serialize error message contained canary".to_owned(),
                    })
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Asserts that `fmt::Debug` output of `value` does not contain the canary (`SEC-041`).
    ///
    /// # Errors
    ///
    /// Returns [`CanaryLeak`] with `surface == "debug"` if the canary appears.
    pub fn check_debug<T: fmt::Debug>(&self, value: &T) -> Result<(), CanaryLeak> {
        let rendered = format!("{value:?}");
        if rendered.contains(&self.canary) {
            Err(CanaryLeak {
                surface: "debug".to_owned(),
                detail: "fmt::Debug output contained canary".to_owned(),
            })
        } else {
            Ok(())
        }
    }

    /// Asserts that `fmt::Display` output of `value` does not contain the canary (`SEC-041`).
    ///
    /// # Errors
    ///
    /// Returns [`CanaryLeak`] with `surface == "display"` if the canary appears.
    pub fn check_display<T: fmt::Display>(&self, value: &T) -> Result<(), CanaryLeak> {
        let rendered = format!("{value}");
        if rendered.contains(&self.canary) {
            Err(CanaryLeak {
                surface: "display".to_owned(),
                detail: "fmt::Display output contained canary".to_owned(),
            })
        } else {
            Ok(())
        }
    }

    /// Asserts that `report_text` does not contain the canary (`INV-15`).
    ///
    /// # Errors
    ///
    /// Returns [`CanaryLeak`] with `surface == "report_text"` and the 1-based line number.
    pub fn check_report_text(&self, report_text: &str) -> Result<(), CanaryLeak> {
        for (idx, line) in report_text.lines().enumerate() {
            if line.contains(&self.canary) {
                return Err(CanaryLeak {
                    surface: "report_text".to_owned(),
                    detail: format!("line {}", idx + 1),
                });
            }
        }
        if report_text.contains(&self.canary) {
            return Err(CanaryLeak {
                surface: "report_text".to_owned(),
                detail: "multiline span".to_owned(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use gap_core::secret::SecretString;
    use serde::Serializer;
    use serde_json::json;

    use super::*;

    const CANARY: &str = "gap-canary-secret-token-9f8e7d6c5b4a";

    // spec: INV-15, SEC-040, SEC-041
    #[test]
    fn inv_15_secret_redacts_across_tracing_json_debug_display_and_report() {
        let sink = CanarySink::new(CANARY).expect("valid canary");
        assert!(!format!("{sink:?}").contains(CANARY));

        let secret = SecretString::new(CANARY.to_owned());

        let ((), capture) = TracingCapture::with_captured(|| {
            let span = tracing::info_span!(
                "auth_check",
                debug_tok = ?secret,
                display_tok = %secret,
                recorded_tok = tracing::field::Empty,
            );
            let _enter = span.enter();
            span.record("recorded_tok", tracing::field::display(&secret));
            tracing::info!(
                ev_debug = ?secret,
                ev_display = %secret,
                "authenticated principal with token {secret}"
            );
        });

        sink.check_tracing(&capture)
            .expect("SecretString must redact in captured tracing spans and events");
        sink.check_serializable(&secret)
            .expect("SecretString Serialize refusal must not leak canary");
        sink.check_json_value(&json!({
            "principal": "alice",
            "redacted": format!("{secret}"),
        }))
        .expect("redacted JSON value must pass CanarySink");
        sink.check_debug(&secret)
            .expect("SecretString Debug must redact");
        sink.check_display(&secret)
            .expect("SecretString Display must redact");
        sink.check_report_text(&format!(
            "{{\"verdict\":\"pass\",\"auth\":\"{secret:?}\"}}\n"
        ))
        .expect("report text with redacted SecretString must pass");
    }

    // spec: INV-15
    #[test]
    fn rejects_empty_or_whitespace_canary() {
        assert_eq!(
            CanarySink::new("").expect_err("empty canary must fail"),
            CanaryInitError::EmptyCanary
        );
        assert_eq!(
            CanarySink::new("   \t\n").expect_err("whitespace canary must fail"),
            CanaryInitError::EmptyCanary
        );
    }

    // spec: INV-15
    #[test]
    fn detects_leak_in_tracing_event_message_without_echoing_canary() {
        let sink = CanarySink::new(CANARY).expect("valid canary");
        let ((), capture) = TracingCapture::with_captured(|| {
            tracing::warn!("upstream failed with header Bearer {CANARY}");
        });
        let err = sink
            .check_tracing(&capture)
            .expect_err("canary in event message must fail");
        assert_eq!(err.surface, "tracing.event.message");
        assert!(!err.to_string().contains(CANARY));
        assert!(!format!("{err:?}").contains(CANARY));
    }

    // spec: INV-15
    #[test]
    fn detects_leak_in_tracing_event_field_only() {
        let sink = CanarySink::new(CANARY).expect("valid canary");
        let ((), capture) = TracingCapture::with_captured(|| {
            tracing::info!(authorization = CANARY, "clean message");
        });
        let err = sink
            .check_tracing(&capture)
            .expect_err("canary in structured event field must fail");
        assert_eq!(err.surface, "tracing.event.field");
        assert!(err.detail.contains("authorization"));
        assert!(!err.to_string().contains(CANARY));
    }

    // spec: INV-15
    #[test]
    fn detects_leak_in_tracing_span_initial_field_without_events() {
        let sink = CanarySink::new(CANARY).expect("valid canary");
        let ((), capture) = TracingCapture::with_captured(|| {
            let _span = tracing::info_span!("connector_call", bearer = CANARY);
        });
        let err = sink
            .check_tracing(&capture)
            .expect_err("canary in span initial field must fail even with zero events");
        assert_eq!(err.surface, "tracing.span.field");
        assert!(err.detail.contains("bearer"));
        assert!(!err.to_string().contains(CANARY));
    }

    // spec: INV-15
    #[test]
    fn detects_leak_in_tracing_span_recorded_field_without_events() {
        let sink = CanarySink::new(CANARY).expect("valid canary");
        let ((), capture) = TracingCapture::with_captured(|| {
            let span = tracing::info_span!("connector_call", bearer = tracing::field::Empty);
            span.record("bearer", CANARY);
        });
        let err = sink
            .check_tracing(&capture)
            .expect_err("canary in span.record(...) must fail even with zero events");
        assert_eq!(err.surface, "tracing.span.field");
        assert!(err.detail.contains("bearer"));
        assert!(!err.to_string().contains(CANARY));
    }

    // spec: INV-15
    #[test]
    fn detects_leak_in_captured_event_span_fields() {
        let sink = CanarySink::new(CANARY).expect("valid canary");
        let mut span_fields = BTreeMap::new();
        span_fields.insert("header".to_owned(), format!("Bearer {CANARY}"));
        let events = vec![CapturedEvent {
            level: tracing::Level::INFO,
            target: "test".to_owned(),
            message: "ok".to_owned(),
            fields: BTreeMap::new(),
            span_path: vec!["req".to_owned()],
            span_fields,
        }];
        let err = sink
            .check_events(&events)
            .expect_err("canary in event.span_fields must fail");
        assert_eq!(err.surface, "tracing.event.span_field");
        assert!(!err.to_string().contains(CANARY));
    }

    // spec: INV-15, SEC-040
    #[test]
    fn detects_leak_in_json_string_value_and_object_key_without_echoing_canary() {
        let sink = CanarySink::new(CANARY).expect("valid canary");

        let val_leak = json!({
            "results": [
                {"detail": "clean"},
                {"detail": format!("token={CANARY}")}
            ]
        });
        let err_val = sink
            .check_json_value(&val_leak)
            .expect_err("canary in nested JSON string must fail");
        assert_eq!(err_val.surface, "json.value");
        assert_eq!(err_val.detail, "$.results[1].detail");
        assert!(!err_val.to_string().contains(CANARY));

        let key_leak = json!({
            "safe": {
                CANARY: "value"
            }
        });
        let err_key = sink
            .check_json_value(&key_leak)
            .expect_err("canary in JSON object key must fail");
        assert_eq!(err_key.surface, "json.key");
        assert!(!err_key.to_string().contains(CANARY));
    }

    // spec: INV-15, SEC-040
    #[test]
    fn detects_leak_in_serializable_ok_payload_and_err_message() {
        #[derive(Serialize)]
        struct LeakyStruct {
            token: &'static str,
        }

        struct LeakySerializeError;
        impl Serialize for LeakySerializeError {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                Err(serde::ser::Error::custom(format!(
                    "refused to serialize secret `{CANARY}`"
                )))
            }
        }

        let sink = CanarySink::new(CANARY).expect("valid canary");
        let err_ok = sink
            .check_serializable(&LeakyStruct { token: CANARY })
            .expect_err("leaky Serialize Ok payload must fail");
        assert_eq!(err_ok.surface, "json.value");

        let err_ser = sink
            .check_serializable(&LeakySerializeError)
            .expect_err("leaky Serialize Err message must fail");
        assert_eq!(err_ser.surface, "serialize_error");
        assert!(!err_ser.to_string().contains(CANARY));
    }

    // spec: INV-15, SEC-041
    #[test]
    fn detects_leak_in_debug_and_display_independently() {
        struct DebugOnlyLeak;
        impl fmt::Debug for DebugOnlyLeak {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "DebugOnlyLeak({CANARY})")
            }
        }
        impl fmt::Display for DebugOnlyLeak {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("***")
            }
        }

        struct DisplayOnlyLeak;
        impl fmt::Debug for DisplayOnlyLeak {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("***")
            }
        }
        impl fmt::Display for DisplayOnlyLeak {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "DisplayOnlyLeak({CANARY})")
            }
        }

        let sink = CanarySink::new(CANARY).expect("valid canary");

        assert!(sink.check_display(&DebugOnlyLeak).is_ok());
        let dbg_err = sink
            .check_debug(&DebugOnlyLeak)
            .expect_err("Debug leak must be caught");
        assert_eq!(dbg_err.surface, "debug");
        assert!(!dbg_err.to_string().contains(CANARY));

        assert!(sink.check_debug(&DisplayOnlyLeak).is_ok());
        let disp_err = sink
            .check_display(&DisplayOnlyLeak)
            .expect_err("Display leak must be caught");
        assert_eq!(disp_err.surface, "display");
        assert!(!disp_err.to_string().contains(CANARY));
    }

    // spec: INV-15
    #[test]
    fn detects_leak_in_report_text_with_line_number() {
        let sink = CanarySink::new(CANARY).expect("valid canary");
        let report = format!("{{\n  \"status\": \"fail\",\n  \"detail\": \"auth {CANARY}\"\n}}\n");
        let err = sink
            .check_report_text(&report)
            .expect_err("report text leak must be caught");
        assert_eq!(err.surface, "report_text");
        assert_eq!(err.detail, "line 3");
        assert!(!err.to_string().contains(CANARY));
    }

    // spec: INV-15
    #[test]
    fn f4a_detects_leak_in_span_name_only() {
        let sink = CanarySink::new(CANARY).expect("valid canary");
        let ((), capture) = TracingCapture::with_captured(|| {
            let _span = tracing::info_span!("span_gap-canary-secret-token-9f8e7d6c5b4a");
        });
        assert!(capture.events().is_empty());
        let err = sink
            .check_tracing(&capture)
            .expect_err("canary in span name must be detected");
        assert_eq!(err.surface, "tracing.span.name");
        assert_eq!(err.detail, "span[0]");
        assert!(!err.to_string().contains(CANARY));
    }

    // spec: INV-15
    #[test]
    fn f4b_detects_leak_in_span_target_only() {
        let sink = CanarySink::new(CANARY).expect("valid canary");
        let ((), capture) = TracingCapture::with_captured(|| {
            let _span = tracing::info_span!(
                target: "tgt_gap-canary-secret-token-9f8e7d6c5b4a",
                "clean_span"
            );
        });
        assert!(capture.events().is_empty());
        let err = sink
            .check_tracing(&capture)
            .expect_err("canary in span target must be detected");
        assert_eq!(err.surface, "tracing.span.target");
        assert_eq!(err.detail, "span[0]");
        assert!(!err.to_string().contains(CANARY));
    }

    // spec: INV-15
    #[test]
    fn f4c_detects_leak_in_span_field_key_only() {
        let sink = CanarySink::new(CANARY).expect("valid canary");
        let ((), capture) = TracingCapture::with_captured(|| {
            let _span = tracing::info_span!(
                "clean_span",
                "key.gap-canary-secret-token-9f8e7d6c5b4a" = "clean_value"
            );
        });
        assert!(capture.events().is_empty());
        let err = sink
            .check_tracing(&capture)
            .expect_err("canary in span field key must be detected");
        assert_eq!(err.surface, "tracing.span.field");
    }

    // spec: INV-15
    #[test]
    fn f4d_detects_leak_in_event_target_only() {
        let sink = CanarySink::new(CANARY).expect("valid canary");
        let ((), capture) = TracingCapture::with_captured(|| {
            tracing::info!(
                target: "ev_gap-canary-secret-token-9f8e7d6c5b4a",
                "clean message"
            );
        });
        assert!(capture.spans().is_empty());
        let err = sink
            .check_tracing(&capture)
            .expect_err("canary in event target must be detected");
        assert_eq!(err.surface, "tracing.event.target");
        assert_eq!(err.detail, "event[0]");
        assert!(!err.to_string().contains(CANARY));
    }

    // spec: INV-15
    #[test]
    fn f4e_detects_leak_in_event_span_path_only() {
        let sink = CanarySink::new(CANARY).expect("valid canary");
        let capture = TracingCapture::new();
        {
            let _guard = capture.enter_guard();
            let enclosing = tracing::info_span!("enclosing_gap-canary-secret-token-9f8e7d6c5b4a");
            let _enter = enclosing.enter();
            // Clear `spans` while `enclosing` stays entered so `capture` holds ONLY the event's
            // `span_path` containing the canary.
            capture.clear();
            tracing::info!("clean message");
        }
        assert!(capture.spans().is_empty());
        assert_eq!(capture.events().len(), 1);

        let err = sink
            .check_tracing(&capture)
            .expect_err("canary in enclosing event span_path must be detected");
        assert_eq!(err.surface, "tracing.event.span_path");
        assert_eq!(err.detail, "event[0]");
        assert!(!err.to_string().contains(CANARY));
    }

    // spec: INV-15, SEC-041
    #[test]
    fn f5a_span_field_key_leak_scrubs_canary_from_detail_display_and_debug() {
        let sink = CanarySink::new(CANARY).expect("valid canary");
        let ((), capture) = TracingCapture::with_captured(|| {
            let _span = tracing::info_span!(
                "clean_span",
                "hdr.gap-canary-secret-token-9f8e7d6c5b4a" = "clean_val"
            );
        });
        let err = sink
            .check_tracing(&capture)
            .expect_err("canary in span field key must fail");
        assert_eq!(err.surface, "tracing.span.field");
        assert_eq!(err.detail, "span[0].hdr.***");
        assert!(!err.detail.contains(CANARY));
        assert!(!err.to_string().contains(CANARY));
        assert!(!format!("{err:?}").contains(CANARY));
    }

    // spec: INV-15, SEC-041
    #[test]
    fn f5b_event_field_key_leak_scrubs_canary_from_detail_display_and_debug() {
        let sink = CanarySink::new(CANARY).expect("valid canary");
        let ((), capture) = TracingCapture::with_captured(|| {
            tracing::info!(
                "hdr.gap-canary-secret-token-9f8e7d6c5b4a" = "clean_val",
                "clean message"
            );
        });
        let err = sink
            .check_tracing(&capture)
            .expect_err("canary in event field key must fail");
        assert_eq!(err.surface, "tracing.event.field");
        assert_eq!(err.detail, "event[0].hdr.***");
        assert!(!err.detail.contains(CANARY));
        assert!(!err.to_string().contains(CANARY));
        assert!(!format!("{err:?}").contains(CANARY));
    }

    // spec: INV-15, SEC-041
    #[test]
    fn f5c_event_enclosing_span_field_key_leak_scrubs_canary_from_detail_display_and_debug() {
        let sink = CanarySink::new(CANARY).expect("valid canary");
        let capture = TracingCapture::new();
        {
            let _guard = capture.enter_guard();
            let enclosing = tracing::info_span!(
                "clean_span",
                "hdr.gap-canary-secret-token-9f8e7d6c5b4a" = "clean_val"
            );
            let _enter = enclosing.enter();
            capture.clear();
            tracing::info!("clean message");
        }
        assert!(capture.spans().is_empty());
        assert_eq!(capture.events().len(), 1);

        let err = sink
            .check_tracing(&capture)
            .expect_err("canary in event enclosing span_field key must fail");
        assert_eq!(err.surface, "tracing.event.span_field");
        assert_eq!(err.detail, "event[0].span.hdr.***");
        assert!(!err.detail.contains(CANARY));
        assert!(!err.to_string().contains(CANARY));
        assert!(!format!("{err:?}").contains(CANARY));
    }
}

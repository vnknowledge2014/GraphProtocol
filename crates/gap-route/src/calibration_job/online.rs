//! Online `gap_s1_online_ece` rolling-window monitor (`RTE-111`) and non-blocking scheduled
//! recalibration snapshot staging (`RTE-068`, `INV-01`, `WP-M5-04`).

use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::Timestamp;
use gap_core::error::GapResult;
use gap_core::snapshot::{CalibrationTable, Snapshot};

use super::pipeline::{
    CalibrationExample, CalibrationPipelineReport, WinsorizedConfig, run_calibration_pipeline,
};
use super::{CalibrationVersionKey, VersionedCalibrationRecord};

/// Minimum labeled samples per online ECE evaluation window (`RTE-111`).
pub const DEFAULT_ONLINE_WINDOW_SIZE: usize = 500;

/// Default online ECE alert threshold across 2 consecutive windows (`RTE-111`).
pub const DEFAULT_ONLINE_ECE_ALERT_THRESHOLD: f32 = 0.08;

/// Default abstention-rate multiplier vs baseline triggering a distribution-shift alert (`RTE-111`).
pub const DEFAULT_ABSTENTION_SHIFT_MULTIPLIER: f32 = 2.0;

/// Single labeled observation streamed from `routing_feedback` into [`OnlineCalibrationMonitor`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OnlineCalibrationSample {
    /// Predicted top probability at discovery time (`p_pred` in `RTE-110`).
    pub p_pred: f32,
    /// Whether server-observed execution matched the top candidate.
    pub correct: bool,
    /// Whether `gap.resolve` abstained (`ABSTAINED`).
    pub abstained: bool,
    /// Observation timestamp in UNIX epoch seconds.
    pub timestamp_secs: u64,
}

/// Configuration knobs for [`OnlineCalibrationMonitor`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OnlineMonitorConfig {
    /// Minimum labeled samples required before finalizing each evaluation window (`>= 500`).
    pub window_size: usize,
    /// Online ECE threshold (`0.08` default) alerted on 2 consecutive breaches (`RTE-111`).
    pub ece_threshold: f32,
    /// Historical baseline abstention rate (`0.0..=1.0`) for shift alerts (`RTE-111`).
    pub baseline_abstention_rate: f32,
    /// Shift multiplier over `baseline_abstention_rate` (`2.0` default, `RTE-111`).
    pub abstention_shift_multiplier: f32,
}

impl Default for OnlineMonitorConfig {
    fn default() -> Self {
        Self {
            window_size: DEFAULT_ONLINE_WINDOW_SIZE,
            ece_threshold: DEFAULT_ONLINE_ECE_ALERT_THRESHOLD,
            baseline_abstention_rate: 0.10,
            abstention_shift_multiplier: DEFAULT_ABSTENTION_SHIFT_MULTIPLIER,
        }
    }
}

/// Structured alert emitted when online calibration degrades (`RTE-111`).
#[derive(Debug, Clone, PartialEq)]
pub enum OnlineEceAlert {
    /// `online_ece > threshold` occurred in two consecutive `>= 500`-row windows (`RTE-111`).
    ConsecutiveHighEce {
        /// Zero-based completed window index of the second breach.
        window_index: u64,
        /// Previous window `ECE_15`.
        previous_ece: f32,
        /// Current window `ECE_15`.
        current_ece: f32,
        /// Configured threshold (`0.08`).
        threshold: f32,
    },
    /// Abstention rate shifted above `multiplier * baseline_abstention_rate` (`RTE-111`).
    AbstentionRateShift {
        /// Zero-based completed window index.
        window_index: u64,
        /// Baseline abstention fraction.
        baseline_rate: f32,
        /// Current window abstention fraction.
        current_rate: f32,
        /// Configured multiplier (`2.0`).
        multiplier: f32,
    },
}

/// Summary emitted whenever a full online window completes.
#[derive(Debug, Clone, PartialEq)]
pub struct OnlineWindowEvaluation {
    /// Completed window index.
    pub window_index: u64,
    /// Number of labeled samples in the completed window.
    pub sample_count: usize,
    /// Computed 15-bin online Expected Calibration Error (`gap_s1_online_ece`).
    pub online_ece: f32,
    /// Observed abstention rate over the window.
    pub abstention_rate: f32,
    /// Any triggered drift alerts.
    pub alerts: Vec<OnlineEceAlert>,
}

/// Stateful online calibration monitor tracking `gap_s1_online_ece` (`RTE-111`).
#[derive(Debug, Clone)]
pub struct OnlineCalibrationMonitor {
    config: OnlineMonitorConfig,
    buffer: Vec<OnlineCalibrationSample>,
    last_online_ece: Option<f32>,
    prev_ece_breach: Option<f32>,
    windows_completed: u64,
}

impl OnlineCalibrationMonitor {
    /// Builds a monitor enforcing a minimum window of `500` labeled rows (`RTE-111`).
    #[must_use]
    pub fn new(mut config: OnlineMonitorConfig) -> Self {
        config.window_size = config.window_size.max(DEFAULT_ONLINE_WINDOW_SIZE);
        Self {
            buffer: Vec::with_capacity(config.window_size),
            config,
            last_online_ece: None,
            prev_ece_breach: None,
            windows_completed: 0,
        }
    }

    /// Returns the latest `gap_s1_online_ece` gauge value once at least one `>= 500`-row window has
    /// completed (`None` while fewer than `window_size` labeled rows exist).
    #[must_use]
    pub const fn online_ece_gauge(&self) -> Option<f32> {
        self.last_online_ece
    }

    /// Number of completed `>= 500`-sample windows evaluated so far.
    #[must_use]
    pub const fn windows_completed(&self) -> u64 {
        self.windows_completed
    }

    /// Current buffered sample count waiting for the next window boundary.
    #[must_use]
    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    /// Records one labeled observation; returns `Some(evaluation)` whenever a full window finishes.
    pub fn observe(&mut self, sample: OnlineCalibrationSample) -> Option<OnlineWindowEvaluation> {
        self.buffer.push(sample);
        if self.buffer.len() < self.config.window_size {
            return None;
        }
        Some(self.finalize_window())
    }

    fn finalize_window(&mut self) -> OnlineWindowEvaluation {
        let mut bin_counts = [0usize; 15];
        let mut bin_conf = [0.0_f64; 15];
        let mut bin_acc = [0.0_f64; 15];
        let mut abstained_count = 0usize;

        let n = self.buffer.len();
        for s in &self.buffer {
            if s.abstained {
                abstained_count += 1;
            }
            let p = if s.p_pred.is_finite() {
                s.p_pred.clamp(0.0, 1.0)
            } else {
                0.0
            };
            let bin = (((f64::from(p) * 15.0).floor() as isize).clamp(0, 14)) as usize;
            bin_counts[bin] += 1;
            bin_conf[bin] += f64::from(p);
            if s.correct {
                bin_acc[bin] += 1.0;
            }
        }

        let total = n as f64;
        let mut ece = 0.0_f64;
        for b in 0..15 {
            let c = bin_counts[b];
            if c > 0 {
                let avg_c = bin_conf[b] / (c as f64);
                let avg_a = bin_acc[b] / (c as f64);
                ece += ((c as f64) / total) * (avg_a - avg_c).abs();
            }
        }
        let online_ece = ece as f32;
        let abstention_rate = (abstained_count as f32) / (n as f32);
        let window_index = self.windows_completed;
        self.windows_completed = self.windows_completed.saturating_add(1);
        self.last_online_ece = Some(online_ece);

        let mut alerts = Vec::new();
        if online_ece > self.config.ece_threshold {
            if let Some(prev) = self.prev_ece_breach {
                alerts.push(OnlineEceAlert::ConsecutiveHighEce {
                    window_index,
                    previous_ece: prev,
                    current_ece: online_ece,
                    threshold: self.config.ece_threshold,
                });
            }
            self.prev_ece_breach = Some(online_ece);
        } else {
            self.prev_ece_breach = None;
        }

        if self.config.baseline_abstention_rate > 0.0
            && abstention_rate
                > self.config.baseline_abstention_rate * self.config.abstention_shift_multiplier
        {
            alerts.push(OnlineEceAlert::AbstentionRateShift {
                window_index,
                baseline_rate: self.config.baseline_abstention_rate,
                current_rate: abstention_rate,
                multiplier: self.config.abstention_shift_multiplier,
            });
        }

        self.buffer.clear();
        OnlineWindowEvaluation {
            window_index,
            sample_count: n,
            online_ece,
            abstention_rate,
            alerts,
        }
    }
}

/// Non-blocking scheduled offline/feedback recalibration coordinator (`RTE-068`).
///
/// Refits temperature & conformal parameters off-path and constructs candidate `Snapshot`
/// generations for normal atomic publish without mutating active request state (`INV-01`).
#[derive(Debug, Clone)]
pub struct ScheduledRecalibrationJob {
    /// Minimum cadence between automatic runs.
    pub cadence: Duration,
    /// Bound version key attached to newly built tables (`RTE-067`).
    pub version_key: CalibrationVersionKey,
    /// Optional winsorization policy (`TM-14`).
    pub winsor: WinsorizedConfig,
    last_run_epoch_secs: Option<u64>,
}

impl ScheduledRecalibrationJob {
    /// Creates a scheduled job executing every `cadence` for `version_key`.
    #[must_use]
    pub const fn new(
        cadence: Duration,
        version_key: CalibrationVersionKey,
        winsor: WinsorizedConfig,
    ) -> Self {
        Self {
            cadence,
            version_key,
            winsor,
            last_run_epoch_secs: None,
        }
    }

    /// Returns `true` when no previous run occurred or `cadence` has elapsed since `last_run_epoch_secs`.
    #[must_use]
    pub const fn is_due(&self, now_epoch_secs: u64) -> bool {
        match self.last_run_epoch_secs {
            None => true,
            Some(prev) => now_epoch_secs.saturating_sub(prev) >= self.cadence.as_secs(),
        }
    }

    /// Executes one off-path recalibration run over `examples` and records `now_epoch_secs`.
    ///
    /// # Errors
    ///
    /// Propagates [`run_calibration_pipeline`] validation errors without mutating `last_run_epoch_secs`.
    pub fn run_once(
        &mut self,
        examples: &[CalibrationExample],
        now_epoch_secs: u64,
    ) -> GapResult<(VersionedCalibrationRecord, CalibrationPipelineReport)> {
        let out = run_calibration_pipeline(self.version_key.clone(), examples, self.winsor)?;
        self.last_run_epoch_secs = Some(now_epoch_secs);
        Ok(out)
    }

    /// Stages `current.generation + 1` carrying `table` while preserving all existing snapshot
    /// `Arc` fields via structural sharing (`ARC-005`, `RTE-068`).
    #[must_use]
    pub fn stage_next_snapshot(
        current: &Snapshot,
        table: CalibrationTable,
        built_at: Timestamp,
    ) -> Snapshot {
        let mut next = current.next_generation(
            current.generation.saturating_add(1),
            built_at,
            "gap-recalibration-job",
            None,
        );
        next.calibration = Some(Arc::new(table));
        next
    }
}

//! Monotonic/wall-clock abstraction and RFC 3339 UTC `Timestamp` (`spec/00-conventions.md` §7).

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

const NANOS_PER_SEC: i128 = 1_000_000_000;
const NANOS_PER_MILLI: i128 = 1_000_000;
const NANOS_PER_MICRO: i128 = 1_000;
const SECS_PER_DAY: i128 = 86_400;

/// Minimum representable timestamp (`0000-01-01T00:00:00Z`) in nanoseconds from Unix epoch.
const MIN_TIMESTAMP_NANOS: i128 = -62_167_219_200 * NANOS_PER_SEC;
/// Maximum representable timestamp (`9999-12-31T23:59:59.999999999Z`) in nanoseconds from Unix epoch.
const MAX_TIMESTAMP_NANOS: i128 = 253_402_300_799 * NANOS_PER_SEC + 999_999_999;

/// Error returned when parsing or constructing an out-of-range RFC 3339 UTC timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TimestampError {
    /// Input string was not valid RFC 3339 UTC (`YYYY-MM-DDTHH:MM:SS[.fffffffff]Z`).
    #[error("invalid RFC 3339 UTC timestamp `{value}`: {reason}")]
    InvalidFormat {
        /// Rejected input string.
        value: String,
        /// Specific syntax or calendar violation.
        reason: &'static str,
    },
    /// Timestamp lies outside civil years `0000..=9999`.
    #[error("timestamp `{0}` is outside representable RFC 3339 range 0000..=9999")]
    OutOfRange(i128),
}

/// RFC 3339 UTC timestamp (`Z` suffix mandatory, `spec/00-conventions.md` §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    unix_nanos: i128,
}

impl Timestamp {
    /// `1970-01-01T00:00:00Z`.
    pub const UNIX_EPOCH: Self = Self { unix_nanos: 0 };

    /// Constructs a [`Timestamp`] from nanoseconds since `1970-01-01T00:00:00Z`.
    ///
    /// # Errors
    ///
    /// Returns [`TimestampError::OutOfRange`] if outside civil years `0000..=9999`.
    pub const fn from_unix_nanos(unix_nanos: i128) -> Result<Self, TimestampError> {
        if unix_nanos < MIN_TIMESTAMP_NANOS || unix_nanos > MAX_TIMESTAMP_NANOS {
            return Err(TimestampError::OutOfRange(unix_nanos));
        }
        Ok(Self { unix_nanos })
    }

    /// Constructs a [`Timestamp`] from integer milliseconds since `1970-01-01T00:00:00Z`.
    ///
    /// # Errors
    ///
    /// Returns [`TimestampError::OutOfRange`] if outside civil years `0000..=9999`.
    pub const fn from_unix_millis(unix_millis: i64) -> Result<Self, TimestampError> {
        Self::from_unix_nanos((unix_millis as i128) * NANOS_PER_MILLI)
    }

    /// Nanoseconds since `1970-01-01T00:00:00Z`.
    #[must_use]
    pub const fn unix_nanos(self) -> i128 {
        self.unix_nanos
    }

    /// Microseconds since `1970-01-01T00:00:00Z` (`*_us` telemetry units).
    #[must_use]
    pub const fn unix_micros(self) -> i64 {
        self.unix_nanos.div_euclid(NANOS_PER_MICRO) as i64
    }

    /// Milliseconds since `1970-01-01T00:00:00Z` (`*Ms` wire API units).
    #[must_use]
    pub const fn unix_millis(self) -> i64 {
        self.unix_nanos.div_euclid(NANOS_PER_MILLI) as i64
    }
}

impl TryFrom<SystemTime> for Timestamp {
    type Error = TimestampError;

    fn try_from(time: SystemTime) -> Result<Self, Self::Error> {
        match time.duration_since(UNIX_EPOCH) {
            Ok(dur) => {
                let nanos = i128::try_from(dur.as_nanos())
                    .map_err(|_| TimestampError::OutOfRange(i128::MAX))?;
                Self::from_unix_nanos(nanos)
            }
            Err(before) => {
                let nanos = i128::try_from(before.duration().as_nanos())
                    .map_err(|_| TimestampError::OutOfRange(i128::MIN))?;
                Self::from_unix_nanos(-nanos)
            }
        }
    }
}

const fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

const fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Proleptic Gregorian civil date to days relative to `1970-01-01` (Hinnant algorithm).
const fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = (year as i64) - (month <= 2) as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = month as u64;
    let d = day as u64;
    let doy = (153 * if m > 2 { m - 3 } else { m + 9 } + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + (doe as i64) - 719_468
}

/// Days relative to `1970-01-01` to proleptic Gregorian `(year, month, day)` (Hinnant algorithm).
const fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + (m <= 2) as i64;
    (year as i32, m as u32, d as u32)
}

fn parse_ascii_digits(bytes: &[u8]) -> Option<u32> {
    let mut acc: u32 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
    }
    Some(acc)
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total_secs = self.unix_nanos.div_euclid(NANOS_PER_SEC);
        let subsec_nanos = self.unix_nanos.rem_euclid(NANOS_PER_SEC) as u32;
        let days = total_secs.div_euclid(SECS_PER_DAY) as i64;
        let secs_of_day = total_secs.rem_euclid(SECS_PER_DAY) as u32;

        let (year, month, day) = civil_from_days(days);
        let hour = secs_of_day / 3_600;
        let minute = (secs_of_day % 3_600) / 60;
        let second = secs_of_day % 60;

        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
        )?;
        if subsec_nanos == 0 {
            f.write_str("Z")
        } else if subsec_nanos.is_multiple_of(1_000_000) {
            write!(f, ".{:03}Z", subsec_nanos / 1_000_000)
        } else if subsec_nanos.is_multiple_of(1_000) {
            write!(f, ".{:06}Z", subsec_nanos / 1_000)
        } else {
            write!(f, ".{subsec_nanos:09}Z")
        }
    }
}

impl FromStr for Timestamp {
    type Err = TimestampError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = |reason: &'static str| TimestampError::InvalidFormat {
            value: s.to_owned(),
            reason,
        };
        let bytes = s.as_bytes();
        // Minimum length: "YYYY-MM-DDTHH:MM:SSZ" == 20 bytes
        if bytes.len() < 20 {
            return Err(invalid("too short for YYYY-MM-DDTHH:MM:SSZ"));
        }
        if bytes[4] != b'-'
            || bytes[7] != b'-'
            || bytes[10] != b'T'
            || bytes[13] != b':'
            || bytes[16] != b':'
        {
            return Err(invalid("expected YYYY-MM-DDTHH:MM:SS separators"));
        }
        if *bytes.last().unwrap_or(&0) != b'Z' {
            return Err(invalid("must end with uppercase UTC 'Z' suffix"));
        }

        let year = parse_ascii_digits(&bytes[0..4]).ok_or_else(|| invalid("invalid year"))? as i32;
        let month = parse_ascii_digits(&bytes[5..7]).ok_or_else(|| invalid("invalid month"))?;
        let day = parse_ascii_digits(&bytes[8..10]).ok_or_else(|| invalid("invalid day"))?;
        let hour = parse_ascii_digits(&bytes[11..13]).ok_or_else(|| invalid("invalid hour"))?;
        let minute = parse_ascii_digits(&bytes[14..16]).ok_or_else(|| invalid("invalid minute"))?;
        let second = parse_ascii_digits(&bytes[17..19]).ok_or_else(|| invalid("invalid second"))?;

        if !(1..=12).contains(&month) {
            return Err(invalid("month out of 01..=12"));
        }
        let max_day = days_in_month(year, month);
        if day < 1 || day > max_day {
            return Err(invalid("day out of range for month"));
        }
        if hour > 23 || minute > 59 || second > 59 {
            return Err(invalid("time component out of 00..=23 / 00..=59"));
        }

        let tail = &bytes[19..bytes.len() - 1];
        let subsec_nanos: u32 = if tail.is_empty() {
            0
        } else {
            if tail[0] != b'.' {
                return Err(invalid("unexpected characters before trailing 'Z'"));
            }
            let frac = &tail[1..];
            if frac.is_empty() || frac.len() > 9 {
                return Err(invalid("fractional seconds must be 1..=9 digits"));
            }
            let raw = parse_ascii_digits(frac)
                .ok_or_else(|| invalid("fractional seconds must be ASCII digits"))?;
            let scale = 10_u32.pow(9 - (frac.len() as u32));
            raw.saturating_mul(scale)
        };

        let days = i128::from(days_from_civil(year, month, day));
        let secs = days * SECS_PER_DAY
            + i128::from(hour) * 3_600
            + i128::from(minute) * 60
            + i128::from(second);
        let unix_nanos = secs * NANOS_PER_SEC + i128::from(subsec_nanos);
        Self::from_unix_nanos(unix_nanos)
    }
}

impl Serialize for Timestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = Cow::<'de, str>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Timestamp {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Timestamp")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "format": "date-time",
            "description": "RFC 3339 UTC timestamp ending with 'Z'."
        })
    }
}

/// Abstract monotonic and wall-clock time source (`INV-02`).
pub trait Clock: Send + Sync + fmt::Debug {
    /// Monotonic instant for deadlines and elapsed-duration measurement.
    fn now_instant(&self) -> Instant;

    /// Wall-clock `SystemTime`.
    fn now_system(&self) -> SystemTime;

    /// Current RFC 3339 UTC [`Timestamp`].
    fn now_timestamp(&self) -> Timestamp {
        Timestamp::try_from(self.now_system()).unwrap_or(Timestamp::UNIX_EPOCH)
    }

    /// Current Unix timestamp in integer milliseconds (`*Ms`).
    fn unix_millis(&self) -> i64 {
        self.now_timestamp().unix_millis()
    }
}

/// Production OS clock backed by [`Instant::now`] and [`SystemTime::now`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_instant(&self) -> Instant {
        Instant::now()
    }

    fn now_system(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Deterministic, lock-free clock for tests (`INV-02`: uses `AtomicU64`, never a `Mutex`).
#[derive(Debug)]
pub struct ManualClock {
    base_instant: Instant,
    base_system: SystemTime,
    offset_nanos: AtomicU64,
}

impl ManualClock {
    /// Creates a [`ManualClock`] anchored at `start_unix_millis` (`>= 0`).
    #[must_use]
    pub fn new(start_unix_millis: u64) -> Self {
        Self {
            base_instant: Instant::now(),
            base_system: UNIX_EPOCH + Duration::from_millis(start_unix_millis),
            offset_nanos: AtomicU64::new(0),
        }
    }

    /// Atomically advances both monotonic and wall time by `delta` (`Ordering::AcqRel`).
    pub fn advance(&self, delta: Duration) {
        let add_nanos = u64::try_from(delta.as_nanos()).unwrap_or(u64::MAX);
        let _ = self
            .offset_nanos
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                Some(cur.saturating_add(add_nanos))
            });
    }

    /// Total duration advanced since construction.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        Duration::from_nanos(self.offset_nanos.load(Ordering::Acquire))
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new(1_700_000_000_000)
    }
}

impl Clock for ManualClock {
    fn now_instant(&self) -> Instant {
        let delta = self.elapsed();
        self.base_instant
            .checked_add(delta)
            .unwrap_or(self.base_instant)
    }

    fn now_system(&self) -> SystemTime {
        let delta = self.elapsed();
        self.base_system
            .checked_add(delta)
            .unwrap_or(self.base_system)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    // spec: INV-02, ARC-010
    #[test]
    fn inv_02_manual_clock_advances_atomically_without_locks() {
        let clock = ManualClock::new(1_700_000_000_000);
        let i0 = clock.now_instant();
        let ms0 = clock.unix_millis();
        assert_eq!(ms0, 1_700_000_000_000);

        clock.advance(Duration::from_millis(1_250));
        assert_eq!(
            clock.now_instant().duration_since(i0),
            Duration::from_millis(1_250)
        );
        assert_eq!(clock.unix_millis(), 1_700_000_001_250);

        let sys_clock = SystemClock;
        assert!(sys_clock.unix_millis() > 0);
    }

    // spec: ARC-010
    #[test]
    fn rfc3339_timestamp_parses_utc_z_and_rejects_offsets_or_bad_dates() {
        let ts: Timestamp = "2026-10-26T22:50:46.123Z".parse().expect("valid RFC 3339");
        assert_eq!(ts.to_string(), "2026-10-26T22:50:46.123Z");

        let leap: Timestamp = "2024-02-29T23:59:59Z".parse().expect("2024 is leap year");
        assert_eq!(leap.to_string(), "2024-02-29T23:59:59Z");

        for invalid in [
            "",
            "2026-10-26 22:50:46Z",
            "2026-10-26T22:50:46+07:00",
            "2026-10-26T22:50:46z",
            "2023-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-10-26T24:00:00Z",
            "2026-10-26T22:50:46.Z",
            "2026-10-26T22:50:46.1234567890Z",
        ] {
            assert!(
                invalid.parse::<Timestamp>().is_err(),
                "expected `{invalid}` to fail RFC 3339 UTC Z validation"
            );
        }
    }

    proptest! {
        // spec: ARC-010
        #[test]
        fn prop_timestamp_roundtrip_display_and_json(
            year in 0_i32..=9999,
            month in 1_u32..=12,
            day_seed in 1_u32..=31,
            hour in 0_u32..=23,
            minute in 0_u32..=59,
            second in 0_u32..=59,
            nanos in 0_u32..=999_999_999,
        ) {
            let day = day_seed.min(days_in_month(year, month));
            let formatted = format!(
                "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{nanos:09}Z"
            );
            let parsed: Timestamp = formatted.parse().expect("valid generated timestamp");
            let reparsed: Timestamp = parsed.to_string().parse().expect("canonical string reparses");
            prop_assert_eq!(parsed, reparsed);

            let json = serde_json::to_string(&parsed).expect("serialize timestamp");
            let from_json: Timestamp = serde_json::from_str(&json).expect("deserialize timestamp");
            prop_assert_eq!(parsed, from_json);
        }
    }
}

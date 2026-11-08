//! `ScheduleSpec`, the typed shape of `schedules.spec` JSONB, and `compute_next`, which turns a
//! spec plus the current time into the next deadline. Pure (no DB/Redis), so it unit-tests
//! standalone.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::parser::{CronParser, Seconds};
use croner::Cron;
use serde::{Deserialize, Serialize};

/// Floor `validate` enforces on interval schedules at write time (env-overridable). `compute_next`
/// doesn't clamp to it: a row below it came from an operator override, which should be honored.
pub const EVERY_SECS_FLOOR: i64 = 60;
/// Interval jitter cap (seconds), to de-sync herds of identically-scheduled intervals.
const INTERVAL_JITTER_CAP_SECS: i64 = 10;
/// Backoff base/cap (seconds): `60 * 2^(n-1)`, capped at 1h.
const BACKOFF_BASE_SECS: i64 = 60;
const BACKOFF_CAP_SECS: i64 = 3600;

/// `schedules.spec` — polymorphic cadence.
///
/// `#[serde(tag = "kind")]` yields
/// `{"kind":"interval","every_secs":300}`, `{"kind":"cron","expr":"0 30 9 * * MON-FRI"}` and
/// `{"kind":"at","time":"09:30","days":["mon","thu"]}` (`days` omitted = daily).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleSpec {
    Interval {
        every_secs: i64,
    },
    Cron {
        expr: String,
    },
    At {
        time: String,
        #[serde(default)]
        days: Option<Vec<String>>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("interval every_secs must be >= {0}")]
    IntervalTooShort(i64),
    #[error("invalid cron expression: {0}")]
    BadCron(String),
    #[error("invalid time '{0}' (want HH:MM)")]
    BadTime(String),
    #[error("invalid day '{0}'")]
    BadDay(String),
    #[error("invalid timezone '{0}'")]
    BadTimezone(String),
    #[error("no next occurrence found")]
    NoNextOccurrence,
}

/// `at` sugar compiles to a 6-field cron expression (seconds forced to 0), in one place, so
/// downstream has only two evaluators.
fn at_to_cron(time: &str, days: Option<&[String]>) -> Result<String, ScheduleError> {
    let (h, m) = parse_hh_mm(time)?;
    let dow = match days {
        None => "*".to_string(),
        Some(ds) => ds
            .iter()
            .map(|d| day_token(d))
            .collect::<Result<Vec<_>, _>>()?
            .join(","),
    };
    Ok(format!("0 {m} {h} * * {dow}"))
}

fn parse_hh_mm(s: &str) -> Result<(u32, u32), ScheduleError> {
    let bad = || ScheduleError::BadTime(s.to_string());
    let (h, m) = s.split_once(':').ok_or_else(bad)?;
    let h: u32 = h.parse().map_err(|_| bad())?;
    let m: u32 = m.parse().map_err(|_| bad())?;
    if h > 23 || m > 59 {
        return Err(bad());
    }
    Ok((h, m))
}

fn day_token(d: &str) -> Result<&'static str, ScheduleError> {
    Ok(match d.to_lowercase().as_str() {
        "mon" | "monday" => "MON",
        "tue" | "tuesday" => "TUE",
        "wed" | "wednesday" => "WED",
        "thu" | "thursday" => "THU",
        "fri" | "friday" => "FRI",
        "sat" | "saturday" => "SAT",
        "sun" | "sunday" => "SUN",
        _ => return Err(ScheduleError::BadDay(d.to_string())),
    })
}

/// Parses and validates a spec against a floor; the deadline loop assumes specs passed this once.
pub fn validate(spec: &ScheduleSpec, floor: i64) -> Result<(), ScheduleError> {
    match spec {
        ScheduleSpec::Interval { every_secs } if *every_secs < floor => {
            Err(ScheduleError::IntervalTooShort(floor))
        }
        ScheduleSpec::Interval { .. } => Ok(()),
        ScheduleSpec::Cron { expr } => compile_cron(expr).map(drop),
        ScheduleSpec::At { time, days } => {
            compile_cron(&at_to_cron(time, days.as_deref())?).map(drop)
        }
    }
}

fn compile_cron(expr: &str) -> Result<Cron, ScheduleError> {
    CronParser::builder()
        .seconds(Seconds::Required)
        // Stored specs may use `0/10` and `/10`, which strict step syntax rejects.
        .sloppy_ranges(true)
        .build()
        .parse(expr)
        .map_err(|e| ScheduleError::BadCron(format!("{e}")))
}

/// Logs once per `(jitter_seed, every_secs)` when an interval reads back below
/// [`EVERY_SECS_FLOOR`], not on every tick. Keyed on the pair because callers without an id yet
/// legitimately pass seed `0`.
fn warn_once_below_floor(jitter_seed: u64, every_secs: i64) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static WARNED: OnceLock<Mutex<HashSet<(u64, i64)>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut warned = warned
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if warned.insert((jitter_seed, every_secs)) {
        tracing::warn!(
            jitter_seed,
            every_secs,
            floor_secs = EVERY_SECS_FLOOR,
            "schedule interval is below the validation floor but was accepted through an \
             operator override; honoring the configured interval rather than silently \
             multiplying it"
        );
    }
}

/// The one function the deadline loop and the control-plane API share: spec, IANA timezone, current
/// instant, consecutive-failure count and a jitter seed in, next deadline out.
///
/// `jitter_seed` comes from `schedule.id` rather than a wall-clock RNG, giving a stable
/// per-schedule offset that is reproducible in tests.
pub fn compute_next(
    spec: &ScheduleSpec,
    timezone: &str,
    now: DateTime<Utc>,
    consecutive_failures: i32,
    jitter_seed: u64,
) -> Result<DateTime<Utc>, ScheduleError> {
    let tz: Tz = timezone
        .parse()
        .map_err(|_| ScheduleError::BadTimezone(timezone.to_string()))?;

    let natural: DateTime<Utc> = match spec {
        ScheduleSpec::Interval { every_secs } => {
            let every = *every_secs;
            if every < EVERY_SECS_FLOOR {
                warn_once_below_floor(jitter_seed, every);
            }
            let cap = (every / 20).clamp(0, INTERVAL_JITTER_CAP_SECS);
            let jitter = (jitter_seed % (cap.cast_unsigned() + 1)).cast_signed();
            now + chrono::Duration::seconds(every + jitter)
        }
        ScheduleSpec::Cron { expr } => next_cron(expr, tz, now)?,
        ScheduleSpec::At { time, days } => next_cron(&at_to_cron(time, days.as_deref())?, tz, now)?,
    };

    // Backoff floor only — compute_next never auto-pauses; finalize owns the
    // consecutive-failure auto-pause rule.
    if consecutive_failures <= 0 {
        return Ok(natural);
    }
    let shift = (consecutive_failures - 1).min(20).cast_unsigned();
    let backoff = BACKOFF_BASE_SECS
        .saturating_mul(1i64 << shift)
        .min(BACKOFF_CAP_SECS);
    Ok(natural.max(now + chrono::Duration::seconds(backoff)))
}

fn next_cron(expr: &str, tz: Tz, now: DateTime<Utc>) -> Result<DateTime<Utc>, ScheduleError> {
    let cron = compile_cron(expr)?;
    let local = now.with_timezone(&tz);
    let next = cron
        .find_next_occurrence(&local, false)
        .map_err(|_| ScheduleError::NoNextOccurrence)?;
    Ok(next.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    // --- serde: JSON shapes round-trip ---

    #[test]
    fn interval_spec_round_trips() {
        let json = serde_json::json!({"kind":"interval","every_secs":300});
        let spec: ScheduleSpec = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(spec, ScheduleSpec::Interval { every_secs: 300 });
        assert_eq!(serde_json::to_value(&spec).unwrap(), json);
    }

    #[test]
    fn cron_spec_round_trips() {
        let json = serde_json::json!({"kind":"cron","expr":"0 30 9 * * MON-FRI"});
        let spec: ScheduleSpec = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            spec,
            ScheduleSpec::Cron {
                expr: "0 30 9 * * MON-FRI".to_string()
            }
        );
        assert_eq!(serde_json::to_value(&spec).unwrap(), json);
    }

    #[test]
    fn at_spec_with_days_round_trips() {
        let json = serde_json::json!({"kind":"at","time":"09:30","days":["mon","thu"]});
        let spec: ScheduleSpec = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            spec,
            ScheduleSpec::At {
                time: "09:30".to_string(),
                days: Some(vec!["mon".to_string(), "thu".to_string()]),
            }
        );
        assert_eq!(serde_json::to_value(&spec).unwrap(), json);
    }

    #[test]
    fn at_spec_without_days_parses_as_daily() {
        let json = serde_json::json!({"kind":"at","time":"00:00"});
        let spec: ScheduleSpec = serde_json::from_value(json).unwrap();
        assert_eq!(
            spec,
            ScheduleSpec::At {
                time: "00:00".to_string(),
                days: None,
            }
        );
    }

    // --- interval + jitter ---

    #[test]
    fn interval_next_is_within_jitter_bounds() {
        let spec = ScheduleSpec::Interval { every_secs: 300 };
        let now = utc(2026, 1, 1, 0, 0, 0);
        let next = compute_next(&spec, "UTC", now, 0, 7).unwrap();
        let delta = (next - now).num_seconds();
        assert!((300..=310).contains(&delta), "delta={delta}");
    }

    #[test]
    fn interval_jitter_is_stable_for_a_fixed_seed() {
        let spec = ScheduleSpec::Interval { every_secs: 300 };
        let now = utc(2026, 1, 1, 0, 0, 0);
        let a = compute_next(&spec, "UTC", now, 0, 42).unwrap();
        let b = compute_next(&spec, "UTC", now, 0, 42).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn interval_jitter_differs_across_seeds() {
        let spec = ScheduleSpec::Interval { every_secs: 300 };
        let now = utc(2026, 1, 1, 0, 0, 0);
        let a = compute_next(&spec, "UTC", now, 0, 1).unwrap();
        let b = compute_next(&spec, "UTC", now, 0, 9).unwrap();
        assert_ne!(a, b, "different seeds should (almost always) desync");
    }

    #[test]
    fn interval_below_floor_is_honoured_not_clamped() {
        // Below EVERY_SECS_FLOOR only via an operator override; compute_next must honor the
        // configured interval.
        let spec = ScheduleSpec::Interval { every_secs: 40 };
        let now = utc(2026, 1, 1, 0, 0, 0);
        let next = compute_next(&spec, "UTC", now, 0, 0).unwrap();
        let delta = (next - now).num_seconds();
        assert!((40..=41).contains(&delta), "delta={delta}");
    }

    #[test]
    fn interval_100_secs_caps_jitter_at_5() {
        // cap = min(100/20, 10) = 5
        let spec = ScheduleSpec::Interval { every_secs: 100 };
        let now = utc(2026, 1, 1, 0, 0, 0);
        for seed in 0..20u64 {
            let next = compute_next(&spec, "UTC", now, 0, seed).unwrap();
            let delta = (next - now).num_seconds();
            assert!((100..=105).contains(&delta), "seed={seed} delta={delta}");
        }
    }

    // --- cron in tz ---

    #[test]
    fn cron_fires_at_correct_utc_instant_in_new_york_tz() {
        let spec = ScheduleSpec::Cron {
            expr: "0 30 9 * * MON-FRI".to_string(),
        };
        // Wed 2026-01-07 00:00 UTC; next 09:30 America/New_York (EST) is 14:30 UTC.
        let now = utc(2026, 1, 7, 0, 0, 0);
        let next = compute_next(&spec, "America/New_York", now, 0, 0).unwrap();
        assert_eq!(next, utc(2026, 1, 7, 14, 30, 0));
    }

    #[test]
    fn at_daily_compiles_and_fires_at_right_minute() {
        let spec = ScheduleSpec::At {
            time: "09:30".to_string(),
            days: None,
        };
        let now = utc(2026, 1, 1, 0, 0, 0);
        let next = compute_next(&spec, "UTC", now, 0, 0).unwrap();
        assert_eq!(next, utc(2026, 1, 1, 9, 30, 0));
    }

    #[test]
    fn at_weekly_compiles_and_fires_on_the_right_day() {
        let spec = ScheduleSpec::At {
            time: "09:30".to_string(),
            days: Some(vec!["thu".to_string()]),
        };
        // 2026-01-01 is a Thursday, at 00:00 — next Thursday 09:30 is the same day.
        let now = utc(2026, 1, 1, 0, 0, 0);
        let next = compute_next(&spec, "UTC", now, 0, 0).unwrap();
        assert_eq!(next, utc(2026, 1, 1, 9, 30, 0));
    }

    // --- DST ---

    #[test]
    fn dst_spring_forward_gap_rolls_to_first_valid_instant() {
        // 02:30 local doesn't exist on 2026-03-08 (clocks jump 02:00 -> 03:00 EDT); next fire is
        // 03:00 EDT = 07:00 UTC.
        let spec = ScheduleSpec::Cron {
            expr: "0 30 2 * * *".to_string(),
        };
        let now = utc(2026, 3, 8, 6, 0, 0); // just before the gap, in UTC terms
        let next = compute_next(&spec, "America/New_York", now, 0, 0).unwrap();
        assert_eq!(next, utc(2026, 3, 8, 7, 0, 0));
    }

    #[test]
    fn dst_fall_back_ambiguous_time_fires_once_at_earliest_occurrence() {
        // 01:30 local occurs twice on 2026-11-01; must fire once, at the earlier (EDT) instant:
        // 05:30 UTC.
        let spec = ScheduleSpec::Cron {
            expr: "0 30 1 * * *".to_string(),
        };
        let now = utc(2026, 11, 1, 4, 0, 0);
        let next = compute_next(&spec, "America/New_York", now, 0, 0).unwrap();
        assert_eq!(next, utc(2026, 11, 1, 5, 30, 0));
    }

    // --- sloppy step syntax ---

    #[test]
    fn sloppy_step_expressions_parse_and_fire_like_star_step() {
        let now = utc(2026, 1, 1, 0, 0, 5);
        let expect = utc(2026, 1, 1, 0, 0, 10);
        for expr in ["0/10 * * * * *", "/10 * * * * *", "*/10 * * * * *"] {
            let spec = ScheduleSpec::Cron {
                expr: expr.to_string(),
            };
            validate(&spec, EVERY_SECS_FLOOR).unwrap_or_else(|e| panic!("{expr}: {e}"));
            assert_eq!(
                compute_next(&spec, "UTC", now, 0, 0).unwrap(),
                expect,
                "{expr}"
            );
        }
        let spec = ScheduleSpec::Cron {
            expr: "0 5/15 * * * *".to_string(),
        };
        let next = compute_next(&spec, "UTC", utc(2026, 1, 1, 0, 6, 0), 0, 0).unwrap();
        assert_eq!(next, utc(2026, 1, 1, 0, 20, 0));
    }

    // --- backoff ---

    #[test]
    fn backoff_schedule_matches_60s_times_2_pow_n_minus_1_capped_at_1h() {
        let spec = ScheduleSpec::Interval { every_secs: 300 };
        let now = utc(2026, 1, 1, 0, 0, 0);

        // failures=1: backoff=60s; natural=now+300..310 dominates.
        let next = compute_next(&spec, "UTC", now, 1, 0).unwrap();
        assert_eq!(next, now + chrono::Duration::seconds(300));

        // failures=2: backoff=120s; still dominated by natural (300s).
        let next = compute_next(&spec, "UTC", now, 2, 0).unwrap();
        assert_eq!(next, now + chrono::Duration::seconds(300));

        // failures=3: backoff=240s; still dominated by natural (300s).
        let next = compute_next(&spec, "UTC", now, 3, 0).unwrap();
        assert_eq!(next, now + chrono::Duration::seconds(300));

        // failures=5: backoff = 60*2^4 = 960s > natural (300s) -> backoff wins.
        let next = compute_next(&spec, "UTC", now, 5, 0).unwrap();
        assert_eq!(next, now + chrono::Duration::seconds(960));

        // failures=10: backoff = 60*2^9 = 30720s, capped at 3600s.
        let next = compute_next(&spec, "UTC", now, 10, 0).unwrap();
        assert_eq!(next, now + chrono::Duration::seconds(3600));
    }

    #[test]
    fn backoff_never_speeds_up_a_daily_cron_grid() {
        let spec = ScheduleSpec::Cron {
            expr: "0 30 9 * * *".to_string(),
        };
        let now = utc(2026, 1, 1, 0, 0, 0);
        let natural = compute_next(&spec, "UTC", now, 0, 0).unwrap();
        let with_backoff = compute_next(&spec, "UTC", now, 3, 0).unwrap();
        assert_eq!(
            natural, with_backoff,
            "a failed daily cron must not retry sooner than its next grid slot"
        );
    }

    // --- cold-start coalesce-to-one ---

    #[test]
    fn cold_start_far_past_deadline_produces_a_single_future_instant_not_a_burst() {
        let spec = ScheduleSpec::Interval { every_secs: 300 };
        // next_run_at hours in the past (worker down): compute_next only looks at `now`, so no
        // backlog to burst through.
        let now = utc(2026, 1, 1, 12, 0, 0);
        let next = compute_next(&spec, "UTC", now, 0, 0).unwrap();
        assert!(next > now);
        assert!((next - now).num_seconds() <= 310);
    }

    // --- validation ---

    #[test]
    fn validate_rejects_interval_below_floor() {
        let spec = ScheduleSpec::Interval { every_secs: 30 };
        assert!(validate(&spec, 60).is_err());
    }

    #[test]
    fn validate_accepts_interval_at_floor() {
        let spec = ScheduleSpec::Interval { every_secs: 60 };
        assert!(validate(&spec, 60).is_ok());
    }

    #[test]
    fn validate_rejects_bad_cron() {
        let spec = ScheduleSpec::Cron {
            expr: "not a cron".to_string(),
        };
        assert!(validate(&spec, 60).is_err());
    }

    #[test]
    fn validate_rejects_bad_timezone_via_compute_next() {
        let spec = ScheduleSpec::Interval { every_secs: 300 };
        let now = Utc::now();
        assert!(matches!(
            compute_next(&spec, "Not/AZone", now, 0, 0),
            Err(ScheduleError::BadTimezone(_))
        ));
    }

    #[test]
    fn validate_rejects_bad_day_token() {
        let spec = ScheduleSpec::At {
            time: "09:00".to_string(),
            days: Some(vec!["funday".to_string()]),
        };
        assert!(validate(&spec, 60).is_err());
    }
}

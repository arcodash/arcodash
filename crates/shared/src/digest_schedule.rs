//! Per-user daily-digest send-time matching.
//!
//! The digest fan-out ticks roughly hourly and, for each digest-enabled user, needs to know whether
//! *this* tick is the one that should send their email — i.e. whether `now`, converted into their
//! chosen timezone, currently reads their chosen send hour. Shared by both fan-out entry points so
//! the timezone math exists exactly once.

use chrono::{DateTime, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;

/// Send hour used for a user whose `digest_hour` is unset (8:00 UTC, together with
/// [`DEFAULT_DIGEST_TIMEZONE`]).
pub const DEFAULT_DIGEST_HOUR: i16 = 8;
/// Timezone used the same way, for `digest_timezone`.
pub const DEFAULT_DIGEST_TIMEZONE: &str = "UTC";

fn resolve_tz(timezone: Option<&str>) -> Tz {
    timezone
        .unwrap_or(DEFAULT_DIGEST_TIMEZONE)
        .parse()
        .unwrap_or(chrono_tz::UTC)
}

/// True once `now`, local to the user's timezone, has reached or passed their chosen send hour on
/// the current local day.
///
/// A bad or missing timezone falls back to UTC rather than erroring — the send is best-effort, not
/// a validation gate (validation happens at write time in the API handler).
///
/// Deliberately `>=` rather than `==` the target hour: the fan-out tick runs roughly hourly, and an
/// exact-hour match would silently skip a user's digest entirely for any tick the worker missed (a
/// deploy, a crash, a delayed restart) spanning their send hour — the next tick would already read
/// a later hour and never match again until the following day. Catching up is safe because the
/// caller's per-user-per-local-day guard (`scheduler::claim_digest_send_for_user`) already stops
/// every later tick the same day from sending a second time, so this only ever causes a late first
/// send, never a duplicate one.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "hour() is 0..=23, which fits i16"
)]
pub fn user_is_due(hour: Option<i16>, timezone: Option<&str>, now: DateTime<Utc>) -> bool {
    let hour = hour.unwrap_or(DEFAULT_DIGEST_HOUR);
    now.with_timezone(&resolve_tz(timezone)).hour() as i16 >= hour
}

/// The user's local calendar date at `now`.
///
/// Used to key the once-per-day send guard on the day as the user experiences it, not UTC's, so a
/// user near a timezone's midnight boundary gets exactly one send per local day rather than one per
/// UTC day.
#[must_use]
pub fn user_local_date(timezone: Option<&str>, now: DateTime<Utc>) -> NaiveDate {
    now.with_timezone(&resolve_tz(timezone)).date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn due_at_default_hour_and_timezone_when_unset() {
        let now = utc(2026, 1, 1, 8, 15);
        assert!(user_is_due(None, None, now));
        let not_yet = utc(2026, 1, 1, 7, 45);
        assert!(!user_is_due(None, None, not_yet));
    }

    #[test]
    fn due_at_local_hour_in_a_non_utc_timezone() {
        // 08:00 Europe/Berlin (CET, UTC+1) in January is 07:00 UTC.
        let now = utc(2026, 1, 15, 7, 30);
        assert!(user_is_due(Some(8), Some("Europe/Berlin"), now));
        assert!(!user_is_due(Some(8), Some("UTC"), now));
    }

    #[test]
    fn due_matches_san_francisco_local_morning_not_utc_overnight() {
        // 08:00 America/Los_Angeles (PST, UTC-8) in January is 16:00 UTC — nowhere near an
        // 08:00 UTC send, which is 00:00 local for this user.
        let now = utc(2026, 1, 15, 16, 10);
        assert!(user_is_due(Some(8), Some("America/Los_Angeles"), now));
        assert!(!user_is_due(
            Some(8),
            Some("America/Los_Angeles"),
            utc(2026, 1, 15, 8, 0)
        ));
    }

    #[test]
    fn a_missed_tick_catches_up_later_the_same_day() {
        // Target hour 8, but the first tick that actually runs today is at local hour 10 (the
        // worker was down/deploying through the 8am and 9am ticks) — must still fire.
        let now = utc(2026, 1, 1, 10, 5);
        assert!(user_is_due(Some(8), Some("UTC"), now));
        // Still not due before the target hour even reached once.
        let too_early = utc(2026, 1, 1, 7, 59);
        assert!(!user_is_due(Some(8), Some("UTC"), too_early));
    }

    #[test]
    fn bad_timezone_string_falls_back_to_utc_rather_than_panicking() {
        let now = utc(2026, 1, 1, 8, 30);
        assert!(user_is_due(Some(8), Some("Not/AZone"), now));
    }

    #[test]
    fn local_date_crosses_the_utc_day_boundary_correctly() {
        // 23:30 UTC on Jan 1 is already 00:30 Jan 2 in a UTC+1 zone.
        let now = utc(2026, 1, 1, 23, 30);
        assert_eq!(
            user_local_date(Some("Europe/Berlin"), now),
            NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()
        );
        assert_eq!(
            user_local_date(Some("UTC"), now),
            NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
        );
    }
}

//! Monthly AI spend accounting helpers.

use chrono::{DateTime, Datelike, Months, TimeZone, Utc};
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, EntityTrait, Statement};

use super::settings::monthly_budget_usd;
use super::AiError;
use crate::entities::organization;

/// `[start of the current UTC calendar month, start of the next)`.
#[must_use]
pub fn month_window(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .unwrap();
    (start, start + Months::new(1))
}

/// Priced USD spent by the org since the start of the current UTC month. Unpriced calls
/// (NULL cost) contribute nothing.
pub async fn spent_this_month(db: &impl ConnectionTrait, org_id: i64) -> Result<f64, DbErr> {
    let (start, _) = month_window(Utc::now());
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT COALESCE(SUM(cost_usd), 0)::double precision AS spent \
             FROM ai_usage WHERE org_id = $1 AND created_at >= $2",
            [org_id.into(), start.into()],
        ))
        .await?;
    row.map_or(Ok(0.0), |r| r.try_get("", "spent"))
}

/// Where an org stands against its monthly cap.
#[derive(Debug, Clone, Copy)]
pub struct BudgetState {
    pub cap: Option<f64>,
    pub spent_month: f64,
    pub resets_at: DateTime<Utc>,
}

/// Errors with [`AiError::BudgetExceeded`] once spend reaches the cap.
///
/// The SUM query is skipped when no cap is set. A DB failure fails open: a transient blip must not
/// block all AI. The cap is soft: spend is checked before a call and recorded after, so concurrent
/// in-flight calls can overshoot it slightly.
pub async fn check_budget(db: &impl ConnectionTrait, org_id: i64) -> Result<BudgetState, AiError> {
    let (_, resets_at) = month_window(Utc::now());
    let mut state = BudgetState {
        cap: None,
        spent_month: 0.0,
        resets_at,
    };
    let cap = match organization::Entity::find_by_id(org_id).one(db).await {
        Ok(Some(org)) => monthly_budget_usd(&org.settings),
        Ok(None) => None,
        Err(_) => {
            tracing::warn!(org_id, "budget check failed reading org settings; allowing");
            return Ok(state);
        }
    };
    let Some(cap) = cap else { return Ok(state) };
    state.cap = Some(cap);
    if let Ok(spent) = spent_this_month(db, org_id).await {
        state.spent_month = spent;
        if spent >= cap {
            return Err(AiError::BudgetExceeded {
                cap,
                spent,
                resets_at,
            });
        }
    } else {
        tracing::warn!(org_id, "budget check failed reading spend; allowing");
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    #[test]
    fn mid_month() {
        assert_eq!(
            month_window(at(2026, 10, 11, 13, 5, 9)),
            (at(2026, 10, 1, 0, 0, 0), at(2026, 11, 1, 0, 0, 0))
        );
    }

    #[test]
    fn december_rolls_into_january() {
        assert_eq!(
            month_window(at(2026, 12, 31, 23, 59, 59)),
            (at(2026, 12, 1, 0, 0, 0), at(2027, 1, 1, 0, 0, 0))
        );
    }

    #[test]
    fn first_second_of_a_month() {
        assert_eq!(
            month_window(at(2026, 3, 1, 0, 0, 0)),
            (at(2026, 3, 1, 0, 0, 0), at(2026, 4, 1, 0, 0, 0))
        );
    }

    #[test]
    fn budget_message_formats_cap_and_reset_date() {
        assert_eq!(
            crate::ai::budget_message(1.0, at(2026, 11, 1, 0, 0, 0)),
            "Monthly AI budget of $1.00 reached; resets 2026-11-01."
        );
    }
}

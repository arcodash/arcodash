//! `GET /api/organization/ai/usage` — aggregate-only usage metering for AI-provider calls
//! (org-scoped, admin-only).
//!
//! **No handler here ever returns an individual `ai_usage` row** — every query below is a `GROUP
//! BY` rollup (totals, per-provider, per-feature, per-user, per-day-per-status); the raw table is
//! operator-side storage only. [`arcodash_core::ai::usage::record_usage`] is the one write path
//! (called from `ai_query.rs::run_fallback_chain` after every real provider call, shared by every
//! AI feature — `generate_sql` passes `feature = "sql_generation"`, `ai_viz.rs`'s Auto Visualize
//! passes `"auto_visualize"`); the rollups in [`get_usage`] (plus the month-to-date spend sum) are
//! the only reads.

use axum::extract::Query;
use sea_orm::{DatabaseBackend, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};

use arcodash_core::ai::{budget, settings};

use crate::auth::{CurrentOrg, CurrentUser};
use crate::db::Db;
use crate::error::ApiError;
use crate::extract::Json;

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    #[serde(default)]
    pub days: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct UsageResponse {
    pub days: i64,
    pub totals: Totals,
    pub by_provider: Vec<ProviderUsage>,
    pub by_feature: Vec<FeatureUsage>,
    pub by_user: Vec<UserUsage>,
    pub by_day: Vec<DayStatusCount>,
    pub budget: BudgetInfo,
}

/// Calendar-month (UTC) spend vs. the org cap; independent of the `days` window.
#[derive(Debug, Serialize)]
pub struct BudgetInfo {
    pub monthly_usd: Option<f64>,
    pub spent_month_usd: f64,
    /// RFC3339 start of next month (UTC).
    pub resets_at: String,
}

#[derive(Debug, Serialize, FromQueryResult)]
pub struct UserUsage {
    /// `None` for system-originated calls; `email`/`name` are `None` too for deleted users.
    pub user_id: Option<i64>,
    pub email: Option<String>,
    pub name: Option<String>,
    pub calls: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Default, Serialize)]
pub struct Totals {
    pub total_calls: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Fraction (0.0–1.0) of calls with `status = 'ok'`. `0.0` when `total_calls == 0`.
    pub success_rate: f64,
    /// `None` when no call in scope was priced (distinct from a real 0).
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct ProviderUsage {
    pub provider: String,
    pub calls: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Fraction (0.0–1.0) of this provider's calls with `status != 'ok'`.
    pub error_rate: f64,
    /// `None` when no call in scope was priced (distinct from a real 0).
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct FeatureUsage {
    pub feature: String,
    pub calls: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Fraction (0.0–1.0) of this feature's calls with `status != 'ok'`.
    pub error_rate: f64,
    /// `None` when no call in scope was priced (distinct from a real 0).
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct DayStatusCount {
    /// `YYYY-MM-DD`, UTC — day-bucketed, never an individual call timestamp.
    pub date: String,
    pub status: String,
    pub count: i64,
    /// `None` when no call in scope was priced (distinct from a real 0).
    pub cost_usd: Option<f64>,
}

#[derive(Debug, FromQueryResult)]
struct TotalsRow {
    total_calls: i64,
    tokens_in: i64,
    tokens_out: i64,
    ok_calls: i64,
    cost_usd: Option<f64>,
}

/// One `GROUP BY` bucket of [`breakdown`]; `name` is the grouped column's value.
#[derive(Debug, FromQueryResult)]
struct BreakdownRow {
    name: String,
    calls: i64,
    tokens_in: i64,
    tokens_out: i64,
    error_calls: i64,
    cost_usd: Option<f64>,
}

#[derive(Debug, FromQueryResult)]
struct DayStatusRow {
    day: chrono::NaiveDate,
    status: String,
    count: i64,
    cost_usd: Option<f64>,
}

/// `total > 0 ? part / total : 0.0` — shared by the totals success rate and each provider's error
/// rate, both "fraction of calls matching a status filter" with the same empty-org edge case.
#[allow(
    clippy::cast_precision_loss,
    reason = "call counts stay far below 2^52, so the f64 ratio is exact enough"
)]
fn rate(part: i64, total: i64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64
    }
}

/// Per-`column` rollup since `since`, ordered by `order_by`. Both are fixed literals at every call
/// site (never request input), so formatting them into the SQL is safe.
async fn breakdown(
    db: &Db,
    column: &str,
    order_by: &str,
    org_id: i64,
    since: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<BreakdownRow>, ApiError> {
    Ok(
        BreakdownRow::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            format!(
                r"
        SELECT
            {column} AS name,
            COUNT(*) AS calls,
            COALESCE(SUM(tokens_in), 0)::bigint AS tokens_in,
            COALESCE(SUM(tokens_out), 0)::bigint AS tokens_out,
            COUNT(*) FILTER (WHERE status != 'ok') AS error_calls,
            SUM(cost_usd)::double precision AS cost_usd
        FROM ai_usage
        WHERE org_id = $1 AND created_at >= $2
        GROUP BY {column}
        ORDER BY {order_by}
        "
            ),
            [org_id.into(), since.into()],
        ))
        .all(db)
        .await?,
    )
}

/// `GET /api/organization/ai/usage?days=<n>` — admin-only. `days` defaults to 30, clamped to
/// 1..=365 (not restricted to the UI's preset 7/30/90 — any lookback window in range is valid).
pub async fn get_usage(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Query(q): Query<UsageQuery>,
) -> Result<Json<UsageResponse>, ApiError> {
    super::require_admin(&db, &caller.0).await?;

    let days = q.days.unwrap_or(30).clamp(1, 365);
    let since = chrono::Utc::now() - chrono::Duration::days(days);

    let totals_row = TotalsRow::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        r"
        SELECT
            COUNT(*) AS total_calls,
            COALESCE(SUM(tokens_in), 0)::bigint AS tokens_in,
            COALESCE(SUM(tokens_out), 0)::bigint AS tokens_out,
            COUNT(*) FILTER (WHERE status = 'ok') AS ok_calls,
            SUM(cost_usd)::double precision AS cost_usd
        FROM ai_usage
        WHERE org_id = $1 AND created_at >= $2
        ",
        [org.id.into(), since.into()],
    ))
    .one(&db)
    .await?;

    let provider_rows = breakdown(&db, "provider", "calls DESC", org.id, since).await?;
    let feature_rows = breakdown(&db, "feature", "calls DESC, feature ASC", org.id, since).await?;

    let day_status_rows = DayStatusRow::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        r"
        SELECT
            (created_at AT TIME ZONE 'UTC')::date AS day,
            status,
            COUNT(*) AS count,
            SUM(cost_usd)::double precision AS cost_usd
        FROM ai_usage
        WHERE org_id = $1 AND created_at >= $2
        GROUP BY day, status
        ORDER BY day ASC, status ASC
        ",
        [org.id.into(), since.into()],
    ))
    .all(&db)
    .await?;

    let by_user = UserUsage::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        r"
        SELECT
            a.user_id,
            u.email,
            u.name,
            COUNT(*) AS calls,
            COALESCE(SUM(a.tokens_in), 0)::bigint AS tokens_in,
            COALESCE(SUM(a.tokens_out), 0)::bigint AS tokens_out,
            SUM(a.cost_usd)::double precision AS cost_usd
        FROM ai_usage a
        LEFT JOIN users u ON u.id = a.user_id
        WHERE a.org_id = $1 AND a.created_at >= $2
        GROUP BY a.user_id, u.email, u.name
        ORDER BY SUM(a.cost_usd) DESC NULLS LAST, COUNT(*) DESC
        ",
        [org.id.into(), since.into()],
    ))
    .all(&db)
    .await?;

    let budget = BudgetInfo {
        monthly_usd: settings::monthly_budget_usd(&org.settings),
        spent_month_usd: budget::spent_this_month(&db, org.id).await?,
        resets_at: budget::month_window(chrono::Utc::now()).1.to_rfc3339(),
    };

    let totals = totals_row
        .map(|r| Totals {
            total_calls: r.total_calls,
            tokens_in: r.tokens_in,
            tokens_out: r.tokens_out,
            success_rate: rate(r.ok_calls, r.total_calls),
            cost_usd: r.cost_usd,
        })
        .unwrap_or_default();

    let by_provider = provider_rows
        .into_iter()
        .map(|r| ProviderUsage {
            provider: r.name,
            calls: r.calls,
            tokens_in: r.tokens_in,
            tokens_out: r.tokens_out,
            error_rate: rate(r.error_calls, r.calls),
            cost_usd: r.cost_usd,
        })
        .collect();

    let by_feature = feature_rows
        .into_iter()
        .map(|r| FeatureUsage {
            feature: r.name,
            calls: r.calls,
            tokens_in: r.tokens_in,
            tokens_out: r.tokens_out,
            error_rate: rate(r.error_calls, r.calls),
            cost_usd: r.cost_usd,
        })
        .collect();

    let by_day = day_status_rows
        .into_iter()
        .map(|r| DayStatusCount {
            date: r.day.format("%Y-%m-%d").to_string(),
            status: r.status,
            count: r.count,
            cost_usd: r.cost_usd,
        })
        .collect();

    Ok(Json(UsageResponse {
        days,
        totals,
        by_provider,
        by_feature,
        by_user,
        by_day,
        budget,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_is_zero_for_an_empty_org() {
        assert_eq!(rate(0, 0), 0.0);
    }

    #[test]
    fn rate_computes_the_expected_fraction() {
        assert_eq!(rate(9, 12), 0.75);
        assert_eq!(rate(0, 5), 0.0);
        assert_eq!(rate(5, 5), 1.0);
    }
}

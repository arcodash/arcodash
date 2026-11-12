//! Monitor business logic: CRUD, mute/unmute, subscriptions.
//!
//! Every read/mutation goes through `authz::authorize::authorize`: capability AND reach;
//! update/delete/mute need `edit_monitor` AND Editor-or-above reach.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, FromQueryResult,
    Order, QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait,
};
use serde_json::{json, Value};

use arcodash_core::baseline::BASELINE_NEED;
use arcodash_core::entities::{
    access_permission, data_source_group, escalation_step, group, monitor, monitor_subscription,
    notification_delivery, notification_destination, query, user,
};
use arcodash_core::events::record_event;
use arcodash_core::notify::{enqueue_send_notification, SendNotificationParams};
use arcodash_core::queue::Queue;
use arcodash_core::Conveyor;

use crate::authz::authorize::{self, Action, MonitorVerb};
use crate::authz::permissions::load_effective_groups;
use crate::db::Db;
use crate::error::{is_unique_violation, ApiError};
use crate::services::query_service;

async fn authorize_monitor(
    db: &impl ConnectionTrait,
    user: &user::Model,
    verb: MonitorVerb,
    id: i64,
) -> Result<(), ApiError> {
    authorize::authorize(db, user, Action::Monitor { verb, id }).await?;
    Ok(())
}

/// Fetch a monitor by id, org-scoped. `user` must see both the underlying query and the monitor
/// itself (`view_monitor` capability AND reach; no `is_draft`, default visibility `workspace`).
pub async fn get_for_user(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    id: i64,
) -> Result<monitor::Model, ApiError> {
    let a = monitor::Entity::find_by_id(id)
        .one(db)
        .await?
        .filter(|a| a.org_id == org_id)
        .ok_or_else(|| ApiError::not_found("monitor not found"))?;

    // Remap so a viewer without query reach gets a monitor-scoped denial that doesn't name the
    // query.
    query_service::get_for_user(db, org_id, user, a.query_id)
        .await
        .map_err(|e| ApiError::new(e.status, "you do not have access to this monitor's data"))?;

    // Check the monitor's own visibility/grants, gated on view_monitor too.
    authorize_monitor(db, user, MonitorVerb::View, a.id).await?;

    Ok(a)
}

/// `scope`: `by-me`/`shared-with-me` narrow the result (as in `query_service::list_for_user`);
/// anything else is "all".
///
/// `query_id`, `data_source_id` and `ids` narrow to one query, one data source, or an exact id set.
/// The full visibility fold still runs, so an unseen id is silently absent.
pub async fn list_for_user(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    scope: Option<&str>,
    query_id: Option<i64>,
    data_source_id: Option<i64>,
    ids: Option<&[i64]>,
) -> Result<Vec<(monitor::Model, query::Model)>, ApiError> {
    let candidate_cap: u64 =
        arcodash_core::config::env_parsed("LIST_MONITORS_MAX_CANDIDATES", 2000);
    let mut q = monitor::Entity::find().filter(monitor::Column::OrgId.eq(org_id));
    if let Some(qid) = query_id {
        q = q.filter(monitor::Column::QueryId.eq(qid));
    }
    if let Some(ids) = ids {
        q = q.filter(monitor::Column::Id.is_in(ids.to_vec()));
    }
    let monitors = q
        .order_by(monitor::Column::Id, Order::Desc)
        .limit(candidate_cap)
        .all(db)
        .await?;
    if monitors.len() as u64 == candidate_cap {
        tracing::warn!(
            org_id,
            candidate_cap,
            "monitor_service::list_for_user: candidate monitor load hit its cap; some monitors \
             may be missing from this listing"
        );
    }

    let groups = load_effective_groups(db, user).await?;
    let admin = crate::authz::permissions::is_admin(&groups);
    // A role without `view_monitor` never lists monitors: capability is the ceiling.
    if !admin && !crate::authz::permissions::user_has_permission(user, &groups, "view_monitor") {
        return Ok(Vec::new());
    }

    // Batch-load grants for the monitor's own visibility gate (no N+1).
    let ids: Vec<i64> = monitors.iter().map(|m| m.id).collect();
    let grants = access_permission::Entity::find()
        .filter(access_permission::Column::ObjectType.eq("monitor"))
        .filter(access_permission::Column::ObjectId.is_in(ids))
        .all(db)
        .await?;

    let visible_monitors: Vec<monitor::Model> = monitors
        .into_iter()
        .filter(|a| {
            admin
                || crate::authz::access::object_role(
                    user.id,
                    &user.groups,
                    admin,
                    a.user_id,
                    "monitor",
                    a.id,
                    &a.visibility,
                    false, // monitors have no is_draft column — visibility + grants only
                    &grants,
                )
                .is_some()
        })
        .collect();

    // Batch the per-row underlying-query visibility check: one fetch each for queries, grants and
    // data-source groups, then a pure fold per row.
    let query_ids: Vec<i64> = visible_monitors.iter().map(|a| a.query_id).collect();
    let visible_queries =
        batch_visible_queries(db, org_id, user, &groups, admin, &query_ids).await?;

    let result: Vec<(monitor::Model, query::Model)> = visible_monitors
        .into_iter()
        .filter_map(|a| {
            let q = visible_queries
                .get(&a.query_id)
                .filter(|q| data_source_id.is_none_or(|dsid| q.data_source_id == dsid))?;
            Some((a, q.clone()))
        })
        .collect();

    Ok(match scope {
        Some("by-me") => result
            .into_iter()
            .filter(|(m, _)| m.user_id == user.id)
            .collect(),
        Some("shared-with-me") => result
            .into_iter()
            .filter(|(m, _)| {
                m.user_id != user.id
                    && crate::authz::access::has_any_grant(
                        user.id,
                        &user.groups,
                        "monitor",
                        m.id,
                        &grants,
                    )
            })
            .collect(),
        _ => result,
    })
}

/// Batched `query_service::get_for_user` visibility check over many query ids; a missing id is the
/// same fail-closed answer a per-id failure gives.
async fn batch_visible_queries(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    groups: &[group::Model],
    is_admin: bool,
    query_ids: &[i64],
) -> Result<HashMap<i64, query::Model>, ApiError> {
    if query_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let queries = query::Entity::find()
        .filter(query::Column::Id.is_in(query_ids.to_vec()))
        .filter(query::Column::OrgId.eq(org_id))
        .all(db)
        .await?;
    if is_admin {
        return Ok(queries.into_iter().map(|q| (q.id, q)).collect());
    }
    // A role without `view_query` never sees any query: capability is the ceiling.
    if !crate::authz::permissions::user_has_permission(user, groups, "view_query") {
        return Ok(HashMap::new());
    }

    let ids: Vec<i64> = queries.iter().map(|q| q.id).collect();
    let grants = access_permission::Entity::find()
        .filter(access_permission::Column::ObjectType.eq("query"))
        .filter(access_permission::Column::ObjectId.is_in(ids))
        .all(db)
        .await?;
    let dsgs = data_source_group::Entity::find()
        .filter(data_source_group::Column::GroupId.is_in(user.groups.clone()))
        .all(db)
        .await?;
    let postures = crate::authz::data_source::postures(groups);

    Ok(queries
        .into_iter()
        .filter(|q| {
            crate::authz::access::explicit_role(
                user.id,
                &user.groups,
                false,
                q.user_id,
                "query",
                q.id,
                &grants,
            )
            .is_some()
                || (crate::authz::access::object_role(
                    user.id,
                    &user.groups,
                    false,
                    q.user_id,
                    "query",
                    q.id,
                    &q.visibility,
                    !crate::authz::access::query_is_published(q),
                    &grants,
                )
                .is_some()
                    && crate::authz::data_source::in_scope(&postures, q.data_source_id, &dsgs))
        })
        .map(|q| (q.id, q))
        .collect())
}

/// One monitor's dashboard/embed tile: state plus the condition and last observed cell. No authz of
/// its own; callers gate visibility first.
#[derive(serde::Serialize, Clone, Debug)]
pub struct MonitorTile {
    pub id: i64,
    pub name: String,
    pub state: String,
    pub column: Option<String>,
    pub op: Option<String>,
    pub value: Option<f64>,
    /// The configured column's cell from the newest terminal run, raw; `None` if unconfigured,
    /// offloaded, no terminal run, or the column is absent.
    pub observed: Option<Value>,
    pub last_checked_at: Option<DateTime<Utc>>,
    /// Currently silenced (permanently muted or snoozed), per `MonitorOptions::is_silenced`.
    pub muted: bool,
    /// `"threshold"` or `"baseline"`.
    pub kind: String,
    pub learning: Option<Learning>,
    /// First line of the newest non-null observation explanation while `state == "triggered"`.
    pub incident_summary: Option<String>,
    /// Newest terminal run is `failed`/`timed_out` — the check itself is broken, whatever `state`
    /// says.
    pub check_failed: bool,
    /// That run's error text; `Some` only when `check_failed`. Never exposed on public embeds.
    pub last_check_error: Option<String>,
}

/// Baseline warm-up progress: non-null observations collected vs. the number needed to alert.
#[derive(serde::Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Learning {
    pub have: u32,
    pub need: u32,
}

#[derive(Debug, FromQueryResult)]
struct ObservationSummaryRow {
    monitor_id: i64,
    have: i64,
    newest_explanation: Option<String>,
}

/// Per-monitor observation summary, one batched query for the whole list.
#[derive(Debug, Default, Clone)]
pub struct ObservationSummary {
    pub have: u32,
    pub newest_explanation: Option<String>,
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "the count is clamped to 0..=BASELINE_NEED, a small constant"
)]
pub async fn observation_summaries(
    db: &impl ConnectionTrait,
    monitor_ids: &[i64],
) -> Result<HashMap<i64, ObservationSummary>, ApiError> {
    if monitor_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = ObservationSummaryRow::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        r"
        SELECT monitor_id,
               COUNT(*) FILTER (WHERE value IS NOT NULL) AS have,
               (ARRAY_AGG(explanation ORDER BY observed_at DESC, id DESC) FILTER (WHERE explanation IS NOT NULL))[1] AS newest_explanation
        FROM monitor_observations
        WHERE monitor_id = ANY($1)
        GROUP BY monitor_id
        ",
        [monitor_ids.to_vec().into()],
    ))
    .all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.monitor_id,
                ObservationSummary {
                    have: r.have.clamp(0, BASELINE_NEED as i64) as u32,
                    newest_explanation: r.newest_explanation,
                },
            )
        })
        .collect())
}

/// `(kind, learning)` for a monitor given its options and observation summary.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "BASELINE_NEED is a small constant that fits in u32"
)]
pub fn kind_and_learning(
    options: &Value,
    id: i64,
    summary: Option<&ObservationSummary>,
) -> (String, Option<Learning>) {
    use arcodash_core::models::MonitorKind;
    let baseline = arcodash_core::models::parse_monitor_options(id, options).kind
        == Some(MonitorKind::Baseline);
    if !baseline {
        return ("threshold".into(), None);
    }
    let have = summary.map_or(0, |s| s.have);
    let learning = (have < BASELINE_NEED as u32).then_some(Learning {
        have,
        need: BASELINE_NEED as u32,
    });
    ("baseline".into(), learning)
}

/// Raw shape of [`tile_data`]'s join, one row per requested monitor.
#[derive(Debug, FromQueryResult)]
struct TileRow {
    monitor_id: i64,
    finished_at: Option<sea_orm::prelude::DateTimeWithTimeZone>,
    row0: Option<Value>,
    run_status: Option<String>,
    run_error: Option<String>,
    obs_at: Option<sea_orm::prelude::DateTimeWithTimeZone>,
    obs_value: Option<f64>,
    use_obs: Option<bool>,
}

/// Enrichment for the monitor-tile widget in one round trip: state, condition, and the first-row
/// cell from the newest evidence: the latest observation's result when it outranks the last
/// terminal run (runs that failed or produced a result win ties and older observations), else the
/// run's. No authz; callers gate visibility first.
pub async fn tile_data(
    db: &impl ConnectionTrait,
    monitors: &[monitor::Model],
) -> Result<HashMap<i64, MonitorTile>, ApiError> {
    let mut out = HashMap::with_capacity(monitors.len());
    if monitors.is_empty() {
        return Ok(out);
    }

    let ids: Vec<i64> = monitors.iter().map(|m| m.id).collect();
    let rows = TileRow::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        r"
        SELECT
            m.id AS monitor_id,
            lr.finished_at AS finished_at,
            qr.data->'rows'->0 AS row0,
            lr.status AS run_status,
            lr.error AS run_error,
            lo.observed_at AS obs_at,
            lo.value AS obs_value,
            f.use_obs AS use_obs
        FROM monitors m
        LEFT JOIN LATERAL (
            SELECT r.finished_at, r.query_result_id, r.status, r.error
            FROM runs r
            WHERE r.entity_type = 'monitor'
              AND r.entity_id = m.id
              AND r.status IN ('success', 'failed', 'timed_out', 'cancelled', 'skipped')
            ORDER BY r.id DESC
            LIMIT 1
        ) lr ON TRUE
        LEFT JOIN LATERAL (
            SELECT o.observed_at, o.value, o.query_result_id
            FROM monitor_observations o
            WHERE o.monitor_id = m.id
            ORDER BY o.observed_at DESC, o.id DESC
            LIMIT 1
        ) lo ON TRUE
        LEFT JOIN LATERAL (
            SELECT lo.observed_at IS NOT NULL AND (
                lr.status IS NULL
                OR NOT (lr.status IN ('failed', 'timed_out') OR lr.query_result_id IS NOT NULL)
                OR lr.finished_at IS NULL
                OR lo.observed_at > lr.finished_at
            ) AS use_obs
        ) f ON TRUE
        LEFT JOIN query_results qr
            ON qr.id = CASE WHEN f.use_obs THEN lo.query_result_id ELSE lr.query_result_id END
        WHERE m.id = ANY($1)
        ",
        [ids.clone().into()],
    ))
    .all(db)
    .await?;
    let by_id: HashMap<i64, TileRow> = rows.into_iter().map(|r| (r.monitor_id, r)).collect();
    let summaries = observation_summaries(db, &ids).await?;

    for m in monitors {
        let column = m.options["column"].as_str().map(String::from);
        let op = m.options["op"].as_str().map(String::from);
        let value = m.options["value"].as_f64();
        let row = by_id.get(&m.id);
        // The SQL decides whether the newest observation outranks the last run (`use_obs`); row0
        // is then that observation's result, else the run's.
        // A successful check writes its observation after the run finalizes.
        let fresh_obs = row
            .filter(|r| r.use_obs == Some(true))
            .and_then(|r| r.obs_at);
        // The stored f64 is a fallback for when there is no result row to read the cell from.
        let observed = match row.and_then(|r| r.row0.as_ref()) {
            Some(row0) => column.as_deref().and_then(|col| row0.get(col).cloned()),
            None => fresh_obs.and_then(|_| {
                row.and_then(|r| r.obs_value)
                    .and_then(serde_json::Number::from_f64)
                    .map(Value::Number)
            }),
        };
        let last_checked_at = fresh_obs
            .or(row.and_then(|r| r.finished_at))
            .map(|t| t.with_timezone(&Utc));
        let muted =
            arcodash_core::models::parse_monitor_options(m.id, &m.options).is_silenced(Utc::now());
        let summary = summaries.get(&m.id);
        let (kind, learning) = kind_and_learning(&m.options, m.id, summary);
        let incident_summary = (m.state == "triggered")
            .then(|| summary.and_then(|s| s.newest_explanation.as_deref()))
            .flatten()
            .and_then(|e| e.lines().next())
            .map(String::from);
        let check_failed = fresh_obs.is_none()
            && row
                .and_then(|r| r.run_status.as_deref())
                .is_some_and(|s| matches!(s, "failed" | "timed_out"));
        let last_check_error = check_failed
            .then(|| row.and_then(|r| r.run_error.clone()))
            .flatten();
        out.insert(
            m.id,
            MonitorTile {
                id: m.id,
                name: m.name.clone(),
                state: m.state.clone(),
                column,
                op,
                value,
                observed,
                last_checked_at,
                muted,
                kind,
                learning,
                incident_summary,
                check_failed,
                last_check_error,
            },
        );
    }

    Ok(out)
}

/// Orgs whose `create` runs a deliberately failing statement after the monitor insert, to force a
/// mid-transaction DB error in tests.
#[cfg(test)]
static FAIL_SCHEDULE_ORGS: std::sync::Mutex<Vec<i64>> = std::sync::Mutex::new(Vec::new());

pub async fn create(
    db: &(impl ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    org_id: i64,
    user: &user::Model,
    query_id: i64,
    name: String,
    options: Value,
    rearm: Option<i32>,
) -> Result<monitor::Model, ApiError> {
    let groups = load_effective_groups(db, user).await?;
    if !crate::authz::permissions::user_has_permission(user, &groups, "create_monitor") {
        return Err(ApiError::forbidden("missing the create_monitor permission"));
    }

    let target_query = query_service::get_for_user(db, org_id, user, query_id).await?;
    query_service::require_published(&target_query)?;

    validate_options(&options)?;

    // `muted`/`muted_until` are system-owned and stripped here too, so a client can't create an
    // already-muted monitor.
    let mut options = options;
    if let Some(obj) = options.as_object_mut() {
        for key in SYSTEM_OWNED_OPTION_KEYS {
            obj.remove(key);
        }
    }

    // Insert, default schedule and audit event land together: a monitor with no schedule silently
    // never checks.
    let txn = db.begin().await?;

    let model = monitor::ActiveModel {
        org_id: Set(org_id),
        user_id: Set(user.id),
        query_id: Set(query_id),
        name: Set(name.clone()),
        options: Set(options),
        state: Set("unknown".to_string()),
        rearm: Set(rearm),
        last_triggered_at: Set(None),
        ..Default::default()
    };
    let created = model.insert(&txn).await?;

    #[cfg(test)]
    if FAIL_SCHEDULE_ORGS.lock().unwrap().contains(&org_id) {
        txn.execute_unprepared("SELECT 1/0").await?;
    }
    crate::services::schedule_service::create_default_monitor_schedule(&txn, org_id, created.id)
        .await?;

    record_event(
        &txn,
        org_id,
        Some(user.id),
        "monitor.created",
        Some("monitor"),
        Some(created.id),
        json!({ "name": name }),
    )
    .await?;

    txn.commit().await?;

    Ok(created)
}

/// `muted`/`muted_until` are written only by [`set_muted`] and [`snooze`]; stripped from caller
/// options at [`create`] and [`update_query`] so a stale blob can't clear a mute or create a muted
/// monitor.
const SYSTEM_OWNED_OPTION_KEYS: [&str; 2] = ["muted", "muted_until"];

/// Builds the update statement for a monitor patch. `options` is merged into the live row (a JSON
/// `null` deletes a key, others merge via `||`) so a stale snapshot can't clobber a concurrent
/// mute/snooze.
#[allow(
    clippy::option_option,
    reason = "absent = leave unchanged, Some(None) = clear, Some(Some(n)) = set"
)]
fn update_query(
    id: i64,
    name: Option<String>,
    rearm: Option<Option<i32>>,
    options: Option<Value>,
) -> sea_orm::UpdateMany<monitor::Entity> {
    let mut q = monitor::Entity::update_many().filter(monitor::Column::Id.eq(id));
    if let Some(n) = name {
        q = q.col_expr(monitor::Column::Name, sea_orm::sea_query::Expr::value(n));
    }
    if let Some(r) = rearm {
        q = q.col_expr(monitor::Column::Rearm, sea_orm::sea_query::Expr::value(r));
    }
    if let Some(mut o) = options {
        if let Some(obj) = o.as_object_mut() {
            for key in SYSTEM_OWNED_OPTION_KEYS {
                obj.remove(key);
            }
            let delete_keys: Vec<String> = obj
                .iter()
                .filter(|(_, v)| v.is_null())
                .map(|(k, _)| k.clone())
                .collect();
            for key in &delete_keys {
                obj.remove(key);
            }
            q = q.col_expr(
                monitor::Column::Options,
                sea_orm::sea_query::Expr::cust_with_values(
                    "(CASE WHEN jsonb_typeof(options) = 'object' THEN options ELSE '{}'::jsonb END \
                     - $2::text[]) || $1::jsonb",
                    [
                        sea_orm::sea_query::Value::from(o),
                        sea_orm::sea_query::Value::from(delete_keys),
                    ],
                ),
            );
        }
    }
    q.col_expr(
        monitor::Column::UpdatedAt,
        sea_orm::sea_query::Expr::value(Utc::now()),
    )
}

/// JSONB merge of `patch` into `monitors.options` against the live row; mute and snooze own
/// disjoint keys and must compose.
fn merge_monitor_options_query(id: i64, patch: Value) -> sea_orm::UpdateMany<monitor::Entity> {
    monitor::Entity::update_many()
        .col_expr(
            monitor::Column::Options,
            sea_orm::sea_query::Expr::cust_with_values(
                "CASE WHEN jsonb_typeof(options) = 'object' THEN options ELSE '{}'::jsonb END \
                 || $1::jsonb",
                [patch],
            ),
        )
        .col_expr(
            monitor::Column::UpdatedAt,
            sea_orm::sea_query::Expr::value(Utc::now()),
        )
        .filter(monitor::Column::Id.eq(id))
}

async fn merge_monitor_options(
    db: &impl ConnectionTrait,
    id: i64,
    patch: Value,
) -> Result<monitor::Model, ApiError> {
    merge_monitor_options_query(id, patch).exec(db).await?;
    monitor::Entity::find_by_id(id)
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("monitor not found"))
}

pub async fn update(
    db: &impl ConnectionTrait,
    user: &user::Model,
    existing: monitor::Model,
    name: Option<String>,
    options: Option<Value>,
    rearm: Option<Option<i32>>,
) -> Result<monitor::Model, ApiError> {
    authorize_monitor(db, user, MonitorVerb::Edit, existing.id).await?;

    if let Some(o) = &options {
        if !o.is_object() {
            return Err(ApiError::bad_request("options must be a JSON object"));
        }
        validate_options(&merge_options(&existing.options, o))?;
    }

    let (org_id, id) = (existing.org_id, existing.id);
    if name.is_some() || rearm.is_some() || options.is_some() {
        update_query(id, name, rearm, options).exec(db).await?;
    }

    let updated = monitor::Entity::find_by_id(id)
        .one(db)
        .await?
        .filter(|m| m.org_id == org_id)
        .ok_or_else(|| ApiError::not_found("monitor not found"))?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "monitor.updated",
        Some("monitor"),
        Some(updated.id),
        json!({ "name": updated.name }),
    )
    .await?;

    Ok(updated)
}

pub async fn delete(
    db: &impl ConnectionTrait,
    user: &user::Model,
    existing: monitor::Model,
) -> Result<(), ApiError> {
    authorize_monitor(db, user, MonitorVerb::Delete, existing.id).await?;
    monitor::Entity::delete_by_id(existing.id).exec(db).await?;

    record_event(
        db,
        existing.org_id,
        Some(user.id),
        "monitor.deleted",
        Some("monitor"),
        Some(existing.id),
        json!({ "name": existing.name }),
    )
    .await?;

    Ok(())
}

pub async fn set_muted(
    db: &impl ConnectionTrait,
    user: &user::Model,
    existing: monitor::Model,
    muted: bool,
) -> Result<monitor::Model, ApiError> {
    authorize_monitor(db, user, MonitorVerb::Edit, existing.id).await?;

    let updated = merge_monitor_options(db, existing.id, json!({ "muted": muted })).await?;

    record_event(
        db,
        existing.org_id,
        Some(user.id),
        if muted {
            "monitor.muted"
        } else {
            "monitor.unmuted"
        },
        Some("monitor"),
        Some(updated.id),
        json!({ "name": existing.name }),
    )
    .await?;

    Ok(updated)
}

/// Longest duration the snooze UI offers (1 day); the API accepts nothing beyond it.
const SNOOZE_MAX_MINUTES: i64 = 24 * 60;

/// Snooze: `minutes > 0` sets `options.muted_until = now + minutes`; `minutes <= 0` clears it.
pub async fn snooze(
    db: &impl ConnectionTrait,
    user: &user::Model,
    existing: monitor::Model,
    minutes: i64,
) -> Result<monitor::Model, ApiError> {
    authorize_monitor(db, user, MonitorVerb::Edit, existing.id).await?;

    let muted_until = if minutes > 0 {
        if minutes > SNOOZE_MAX_MINUTES {
            return Err(ApiError::bad_request(format!(
                "minutes must be at most {SNOOZE_MAX_MINUTES} ({} day(s))",
                SNOOZE_MAX_MINUTES / (24 * 60)
            )));
        }
        // `checked_*` throughout so the arithmetic is total even if the range check were bypassed.
        Some(
            chrono::Duration::try_minutes(minutes)
                .and_then(|d| Utc::now().checked_add_signed(d))
                .ok_or_else(|| ApiError::bad_request("minutes is out of range"))?,
        )
    } else {
        None
    };

    let updated =
        merge_monitor_options(db, existing.id, json!({ "muted_until": muted_until })).await?;

    record_event(
        db,
        existing.org_id,
        Some(user.id),
        if muted_until.is_some() {
            "monitor.snoozed"
        } else {
            "monitor.snooze_cleared"
        },
        Some("monitor"),
        Some(updated.id),
        json!({ "name": existing.name, "minutes": minutes, "until": muted_until }),
    )
    .await?;

    Ok(updated)
}

/// Acknowledge a `triggered` monitor. Visibility-scoped like a view (not owner-or-admin); stops
/// escalation but does not mute.
pub async fn ack(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    monitor_id: i64,
) -> Result<monitor::Model, ApiError> {
    get_for_user(db, org_id, user, monitor_id).await?;

    // One conditional statement instead of load-check-write: the worker CASes `state`/`acked_at` on
    // the same row.
    let result = monitor::Entity::update_many()
        .col_expr(
            monitor::Column::AckedAt,
            sea_orm::sea_query::Expr::value(Utc::now()),
        )
        .col_expr(
            monitor::Column::AckedBy,
            sea_orm::sea_query::Expr::value(user.id),
        )
        .col_expr(
            monitor::Column::UpdatedAt,
            sea_orm::sea_query::Expr::value(Utc::now()),
        )
        .filter(monitor::Column::Id.eq(monitor_id))
        .filter(monitor::Column::OrgId.eq(org_id))
        .filter(monitor::Column::State.eq("triggered"))
        .filter(monitor::Column::AckedAt.is_null())
        .exec(db)
        .await?;

    let updated = monitor::Entity::find_by_id(monitor_id)
        .one(db)
        .await?
        .filter(|m| m.org_id == org_id)
        .ok_or_else(|| ApiError::not_found("monitor not found"))?;
    if result.rows_affected == 0 {
        // The re-read tells "not triggered" from "already acknowledged".
        return Err(ApiError::conflict(if updated.state == "triggered" {
            "monitor already acknowledged"
        } else {
            "monitor is not currently triggered"
        }));
    }

    record_event(
        db,
        org_id,
        Some(user.id),
        "monitor.acknowledged",
        Some("monitor"),
        Some(updated.id),
        json!({}),
    )
    .await?;

    Ok(updated)
}

/// Current escalation chain, ordered by position.
pub async fn get_escalation(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    monitor_id: i64,
) -> Result<Vec<escalation_step::Model>, ApiError> {
    let existing = get_for_user(db, org_id, user, monitor_id).await?;
    Ok(escalation_step::Entity::find()
        .filter(escalation_step::Column::MonitorId.eq(existing.id))
        .order_by_asc(escalation_step::Column::Position)
        .all(db)
        .await?)
}

/// Replace the whole escalation chain with `steps` (positions 1..N from array order; empty clears).
///
/// Every `destination_id` must belong to the caller's org. Delete-then-insert runs in one
/// transaction so a crash can't leave an incident with no chain.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "escalation chains are a handful of steps, far below i32::MAX"
)]
pub async fn put_escalation(
    db: &(impl ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    org_id: i64,
    user: &user::Model,
    monitor_id: i64,
    steps: Vec<(i32, i64)>,
) -> Result<Vec<escalation_step::Model>, ApiError> {
    let existing = get_for_user(db, org_id, user, monitor_id).await?;
    authorize_monitor(db, user, MonitorVerb::Edit, existing.id).await?;

    for (delay_minutes, destination_id) in &steps {
        if *delay_minutes < 1 {
            return Err(ApiError::bad_request("delay_minutes must be at least 1"));
        }
        notification_destination::Entity::find_by_id(*destination_id)
            .one(db)
            .await?
            .filter(|d| d.org_id == org_id)
            .ok_or_else(|| {
                ApiError::bad_request(format!("destination {destination_id} not found"))
            })?;
    }

    let txn = db.begin().await?;

    escalation_step::Entity::delete_many()
        .filter(escalation_step::Column::MonitorId.eq(existing.id))
        .exec(&txn)
        .await?;

    let now = Utc::now();
    let mut inserted = Vec::with_capacity(steps.len());
    for (i, (delay_minutes, destination_id)) in steps.into_iter().enumerate() {
        let model = escalation_step::ActiveModel {
            org_id: Set(org_id),
            monitor_id: Set(existing.id),
            position: Set((i + 1) as i32),
            delay_minutes: Set(delay_minutes),
            destination_id: Set(destination_id),
            created_at: Set(now.into()),
            updated_at: Set(now.into()),
            ..Default::default()
        };
        inserted.push(model.insert(&txn).await?);
    }

    // If the new chain ends before the step an in-flight incident is on, there is nothing left to
    // escalate to; surface that.
    if existing.state == "triggered"
        && existing.acked_at.is_none()
        && (inserted.len() as i32) < existing.escalation_step
    {
        tracing::warn!(
            monitor_id = existing.id,
            escalation_step = existing.escalation_step,
            new_step_count = inserted.len(),
            "put_escalation: shortened chain ends an in-flight, unacked escalation"
        );
    }

    record_event(
        &txn,
        org_id,
        Some(user.id),
        "monitor.escalation_updated",
        Some("monitor"),
        Some(existing.id),
        json!({ "name": existing.name, "step_count": inserted.len() }),
    )
    .await?;

    txn.commit().await?;

    Ok(inserted)
}

pub async fn list_subscriptions(
    db: &impl ConnectionTrait,
    monitor_id: i64,
) -> Result<Vec<monitor_subscription::Model>, ApiError> {
    Ok(monitor_subscription::Entity::find()
        .filter(monitor_subscription::Column::MonitorId.eq(monitor_id))
        .all(db)
        .await?)
}

pub async fn subscribe(
    db: &Db,
    queue: Arc<dyn Queue>,
    conveyor: Option<Arc<Conveyor>>,
    org_id: i64,
    user: &user::Model,
    monitor: &monitor::Model,
    destination_id: Option<i64>,
) -> Result<monitor_subscription::Model, ApiError> {
    // Personal opt-in, but still view-tier: a direct caller must not skip the check `get_for_user`
    // carries.
    authorize_monitor(db, user, MonitorVerb::Subscribe, monitor.id).await?;

    // If a destination is specified, verify it belongs to this org.
    if let Some(did) = destination_id {
        notification_destination::Entity::find_by_id(did)
            .one(db)
            .await?
            .filter(|d| d.org_id == org_id)
            .ok_or_else(|| ApiError::not_found("destination not found"))?;
    }

    let model = monitor_subscription::ActiveModel {
        monitor_id: Set(monitor.id),
        user_id: Set(user.id),
        destination_id: Set(destination_id),
        ..Default::default()
    };
    let sub = model.insert(db).await.map_err(|e| {
        if is_unique_violation(&e) {
            ApiError::conflict("already subscribed to this monitor")
        } else {
            ApiError::from(e)
        }
    })?;

    // Notify-on-subscribe: a `triggered` monitor fires no further transition, so a mid-incident
    // subscriber would get nothing. Only `triggered` auto-notifies. Best-effort: enqueue failure
    // must not fail the subscribe.
    let monitor_options =
        arcodash_core::models::parse_monitor_options(monitor.id, &monitor.options);
    if monitor.state == "triggered" && !monitor_options.is_silenced(Utc::now()) {
        // Insert the durable delivery row BEFORE the LPUSH; failure here must not fail the
        // subscribe.
        let destination_id = sub.destination_id.unwrap_or(-1);
        match notification_delivery::insert_pending(
            db,
            org_id,
            monitor.id,
            Some(sub.id),
            destination_id,
            &monitor.state,
            None,
        )
        .await
        {
            Err(e) => tracing::error!(
                org_id,
                monitor_id = monitor.id,
                subscription_id = sub.id,
                error = %e,
                "failed to pre-create the notify-on-subscribe delivery row"
            ),
            Ok(delivery_id) => {
                // Deferred via `Db::after_commit`: a worker could dequeue before the request
                // transaction commits.
                let new_state = monitor.state.clone();
                let monitor_id = monitor.id;
                let subscriber_id = user.id;
                let subscription_id = Some(sub.id);
                db.after_commit(Box::pin(async move {
                    if let Err(e) = enqueue_send_notification(
                        queue.as_ref(),
                        conveyor.as_deref(),
                        SendNotificationParams {
                            org_id,
                            monitor_id,
                            destination_id,
                            new_state,
                            subscriber_id,
                            subscription_id,
                            attempt: 1,
                            delivery_id: Some(delivery_id),
                            observed: None,
                            explanation: None,
                        },
                    )
                    .await
                    {
                        tracing::error!(
                            org_id,
                            monitor_id,
                            delivery_id,
                            error = %e,
                            "failed to enqueue notification for a committed pending delivery"
                        );
                    }
                }))
                .await;
            }
        }
    }

    Ok(sub)
}

/// Manual "Send now": re-notify one subscription regardless of monitor state.
///
/// Scoped through the parent monitor (`monitor_subscriptions` has no `org_id`). View-tier reach for
/// the owner, Edit-tier for anyone acting on another's subscription.
pub async fn notify_subscription(
    db: &Db,
    queue: Arc<dyn Queue>,
    conveyor: Option<Arc<Conveyor>>,
    org_id: i64,
    user: &user::Model,
    monitor_id: i64,
    sub_id: i64,
) -> Result<(), ApiError> {
    let a = monitor::Entity::find_by_id(monitor_id)
        .one(db)
        .await?
        .filter(|a| a.org_id == org_id)
        .ok_or_else(|| ApiError::not_found("monitor not found"))?;

    let sub = monitor_subscription::Entity::find_by_id(sub_id)
        .one(db)
        .await?
        .filter(|s| s.monitor_id == a.id)
        .ok_or_else(|| ApiError::not_found("subscription not found"))?;

    // Always authorizes, even for the caller's own subscription: reach can have been revoked since.
    let verb = if sub.user_id == user.id {
        MonitorVerb::View
    } else {
        MonitorVerb::Edit
    };
    if let Err(e) = authorize_monitor(db, user, verb, a.id).await {
        crate::metrics::record_monitor_notification("denied");
        return Err(e);
    }

    let destination_id = sub.destination_id.unwrap_or(-1);
    let delivery_id = notification_delivery::insert_pending(
        db,
        org_id,
        a.id,
        Some(sub.id),
        destination_id,
        &a.state,
        None,
    )
    .await
    .map_err(|e| ApiError::internal(format!("failed to record delivery: {e}")))?;

    // Deferred via `Db::after_commit_fallible` (same ordering reason as `subscribe`). "Send now" IS
    // the request, so failure is a real error where one can be returned and loudly logged where
    // not.
    let new_state = a.state.clone();
    let monitor_id = a.id;
    let subscriber_id = user.id;
    let subscription_id = Some(sub.id);
    let result = db
        .after_commit_fallible(Box::pin(async move {
            enqueue_send_notification(
                queue.as_ref(),
                conveyor.as_deref(),
                SendNotificationParams {
                    org_id,
                    monitor_id,
                    destination_id,
                    new_state,
                    subscriber_id,
                    subscription_id,
                    attempt: 1,
                    delivery_id: Some(delivery_id),
                    observed: None,
                    explanation: None,
                },
            )
            .await
            .map_err(|e| ApiError::internal(format!("failed to enqueue notification: {e}")))
        }))
        .await;
    crate::metrics::record_monitor_notification(if result.is_ok() { "sent" } else { "error" });
    result
}

pub async fn unsubscribe(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    sub_id: i64,
) -> Result<(), ApiError> {
    let sub = monitor_subscription::Entity::find_by_id(sub_id)
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("subscription not found"))?;

    // Scope through the parent monitor: `is_admin` is not org-aware, so without this any org's
    // admin could delete another org's subscription by id.
    let parent_monitor = monitor::Entity::find_by_id(sub.monitor_id)
        .filter(monitor::Column::OrgId.eq(org_id))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("subscription not found"))?;

    authorize_monitor(db, user, MonitorVerb::Unsubscribe, parent_monitor.id)
        .await
        .map_err(|_| ApiError::not_found("subscription not found"))?;

    // Only the subscriber (or an admin) may delete.
    if sub.user_id != user.id {
        let groups = load_effective_groups(db, user).await?;
        if !crate::authz::permissions::is_admin(&groups) {
            return Err(ApiError::not_found("subscription not found"));
        }
    }

    monitor_subscription::Entity::delete_by_id(sub_id)
        .exec(db)
        .await?;
    Ok(())
}

/// Mirrors `update_query`'s jsonb merge: patch keys overwrite, `null` deletes.
fn merge_options(existing: &Value, patch: &Value) -> Value {
    let mut merged = existing.as_object().cloned().unwrap_or_default();
    for (k, v) in patch.as_object().into_iter().flatten() {
        if v.is_null() {
            merged.remove(k);
        } else {
            merged.insert(k.clone(), v.clone());
        }
    }
    Value::Object(merged)
}

fn validate_options(options: &Value) -> Result<(), ApiError> {
    if !options.is_object() {
        return Err(ApiError::bad_request("options must be a JSON object"));
    }
    let valid_ops = [">", "<", ">=", "<=", "==", "!="];
    if let Some(op) = options.get("op").and_then(|v| v.as_str()) {
        if !valid_ops.contains(&op) {
            return Err(ApiError::bad_request(format!(
                "invalid op '{op}'; must be one of: {}",
                valid_ops.join(", ")
            )));
        }
    }
    if let Some(secs) = options
        .get("max_fresh_wait_secs")
        .and_then(sea_orm::JsonValue::as_i64)
    {
        if !(0..=7200).contains(&secs) {
            return Err(ApiError::bad_request(
                "max_fresh_wait_secs must be between 0 and 7200 seconds",
            ));
        }
    }
    let present = |k: &str| options.get(k).filter(|v| !v.is_null());
    let kind = present("kind").and_then(|v| v.as_str());
    if present("kind").is_some() && !matches!(kind, Some("threshold" | "baseline")) {
        return Err(ApiError::bad_request(
            "kind must be one of: threshold, baseline",
        ));
    }
    if present("sensitivity").is_some()
        && !matches!(
            present("sensitivity").and_then(|v| v.as_str()),
            Some("low" | "medium" | "high")
        )
    {
        return Err(ApiError::bad_request(
            "sensitivity must be one of: low, medium, high",
        ));
    }
    if kind == Some("baseline") {
        if present("column")
            .and_then(|v| v.as_str())
            .is_none_or(str::is_empty)
        {
            return Err(ApiError::bad_request(
                "baseline monitors require a non-empty column",
            ));
        }
        if present("op").is_some() || present("value").is_some() {
            return Err(ApiError::bad_request(
                "baseline monitors do not take op or value",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{DbBackend, PaginatorTrait, QueryTrait};

    /// Exact SQL shape for an options patch: a single `UPDATE ... options || $1`, never a `SELECT`
    /// first.
    #[test]
    fn update_query_merges_options_into_the_live_row_with_no_select() {
        let sql = update_query(1, None, None, Some(json!({ "muted": true })))
            .build(DbBackend::Postgres)
            .to_string()
            .to_lowercase();
        assert!(sql.starts_with("update"), "sql: {sql}");
        assert!(sql.contains("jsonb_typeof(options)"), "sql: {sql}");
        assert!(sql.contains("|| "), "sql: {sql}");
        assert!(!sql.contains("select"), "must not read-then-write: {sql}");
    }

    /// A JSON `null` in the patch deletes the key (jsonb `-`) rather than merging a literal `null`.
    #[test]
    fn update_query_deletes_null_valued_keys_and_merges_the_rest() {
        let sql = update_query(
            1,
            None,
            None,
            Some(json!({ "max_fresh_wait_secs": 30, "old_key": null })),
        )
        .build(DbBackend::Postgres)
        .to_string();
        assert!(sql.contains("max_fresh_wait_secs"), "sql: {sql}");
        assert!(sql.contains("old_key"), "sql: {sql}");
        assert!(sql.to_lowercase().contains(" - "), "sql: {sql}");
        assert!(sql.to_lowercase().contains(" || "), "sql: {sql}");
    }

    /// `muted`/`muted_until` are the writer's own keys: a patch touching either must never reach
    /// the SQL.
    #[test]
    fn update_query_strips_system_owned_keys_before_merge_or_delete() {
        let sql = update_query(
            1,
            None,
            None,
            Some(json!({ "muted": false, "muted_until": null, "op": ">" })),
        )
        .build(DbBackend::Postgres)
        .to_string();
        assert!(
            !sql.contains("muted"),
            "sql leaked a system-owned key: {sql}"
        );
        // Not `"op"` (quoted): the injected jsonb literal backslash-escapes embedded quotes.
        assert!(sql.contains("op"), "sql: {sql}");
    }

    /// Mute and snooze own disjoint keys, so each merge names only its own key.
    #[test]
    fn merge_monitor_options_query_merges_only_the_given_key_with_no_select() {
        let sql = merge_monitor_options_query(1, json!({ "muted": true }))
            .build(DbBackend::Postgres)
            .to_string()
            .to_lowercase();
        assert!(sql.starts_with("update"), "sql: {sql}");
        assert!(sql.contains("jsonb_typeof(options)"), "sql: {sql}");
        assert!(sql.contains("|| "), "sql: {sql}");
        assert!(!sql.contains("select"), "must not read-then-write: {sql}");

        let sql = merge_monitor_options_query(1, json!({ "muted_until": null }))
            .build(DbBackend::Postgres)
            .to_string()
            .to_lowercase();
        assert!(sql.contains("jsonb_typeof(options)"), "sql: {sql}");
    }

    #[test]
    fn snooze_bound_rejects_i64_max_without_panicking() {
        // The arithmetic `snooze` uses, at a value that would panic `chrono::Duration::minutes`
        // without the bound check.
        assert!(chrono::Duration::try_minutes(i64::MAX).is_none());
    }

    #[test]
    fn snooze_bound_accepts_the_maximum_allowed_value() {
        let until = chrono::Duration::try_minutes(SNOOZE_MAX_MINUTES)
            .and_then(|d| Utc::now().checked_add_signed(d));
        assert!(until.is_some());
    }

    #[test]
    fn validate_options_accepts_all_valid_ops() {
        for op in [">", "<", ">=", "<=", "==", "!="] {
            let opts = json!({ "op": op, "value": 1 });
            assert!(
                validate_options(&opts).is_ok(),
                "op {op:?} should be accepted",
            );
        }
    }

    #[test]
    fn validate_options_rejects_unknown_op() {
        let opts = json!({ "op": "=>", "value": 1 });
        let err = validate_options(&opts).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        // Message must surface the offending op and the full list of valid ops.
        assert!(err.message.contains("=>"), "message: {}", err.message);
        for valid in [">", "<", ">=", "<=", "==", "!="] {
            assert!(
                err.message.contains(valid),
                "message should list valid op {valid:?}: {}",
                err.message,
            );
        }
    }

    #[test]
    fn validate_options_rejects_non_object_values() {
        for opts in [json!([]), json!("x"), json!(null), json!(5)] {
            let err = validate_options(&opts).unwrap_err();
            assert_eq!(
                err.status,
                axum::http::StatusCode::BAD_REQUEST,
                "opts: {opts}"
            );
        }
        assert!(validate_options(&json!({})).is_ok());
    }

    #[test]
    fn merge_then_validate_allows_nulling_op_and_value() {
        let existing = json!({ "column": "x", "op": ">", "value": 1 });
        let patch = json!({ "kind": "baseline", "op": null, "value": null });
        assert!(validate_options(&merge_options(&existing, &patch)).is_ok());
    }

    #[test]
    fn validate_options_no_op_key_is_ok() {
        // op is optional — options without it (or with a non-string op) must pass.
        assert!(validate_options(&json!({ "value": 5 })).is_ok());
        assert!(validate_options(&json!({})).is_ok());
        assert!(validate_options(&json!({ "op": 5 })).is_ok());
    }

    #[test]
    fn validate_options_accepts_max_fresh_wait_secs_in_range_or_absent() {
        assert!(validate_options(&json!({ "max_fresh_wait_secs": 0 })).is_ok());
        assert!(validate_options(&json!({ "max_fresh_wait_secs": 7200 })).is_ok());
        assert!(validate_options(&json!({ "max_fresh_wait_secs": 300 })).is_ok());
        assert!(
            validate_options(&json!({})).is_ok(),
            "missing key means always-wait"
        );
        assert!(
            validate_options(&json!({ "max_fresh_wait_secs": null })).is_ok(),
            "explicit null means always-wait"
        );
    }

    #[test]
    fn validate_options_rejects_max_fresh_wait_secs_out_of_range() {
        let err = validate_options(&json!({ "max_fresh_wait_secs": 7201 })).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.message.contains("max_fresh_wait_secs"));

        let err = validate_options(&json!({ "max_fresh_wait_secs": -1 })).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validate_options_baseline_requires_column() {
        for opts in [
            json!({ "kind": "baseline" }),
            json!({ "kind": "baseline", "column": "" }),
            json!({ "kind": "baseline", "column": 5 }),
        ] {
            assert!(validate_options(&opts).is_err(), "opts: {opts}");
        }
        assert!(validate_options(&json!({ "kind": "baseline", "column": "n" })).is_ok());
        assert!(validate_options(
            &json!({ "kind": "baseline", "column": "n", "op": null, "value": null })
        )
        .is_ok());
    }

    #[test]
    fn validate_options_baseline_rejects_op_value() {
        assert!(
            validate_options(&json!({ "kind": "baseline", "column": "n", "op": ">" })).is_err()
        );
        assert!(
            validate_options(&json!({ "kind": "baseline", "column": "n", "value": 3 })).is_err()
        );
        assert!(validate_options(&json!({ "kind": "threshold", "op": ">", "value": 3 })).is_ok());
    }

    #[test]
    fn validate_options_rejects_unknown_kind_and_sensitivity() {
        assert!(validate_options(&json!({ "kind": "zscore" })).is_err());
        assert!(validate_options(&json!({ "kind": 3 })).is_err());
        assert!(validate_options(&json!({ "sensitivity": "extreme" })).is_err());
        for s in ["low", "medium", "high"] {
            assert!(validate_options(&json!({ "sensitivity": s })).is_ok());
        }
    }

    #[test]
    fn merge_options_overwrites_and_deletes_null_keys() {
        let merged = merge_options(
            &json!({ "op": ">", "value": 1, "column": "a" }),
            &json!({ "kind": "baseline", "op": null }),
        );
        assert_eq!(
            merged,
            json!({ "value": 1, "column": "a", "kind": "baseline" })
        );
    }

    /// An org with one admin-permission group and a user in it.
    async fn seed_org_and_admin(
        db: &sea_orm::DatabaseConnection,
        tag: &str,
    ) -> (arcodash_core::entities::organization::Model, user::Model) {
        use arcodash_core::entities::{group_role, role};
        use sea_orm::ActiveValue::NotSet;

        let mut org = arcodash_core::test_support::new_organization();
        org.slug = Set(format!("mon-svc-{tag}"));
        let org = org.insert(db).await.unwrap();
        // A group's `permissions` column is vestigial: admin capability needs a real role row.
        let admin_group = arcodash_core::test_support::new_group(org.id, "admin", vec![])
            .insert(db)
            .await
            .unwrap();
        let admin_role_id = role::Entity::insert(role::ActiveModel {
            id: NotSet,
            org_id: Set(org.id),
            name: Set(format!("admin-role-{tag}")),
            permissions: Set(vec!["admin".to_string()]),
            r#type: Set("custom".to_string()),
            created_at: NotSet,
            updated_at: NotSet,
        })
        .exec(db)
        .await
        .unwrap()
        .last_insert_id;
        group_role::Entity::insert(group_role::ActiveModel {
            id: NotSet,
            group_id: Set(admin_group.id),
            role_id: Set(admin_role_id),
            created_at: NotSet,
            updated_at: NotSet,
        })
        .exec(db)
        .await
        .unwrap();
        let mut user = arcodash_core::test_support::new_user(org.id);
        user.email = Set(format!("mon-svc-{tag}@test.local"));
        user.api_key_hash = Set(arcodash_core::hash::hash_api_key(&format!("mon-svc-{tag}")));
        user.groups = Set(vec![admin_group.id]);
        let user = user.insert(db).await.unwrap();
        (org, user)
    }

    /// A published query the admin owns, ready to back a monitor.
    async fn seed_published_query(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        owner: &user::Model,
    ) -> query::Model {
        let ds = arcodash_core::test_support::new_data_source(org_id)
            .insert(db)
            .await
            .unwrap();
        let mut q = arcodash_core::test_support::new_query(org_id, ds.id, owner.id);
        q.api_key = Set(uuid::Uuid::new_v4().simple().to_string());
        let q = q.insert(db).await.unwrap();
        query_service::publish(db, owner, q, 0).await.unwrap()
    }

    async fn test_db() -> Option<sea_orm::DatabaseConnection> {
        let Ok(db_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return None;
        };
        Some(arcodash_core::db::connect(&db_url).await.unwrap())
    }

    /// A fresh org with an admin and a published query.
    async fn seed(
        db: &sea_orm::DatabaseConnection,
    ) -> (
        arcodash_core::entities::organization::Model,
        user::Model,
        query::Model,
    ) {
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let (org, admin) = seed_org_and_admin(db, &tag).await;
        let query = seed_published_query(db, org.id, &admin).await;
        (org, admin, query)
    }

    /// A threshold monitor via the real `create` path.
    async fn create_monitor(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        admin: &user::Model,
        query_id: i64,
        name: &str,
    ) -> Result<monitor::Model, ApiError> {
        create(
            db,
            org_id,
            admin,
            query_id,
            name.to_string(),
            json!({ "op": ">", "value": 1 }),
            None,
        )
        .await
    }

    /// Positive control: a normal create leaves exactly one default schedule row.
    #[tokio::test]
    async fn monitor_create_still_produces_exactly_one_schedule_row() {
        let Some(db) = test_db().await else { return };
        let (org, admin, query) = seed(&db).await;

        let created = create_monitor(&db, org.id, &admin, query.id, "Watched")
            .await
            .unwrap();

        let schedule_count = arcodash_core::entities::schedule::Entity::find()
            .filter(arcodash_core::entities::schedule::Column::EntityType.eq("monitor"))
            .filter(arcodash_core::entities::schedule::Column::EntityId.eq(created.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            schedule_count, 1,
            "a normal create must leave exactly one default schedule row"
        );
    }

    /// If the default-schedule insert fails, the monitor row must not survive either. The failure
    /// is injected for this test's org only.
    #[tokio::test]
    async fn monitor_create_rolls_back_when_the_default_schedule_fails() {
        let Some(db) = test_db().await else { return };
        let (org, admin, query) = seed(&db).await;

        struct Registered(i64);
        impl Drop for Registered {
            fn drop(&mut self) {
                FAIL_SCHEDULE_ORGS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|id| *id != self.0);
            }
        }
        FAIL_SCHEDULE_ORGS.lock().unwrap().push(org.id);
        let _registered = Registered(org.id);

        let result = create_monitor(&db, org.id, &admin, query.id, "Blocked").await;
        let err = result.expect_err("create must fail on a DB error after the monitor insert");
        assert_eq!(err.status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);

        let survivors = monitor::Entity::find()
            .filter(monitor::Column::OrgId.eq(org.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            survivors, 0,
            "the monitor insert must roll back together with the failed schedule insert"
        );
    }

    /// `put_escalation`'s pre-checks run before the transaction opens, so one bad destination
    /// leaves the existing chain untouched.
    #[tokio::test]
    async fn put_escalation_leaves_the_existing_chain_intact_when_a_destination_is_invalid() {
        let Some(db) = test_db().await else { return };
        let (org, admin, query) = seed(&db).await;
        let created = create_monitor(&db, org.id, &admin, query.id, "Escalating")
            .await
            .unwrap();

        let key = [7u8; 32];
        let mut destinations = Vec::new();
        for i in 0..2 {
            destinations.push(
                crate::services::destination_service::create(
                    &db,
                    &key,
                    org.id,
                    admin.id,
                    format!("Dest {i}"),
                    "webhook".to_string(),
                    json!({ "url": format!("https://example.test/{i}") }),
                )
                .await
                .unwrap(),
            );
        }

        let first_chain = put_escalation(
            &db,
            org.id,
            &admin,
            created.id,
            vec![(5, destinations[0].id), (10, destinations[1].id)],
        )
        .await
        .unwrap();
        assert_eq!(first_chain.len(), 2);

        let bogus_destination_id = destinations[1].id + 1_000_000;
        let result = put_escalation(
            &db,
            org.id,
            &admin,
            created.id,
            vec![(5, destinations[0].id), (15, bogus_destination_id)],
        )
        .await;
        assert!(
            result.is_err(),
            "an unknown destination id must be rejected"
        );

        let remaining = escalation_step::Entity::find()
            .filter(escalation_step::Column::MonitorId.eq(created.id))
            .order_by_asc(escalation_step::Column::Position)
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            remaining.len(),
            2,
            "a rejected replace must leave the prior chain exactly as it was"
        );
        assert_eq!(remaining[0].destination_id, destinations[0].id);
        assert_eq!(remaining[1].destination_id, destinations[1].id);
    }

    use arcodash_core::entities::query_result;
    use sea_orm::ActiveValue::NotSet;

    async fn insert_monitor(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        query_id: i64,
        user_id: i64,
        options: Value,
        state: &str,
    ) -> monitor::Model {
        monitor::ActiveModel {
            org_id: Set(org_id),
            user_id: Set(user_id),
            query_id: Set(query_id),
            name: Set("Tile monitor".to_string()),
            options: Set(options),
            state: Set(state.to_string()),
            rearm: Set(None),
            last_triggered_at: Set(None),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    async fn insert_query_result(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        data_source_id: i64,
        data: Option<Value>,
    ) -> query_result::Model {
        let now = Utc::now();
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        query_result::ActiveModel {
            org_id: Set(org_id),
            data_source_id: Set(data_source_id),
            query_hash: Set(format!("hash-{suffix}")),
            query: Set("SELECT 1".to_string()),
            data: Set(data),
            data_location: Set(None),
            data_size_bytes: Set(None),
            runtime: Set(Some(0.01)),
            retrieved_at: Set(now.into()),
            last_used_at: Set(now.into()),
            created_at: Set(now.into()),
            updated_at: Set(now.into()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    /// Terminal `runs` row for a monitor.
    async fn insert_run(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        monitor_id: i64,
        status: &str,
        finished_at: DateTime<Utc>,
        query_result_id: Option<i64>,
    ) -> arcodash_core::entities::run::Model {
        arcodash_core::entities::run::ActiveModel {
            id: NotSet,
            org_id: Set(org_id),
            entity_type: Set("monitor".to_string()),
            entity_id: Set(monitor_id),
            schedule_id: Set(None),
            data_source_id: Set(None),
            scheduled_for: Set(None),
            triggered_by: Set("manual".to_string()),
            status: Set(status.to_string()),
            waiting_on_run_id: Set(None),
            job_id: Set(None),
            worker_id: Set(None),
            lease_expires_at: Set(None),
            started_at: Set(Some(finished_at.into())),
            finished_at: Set(Some(finished_at.into())),
            duration_ms: Set(Some(1)),
            row_count: Set(None),
            error: Set(
                (status == "failed").then(|| "connection refused: db.internal:5432".to_string())
            ),
            data_age_secs: Set(None),
            query_result_id: Set(query_result_id),
            condition_met: Set(None),
            created_at: Set(finished_at.into()),
            enqueued_at: NotSet,
        }
        .insert(db)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn tile_data_reports_observed_and_last_checked_from_the_completed_run() {
        let Some(db) = test_db().await else { return };
        let (org, admin, query) = seed(&db).await;
        let monitor = insert_monitor(
            &db,
            org.id,
            query.id,
            admin.id,
            json!({ "column": "value", "op": ">", "value": 5.0 }),
            "triggered",
        )
        .await;
        let qr = insert_query_result(
            &db,
            org.id,
            query.data_source_id,
            Some(json!({ "rows": [{ "value": 12 }] })),
        )
        .await;
        let finished_at = Utc::now();
        insert_run(&db, org.id, monitor.id, "success", finished_at, Some(qr.id)).await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).expect("tile must be present");
        assert_eq!(tile.state, "triggered");
        assert_eq!(tile.column.as_deref(), Some("value"));
        assert_eq!(tile.op.as_deref(), Some(">"));
        assert_eq!(tile.value, Some(5.0));
        assert_eq!(tile.observed, Some(json!(12)));
        assert!(
            tile.last_checked_at
                .is_some_and(|t| (t - finished_at).num_seconds().abs() < 5),
            "last_checked_at: {:?}",
            tile.last_checked_at
        );
        assert!(!tile.muted);
    }

    #[tokio::test]
    async fn tile_data_reports_muted_true_for_a_permanently_muted_monitor() {
        let Some(db) = test_db().await else { return };
        let (org, admin, query) = seed(&db).await;
        let monitor = insert_monitor(
            &db,
            org.id,
            query.id,
            admin.id,
            json!({ "column": "value", "op": ">", "value": 5.0, "muted": true }),
            "triggered",
        )
        .await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert!(tile.muted);
    }

    #[tokio::test]
    async fn tile_data_flags_check_failed_with_error_when_newest_run_failed() {
        let Some(db) = test_db().await else { return };
        let (org, admin, query) = seed(&db).await;
        let monitor = insert_monitor(&db, org.id, query.id, admin.id, json!({}), "unknown").await;
        insert_run(&db, org.id, monitor.id, "success", Utc::now(), None).await;
        insert_run(&db, org.id, monitor.id, "failed", Utc::now(), None).await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert!(tile.check_failed);
        assert_eq!(
            tile.last_check_error.as_deref(),
            Some("connection refused: db.internal:5432")
        );
    }

    #[tokio::test]
    async fn tile_data_check_failed_clears_when_newest_run_succeeded_after_a_failure() {
        let Some(db) = test_db().await else { return };
        let (org, admin, query) = seed(&db).await;
        let monitor = insert_monitor(&db, org.id, query.id, admin.id, json!({}), "ok").await;
        insert_run(&db, org.id, monitor.id, "failed", Utc::now(), None).await;
        insert_run(&db, org.id, monitor.id, "success", Utc::now(), None).await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert!(!tile.check_failed);
        assert_eq!(tile.last_check_error, None);
    }

    #[tokio::test]
    async fn tile_data_picks_the_newer_of_two_runs() {
        let Some(db) = test_db().await else { return };
        let (org, admin, query) = seed(&db).await;
        let monitor = insert_monitor(
            &db,
            org.id,
            query.id,
            admin.id,
            json!({ "column": "value", "op": ">", "value": 5.0 }),
            "ok",
        )
        .await;

        let older_qr = insert_query_result(
            &db,
            org.id,
            query.data_source_id,
            Some(json!({ "rows": [{ "value": 1 }] })),
        )
        .await;
        insert_run(
            &db,
            org.id,
            monitor.id,
            "success",
            Utc::now(),
            Some(older_qr.id),
        )
        .await;

        let newer_qr = insert_query_result(
            &db,
            org.id,
            query.data_source_id,
            Some(json!({ "rows": [{ "value": 99 }] })),
        )
        .await;
        insert_run(
            &db,
            org.id,
            monitor.id,
            "success",
            Utc::now(),
            Some(newer_qr.id),
        )
        .await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert_eq!(
            tile.observed,
            Some(json!(99)),
            "the higher-id (newer) run must win, not the older one"
        );
    }

    #[tokio::test]
    async fn tile_data_with_no_runs_still_returns_a_tile_with_no_observation() {
        let Some(db) = test_db().await else { return };
        let (org, admin, query) = seed(&db).await;
        let monitor = insert_monitor(
            &db,
            org.id,
            query.id,
            admin.id,
            json!({ "column": "value", "op": ">", "value": 5.0 }),
            "unknown",
        )
        .await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert_eq!(tile.state, "unknown");
        assert_eq!(tile.column.as_deref(), Some("value"));
        assert!(tile.observed.is_none());
        assert!(tile.last_checked_at.is_none());
    }

    /// `monitor_observations` row with the given numeric value and state, optionally linked to the
    /// run and query result it was evaluated from.
    #[allow(clippy::too_many_arguments)]
    async fn insert_observation(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        monitor_id: i64,
        observed_at: DateTime<Utc>,
        value: Option<f64>,
        run_id: Option<i64>,
        query_result_id: Option<i64>,
        state: &str,
    ) {
        arcodash_core::entities::monitor_observation::ActiveModel {
            id: NotSet,
            org_id: Set(org_id),
            monitor_id: Set(monitor_id),
            observed_at: Set(observed_at.into()),
            value: Set(value),
            state: Set(state.to_string()),
            run_id: Set(run_id),
            query_result_id: Set(query_result_id),
            explanation: Set(None),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn value_monitor(db: &sea_orm::DatabaseConnection) -> (i64, i64, i64, monitor::Model) {
        let (org, admin, query) = seed(db).await;
        let monitor = insert_monitor(
            db,
            org.id,
            query.id,
            admin.id,
            json!({ "column": "value", "op": ">", "value": 5.0 }),
            "ok",
        )
        .await;
        (org.id, query.data_source_id, query.id, monitor)
    }

    /// Production order: the run finishes at `t`, its observation lands 1ms later.
    #[tokio::test]
    async fn tile_data_serves_the_raw_cell_of_a_run_backed_observation() {
        let Some(db) = test_db().await else { return };
        let (org_id, ds_id, _, monitor) = value_monitor(&db).await;
        let qr = insert_query_result(
            &db,
            org_id,
            ds_id,
            Some(json!({ "rows": [{ "value": "10813421.70" }] })),
        )
        .await;
        let t = Utc::now();
        let run = insert_run(&db, org_id, monitor.id, "success", t, Some(qr.id)).await;
        let obs_at = t + chrono::Duration::milliseconds(1);
        insert_observation(
            &db,
            org_id,
            monitor.id,
            obs_at,
            Some(10813421.7),
            Some(run.id),
            Some(qr.id),
            "ok",
        )
        .await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert_eq!(tile.observed, Some(json!("10813421.70")));
        assert!(tile
            .last_checked_at
            .is_some_and(|x| (x - obs_at).num_milliseconds().abs() < 2));
    }

    #[tokio::test]
    async fn tile_data_serves_a_non_numeric_cell_of_an_unevaluable_observation() {
        let Some(db) = test_db().await else { return };
        let (org_id, ds_id, _, monitor) = value_monitor(&db).await;
        let qr = insert_query_result(
            &db,
            org_id,
            ds_id,
            Some(json!({ "rows": [{ "value": "n/a" }] })),
        )
        .await;
        let t = Utc::now();
        let run = insert_run(&db, org_id, monitor.id, "success", t, Some(qr.id)).await;
        insert_observation(
            &db,
            org_id,
            monitor.id,
            t + chrono::Duration::milliseconds(1),
            None,
            Some(run.id),
            Some(qr.id),
            "unevaluable",
        )
        .await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert_eq!(tile.observed, Some(json!("n/a")));
    }

    #[tokio::test]
    async fn tile_data_falls_back_to_the_stored_value_when_the_query_result_is_gone() {
        let Some(db) = test_db().await else { return };
        let (org_id, _, _, monitor) = value_monitor(&db).await;
        insert_observation(
            &db,
            org_id,
            monitor.id,
            Utc::now(),
            Some(7.5),
            None,
            Some(i64::MAX),
            "ok",
        )
        .await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert_eq!(tile.observed, Some(json!(7.5)));
    }

    #[tokio::test]
    async fn tile_data_failed_run_newer_than_an_observation_reports_check_failed() {
        let Some(db) = test_db().await else { return };
        let (org_id, ds_id, _, monitor) = value_monitor(&db).await;
        let qr = insert_query_result(
            &db,
            org_id,
            ds_id,
            Some(json!({ "rows": [{ "value": 3 }] })),
        )
        .await;
        let t = Utc::now();
        insert_observation(
            &db,
            org_id,
            monitor.id,
            t - chrono::Duration::minutes(5),
            Some(3.0),
            None,
            Some(qr.id),
            "ok",
        )
        .await;
        insert_run(&db, org_id, monitor.id, "failed", t, None).await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert!(tile.check_failed);
        assert_eq!(
            tile.last_check_error.as_deref(),
            Some("connection refused: db.internal:5432")
        );
    }

    #[tokio::test]
    async fn tile_data_skipped_run_without_a_result_keeps_the_observation() {
        let Some(db) = test_db().await else { return };
        let (org_id, ds_id, _, monitor) = value_monitor(&db).await;
        let qr = insert_query_result(
            &db,
            org_id,
            ds_id,
            Some(json!({ "rows": [{ "value": "42.50" }] })),
        )
        .await;
        let t = Utc::now() - chrono::Duration::minutes(5);
        insert_observation(
            &db,
            org_id,
            monitor.id,
            t,
            Some(42.5),
            None,
            Some(qr.id),
            "ok",
        )
        .await;
        insert_run(&db, org_id, monitor.id, "skipped", Utc::now(), None).await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert_eq!(tile.observed, Some(json!("42.50")));
        assert!(tile
            .last_checked_at
            .is_some_and(|x| (x - t).num_milliseconds().abs() < 2));
        assert!(!tile.check_failed);
    }

    #[tokio::test]
    async fn tile_data_uses_a_run_less_observation() {
        let Some(db) = test_db().await else { return };
        let (org_id, _, _, monitor) = value_monitor(&db).await;
        let at = Utc::now();
        insert_observation(&db, org_id, monitor.id, at, Some(1.0), None, None, "ok").await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert_eq!(tile.observed.as_ref().and_then(Value::as_f64), Some(1.0));
        assert!(tile
            .last_checked_at
            .is_some_and(|t| (t - at).num_seconds().abs() < 5));
        assert!(!tile.check_failed);
    }

    #[tokio::test]
    async fn tile_data_observation_newer_than_a_failed_run_clears_check_failed() {
        let Some(db) = test_db().await else { return };
        let (org_id, _, _, monitor) = value_monitor(&db).await;
        let run_at = Utc::now() - chrono::Duration::minutes(5);
        insert_run(&db, org_id, monitor.id, "failed", run_at, None).await;
        insert_observation(
            &db,
            org_id,
            monitor.id,
            Utc::now(),
            Some(7.5),
            None,
            None,
            "ok",
        )
        .await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert!(!tile.check_failed);
        assert_eq!(tile.last_check_error, None);
        assert_eq!(tile.observed, Some(json!(7.5)));
    }

    #[tokio::test]
    async fn tile_data_null_valued_newer_observation_yields_no_observed() {
        let Some(db) = test_db().await else { return };
        let (org_id, _, _, monitor) = value_monitor(&db).await;
        insert_observation(&db, org_id, monitor.id, Utc::now(), None, None, None, "ok").await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert!(tile.observed.is_none());
        assert!(tile.last_checked_at.is_some());
    }

    #[tokio::test]
    async fn tile_data_result_without_the_column_ignores_the_observed_value() {
        let Some(db) = test_db().await else { return };
        let (org_id, ds_id, _, monitor) = value_monitor(&db).await;
        let qr = insert_query_result(
            &db,
            org_id,
            ds_id,
            Some(json!({ "rows": [{ "other": 1 }] })),
        )
        .await;
        let t = Utc::now();
        let run = insert_run(&db, org_id, monitor.id, "success", t, Some(qr.id)).await;
        insert_observation(
            &db,
            org_id,
            monitor.id,
            t + chrono::Duration::milliseconds(1),
            Some(9.0),
            Some(run.id),
            Some(qr.id),
            "ok",
        )
        .await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert!(tile.observed.is_none());
    }

    #[tokio::test]
    async fn tile_data_run_with_a_result_newer_than_an_observation_for_another_result_wins() {
        let Some(db) = test_db().await else { return };
        let (org_id, ds_id, _, monitor) = value_monitor(&db).await;
        let old_qr = insert_query_result(
            &db,
            org_id,
            ds_id,
            Some(json!({ "rows": [{ "value": "1" }] })),
        )
        .await;
        let new_qr = insert_query_result(
            &db,
            org_id,
            ds_id,
            Some(json!({ "rows": [{ "value": "2" }] })),
        )
        .await;
        let t = Utc::now();
        insert_observation(
            &db,
            org_id,
            monitor.id,
            t - chrono::Duration::minutes(5),
            Some(1.0),
            None,
            Some(old_qr.id),
            "ok",
        )
        .await;
        insert_run(&db, org_id, monitor.id, "success", t, Some(new_qr.id)).await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert_eq!(tile.observed, Some(json!("2")));
        assert!(tile
            .last_checked_at
            .is_some_and(|x| (x - t).num_milliseconds().abs() < 2));
    }

    #[tokio::test]
    async fn tile_data_older_run_beside_a_resultless_observation_falls_back_to_its_value() {
        let Some(db) = test_db().await else { return };
        let (org_id, ds_id, _, monitor) = value_monitor(&db).await;
        let qr = insert_query_result(
            &db,
            org_id,
            ds_id,
            Some(json!({ "rows": [{ "value": "3" }] })),
        )
        .await;
        let t = Utc::now();
        insert_run(
            &db,
            org_id,
            monitor.id,
            "success",
            t - chrono::Duration::minutes(5),
            Some(qr.id),
        )
        .await;
        insert_observation(&db, org_id, monitor.id, t, Some(8.0), None, None, "ok").await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert_eq!(tile.observed, Some(json!(8.0)));
        assert!(tile
            .last_checked_at
            .is_some_and(|x| (x - t).num_milliseconds().abs() < 2));
    }

    #[tokio::test]
    async fn tile_data_resultless_success_run_newer_than_an_observation_keeps_the_observation() {
        let Some(db) = test_db().await else { return };
        let (org_id, _, _, mut monitor) = value_monitor(&db).await;
        monitor.state = "unknown".to_string();
        let t = Utc::now();
        insert_observation(
            &db,
            org_id,
            monitor.id,
            t - chrono::Duration::minutes(5),
            Some(4.0),
            None,
            None,
            "ok",
        )
        .await;
        insert_run(&db, org_id, monitor.id, "success", t, None).await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert_eq!(tile.state, "unknown");
        assert_eq!(tile.observed, Some(json!(4.0)));
        assert!(!tile.check_failed);
    }

    #[tokio::test]
    async fn tile_data_column_missing_from_row_yields_no_observed_without_error() {
        let Some(db) = test_db().await else { return };
        let (org, admin, query) = seed(&db).await;
        let monitor = insert_monitor(
            &db,
            org.id,
            query.id,
            admin.id,
            json!({ "column": "not_a_real_column", "op": ">", "value": 5.0 }),
            "ok",
        )
        .await;
        let qr = insert_query_result(
            &db,
            org.id,
            query.data_source_id,
            Some(json!({ "rows": [{ "value": 12 }] })),
        )
        .await;
        insert_run(&db, org.id, monitor.id, "success", Utc::now(), Some(qr.id)).await;

        let tiles = tile_data(&db, std::slice::from_ref(&monitor))
            .await
            .unwrap();
        let tile = tiles.get(&monitor.id).unwrap();
        assert!(tile.observed.is_none());
        assert!(tile.last_checked_at.is_some());
    }

    #[tokio::test]
    async fn list_for_user_ids_filter_narrows_and_still_excludes_invisible_monitors() {
        let Some(db) = test_db().await else { return };
        let (org, admin, query) = seed(&db).await;

        let visible = create_monitor(&db, org.id, &admin, query.id, "Visible")
            .await
            .unwrap();
        let other_visible = create_monitor(&db, org.id, &admin, query.id, "Other visible")
            .await
            .unwrap();

        // A monitor in another org, requested by id, must be silently absent.
        let (foreign_org, foreign_admin, foreign_query) = seed(&db).await;
        let invisible = create_monitor(
            &db,
            foreign_org.id,
            &foreign_admin,
            foreign_query.id,
            "Invisible",
        )
        .await
        .unwrap();

        let result = list_for_user(
            &db,
            org.id,
            &admin,
            None,
            None,
            None,
            Some(&[visible.id, invisible.id]),
        )
        .await
        .unwrap();
        let ids: Vec<i64> = result.iter().map(|(m, _)| m.id).collect();
        assert!(ids.contains(&visible.id));
        assert!(
            !ids.contains(&other_visible.id),
            "ids filter must exclude a visible monitor that wasn't requested"
        );
        assert!(
            !ids.contains(&invisible.id),
            "ids filter must still exclude a monitor the caller cannot see"
        );
    }
}

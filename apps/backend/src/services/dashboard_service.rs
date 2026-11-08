//! Dashboard CRUD business logic for `/api/dashboards*`. Every read/mutation goes through
//! `authz::authorize::authorize`: capability AND reach; ownership alone never bypasses capability.

use std::collections::{HashMap, HashSet};

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, FromQueryResult,
    Order, QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use arcodash_core::entities::{dashboard, embed, favorite, query, user, visualization, widget};
use arcodash_core::events::record_event;

use crate::authz::authorize::{self, Action, DashboardVerb, QueryVerb};
use crate::authz::permissions::{load_effective_groups, user_has_permission};
use crate::error::{is_unique_violation, ApiError};
use crate::services::embed_service::EmbedObjectType;

/// Clock reading whose sub-second part serves as a cheap uniqueness suffix.
fn clock_fraction() -> std::time::Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

/// Generate a URL-safe slug from the dashboard name, with a short random suffix for uniqueness.
fn make_slug(name: &str) -> String {
    format!(
        "{}-{:06x}",
        crate::services::run_service::slugify(name, "dashboard"),
        clock_fraction().subsec_micros()
    )
}

/// Build a unique slug for a new dashboard in the org.
pub async fn unique_slug(
    db: &impl ConnectionTrait,
    org_id: i64,
    name: &str,
) -> Result<String, ApiError> {
    let slug = make_slug(name);
    let collision = dashboard::Entity::find()
        .filter(dashboard::Column::OrgId.eq(org_id))
        .filter(dashboard::Column::Slug.eq(slug.clone()))
        .one(db)
        .await?;
    if collision.is_none() {
        return Ok(slug);
    }
    Ok(format!("{slug}-{}", clock_fraction().subsec_nanos()))
}

/// Ids of the org's dashboards matching `sql` (`$1` = org id, `$2` = `arg`).
async fn matching_ids(
    db: &impl ConnectionTrait,
    sql: &str,
    org_id: i64,
    arg: sea_orm::Value,
) -> Result<HashSet<i64>, ApiError> {
    Ok(db
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            [org_id.into(), arg],
        ))
        .await?
        .iter()
        .filter_map(|r| r.try_get::<i64>("", "id").ok())
        .collect())
}

/// No trigger maintains `search_vector`; every write of `name`/`tags` refreshes it.
async fn update_search_vector(db: &impl ConnectionTrait, id: i64) -> Result<(), ApiError> {
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "UPDATE dashboards SET search_vector = to_tsvector('english', coalesce(name,'') || ' ' || array_to_string(coalesce(tags, '{}'),' ')) WHERE id = $1",
        [id.into()],
    ))
    .await?;
    Ok(())
}

async fn authorize_dashboard(
    db: &impl ConnectionTrait,
    user: &user::Model,
    verb: DashboardVerb,
    id: i64,
) -> Result<(), ApiError> {
    authorize::authorize(db, user, Action::Dashboard { verb, id }).await?;
    Ok(())
}

/// `GET /api/dashboards`: non-archived dashboards the user can see (owner/admin, explicit grant, or
/// org-open default for published ones).
///
/// Grants are batch-loaded, no N+1. `scope` narrows to `by-me`/`shared-with-me`; archived are
/// excluded unless `include_archived`.
pub async fn list(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    include_archived: bool,
    search: Option<String>,
    tags: Option<Vec<String>>,
    scope: Option<&str>,
) -> Result<Vec<dashboard::Model>, ApiError> {
    // Substring match (not full-text) so a partial word like "reve" finds "Revenue".
    let search_ids = match search.as_deref().filter(|q| !q.is_empty()) {
        Some(q) => {
            // Escaped in Rust so a literal % or _ in the term isn't read as a LIKE wildcard.
            let pattern = format!(
                "%{}%",
                q.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            );
            Some(
                matching_ids(
                    db,
                    "SELECT id FROM dashboards WHERE org_id = $1 AND (name ILIKE $2 OR array_to_string(coalesce(tags,'{}'), ' ') ILIKE $2)",
                    org_id,
                    pattern.into(),
                )
                .await?,
            )
        }
        None => None,
    };

    let tag_ids = match tags.filter(|t| !t.is_empty()) {
        Some(required) => Some(
            matching_ids(
                db,
                "SELECT id FROM dashboards WHERE org_id = $1 AND tags && $2::text[]",
                org_id,
                required.into(),
            )
            .await?,
        ),
        None => None,
    };

    let candidate_cap: u64 =
        arcodash_core::config::env_parsed("LIST_DASHBOARDS_MAX_CANDIDATES", 2000);
    let mut all_q = dashboard::Entity::find().filter(dashboard::Column::OrgId.eq(org_id));
    if !include_archived {
        all_q = all_q.filter(dashboard::Column::IsArchived.eq(false));
    }
    let all = all_q
        .order_by(dashboard::Column::Id, Order::Desc)
        .limit(candidate_cap)
        .all(db)
        .await?;
    if all.len() as u64 == candidate_cap {
        tracing::warn!(
            org_id,
            candidate_cap,
            "dashboard_service::list: candidate dashboard load hit its cap; some dashboards may be \
             missing from this listing"
        );
    }

    let all: Vec<dashboard::Model> = all
        .into_iter()
        .filter(|d| {
            [&search_ids, &tag_ids]
                .into_iter()
                .all(|ids| ids.as_ref().is_none_or(|ids| ids.contains(&d.id)))
        })
        .collect();

    let groups = load_effective_groups(db, user).await?;
    let admin = crate::authz::permissions::is_admin(&groups);
    // A role without `view_dashboard` never lists dashboards: capability is the ceiling.
    let can_view_type = admin || user_has_permission(user, &groups, "view_dashboard");

    let ids: Vec<i64> = all.iter().map(|d| d.id).collect();
    let grants = arcodash_core::entities::access_permission::Entity::find()
        .filter(arcodash_core::entities::access_permission::Column::ObjectType.eq("dashboard"))
        .filter(arcodash_core::entities::access_permission::Column::ObjectId.is_in(ids))
        .all(db)
        .await?;

    let visible: Vec<dashboard::Model> = if admin {
        all
    } else if !can_view_type {
        Vec::new()
    } else {
        all.into_iter()
            .filter(|d| {
                crate::authz::access::object_role(
                    user.id,
                    &user.groups,
                    admin,
                    d.user_id,
                    "dashboard",
                    d.id,
                    &d.visibility,
                    d.published_snapshot.is_none(),
                    &grants,
                )
                .is_some()
            })
            .collect()
    };

    Ok(match scope {
        Some("by-me") => visible
            .into_iter()
            .filter(|d| d.user_id == user.id)
            .collect(),
        Some("shared-with-me") => visible
            .into_iter()
            .filter(|d| {
                d.user_id != user.id
                    && crate::authz::access::has_any_grant(
                        user.id,
                        &user.groups,
                        "dashboard",
                        d.id,
                        &grants,
                    )
            })
            .collect(),
        _ => visible,
    })
}

/// `POST /api/dashboards` — create. Requires the `create_dashboard` capability.
pub async fn create(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    name: String,
) -> Result<dashboard::Model, ApiError> {
    authorize::authorize(db, user, Action::DashboardCreate).await?;

    let slug = unique_slug(db, org_id, &name).await?;
    let now = Utc::now();
    let model = dashboard::ActiveModel {
        org_id: Set(org_id),
        slug: Set(slug),
        name: Set(name),
        user_id: Set(user.id),
        layout: Set(serde_json::json!([])),
        dashboard_filters_enabled: Set(false),
        is_archived: Set(false),
        tags: Set(None),
        options: Set(serde_json::json!({})),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
        ..Default::default()
    };
    let d = model.insert(db).await?;
    update_search_vector(db, d.id).await?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "dashboard.created",
        Some("dashboard"),
        Some(d.id),
        serde_json::json!({ "name": d.name }),
    )
    .await?;

    Ok(d)
}

/// `GET /api/dashboards/:slug`: org-scoped, access-checked via `authorize`; 404s (not 403) on
/// no-access. Not used by the public-token read path.
pub async fn get_by_slug(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    slug: &str,
) -> Result<dashboard::Model, ApiError> {
    let d = dashboard::Entity::find()
        .filter(dashboard::Column::OrgId.eq(org_id))
        .filter(dashboard::Column::Slug.eq(slug))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("dashboard not found"))?;

    authorize_dashboard(db, user, DashboardVerb::View, d.id).await?;
    Ok(d)
}

/// Fetch a dashboard by integer id (org-scoped), access-checked like [`get_by_slug`].
pub async fn get_by_id(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    id: i64,
) -> Result<dashboard::Model, ApiError> {
    let d = dashboard::Entity::find_by_id(id)
        .filter(dashboard::Column::OrgId.eq(org_id))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("dashboard not found"))?;

    authorize_dashboard(db, user, DashboardVerb::View, d.id).await?;
    Ok(d)
}

/// `secret_token` is written only by [`share`] and cleared only by [`revoke_share`]; stripped from
/// caller patches in [`update`] so a stale `options` blob can't resurrect a revoked token.
const SYSTEM_OWNED_OPTION_KEYS: [&str; 1] = ["secret_token"];

/// Removes [`SYSTEM_OWNED_OPTION_KEYS`] from a caller-supplied options patch in place; no-op on a
/// non-object.
fn strip_system_owned_option_keys(patch: &mut Value) {
    if let Some(obj) = patch.as_object_mut() {
        for key in SYSTEM_OWNED_OPTION_KEYS {
            obj.remove(key);
        }
    }
}

/// Merge `patch`'s top-level keys into a dashboard's `options` against the live row, not a
/// caller-held snapshot. `||` can't delete a key; use `-` for that (see `revoke_share`).
fn options_merge_query(dashboard_id: i64, patch: Value) -> sea_orm::UpdateMany<dashboard::Entity> {
    dashboard::Entity::update_many()
        .col_expr(
            dashboard::Column::Options,
            Expr::cust_with_values("options || $1::jsonb", [patch]),
        )
        .col_expr(dashboard::Column::UpdatedAt, Expr::value(Utc::now()))
        .filter(dashboard::Column::Id.eq(dashboard_id))
}

/// `POST /api/dashboards/:slug` — update metadata.
pub async fn update(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    slug: &str,
    name: Option<String>,
    tags: Option<Vec<String>>,
    options: Option<Value>,
    is_archived: Option<bool>,
    dashboard_filters_enabled: Option<bool>,
    layout: Option<Value>,
) -> Result<dashboard::Model, ApiError> {
    let existing = get_by_slug(db, org_id, user, slug).await?;
    authorize_dashboard(db, user, DashboardVerb::Edit, existing.id).await?;

    let dashboard_id = existing.id;
    let changed: Vec<&'static str> = [
        ("name", name.is_some()),
        ("tags", tags.is_some()),
        ("options", options.is_some()),
        ("archived", is_archived.is_some()),
        (
            "dashboard_filters_enabled",
            dashboard_filters_enabled.is_some(),
        ),
        ("layout", layout.is_some()),
    ]
    .into_iter()
    .filter_map(|(field, present)| present.then_some(field))
    .collect();

    // `options` is merged separately via the DB-side `||` against the live row; a wholesale `Set`
    // would clobber a concurrent writer's key (e.g. `share`'s `secret_token`).
    let mut active: dashboard::ActiveModel = existing.into();
    if let Some(n) = name {
        active.name = Set(n);
    }
    if let Some(t) = tags {
        active.tags = Set(Some(t));
    }
    if let Some(a) = is_archived {
        active.is_archived = Set(a);
    }
    if let Some(f) = dashboard_filters_enabled {
        active.dashboard_filters_enabled = Set(f);
    }
    if let Some(l) = layout {
        active.layout = Set(l);
    }
    active.updated_at = Set(Utc::now().into());

    let updated = active.update(db).await?;

    let d = if let Some(mut o) = options {
        strip_system_owned_option_keys(&mut o);
        let rows = options_merge_query(dashboard_id, o)
            .exec_with_returning(db)
            .await?;
        rows.into_iter()
            .next()
            .ok_or_else(|| ApiError::not_found("dashboard not found"))?
    } else {
        updated
    };

    update_search_vector(db, d.id).await?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "dashboard.updated",
        Some("dashboard"),
        Some(d.id),
        serde_json::json!({ "name": d.name, "changed": changed }),
    )
    .await?;

    Ok(d)
}

/// `DELETE /api/dashboards/:slug` — archive (soft-delete).
pub async fn archive(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    slug: &str,
) -> Result<(), ApiError> {
    let existing = get_by_slug(db, org_id, user, slug).await?;
    authorize_dashboard(db, user, DashboardVerb::Delete, existing.id).await?;

    let mut active: dashboard::ActiveModel = existing.into();
    active.is_archived = Set(true);
    active.updated_at = Set(Utc::now().into());
    let archived = active.update(db).await?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "dashboard.archived",
        Some("dashboard"),
        Some(archived.id),
        serde_json::json!({ "name": archived.name }),
    )
    .await?;

    Ok(())
}

/// `GET /api/dashboards/tags` — distinct tags across all non-archived dashboards in the org.
pub async fn list_tags(db: &impl ConnectionTrait, org_id: i64) -> Result<Vec<String>, ApiError> {
    let tags: std::collections::BTreeSet<String> = dashboard::Entity::find()
        .filter(dashboard::Column::OrgId.eq(org_id))
        .filter(dashboard::Column::IsArchived.eq(false))
        .all(db)
        .await?
        .into_iter()
        .flat_map(|d| d.tags.unwrap_or_default())
        .collect();
    Ok(tags.into_iter().collect())
}

/// Load widgets with their visualizations and queries nested, via two batched `IN (...)` queries.
pub async fn load_widgets_with_viz(
    db: &impl ConnectionTrait,
    dashboard_id: i64,
) -> Result<Vec<WidgetWithViz>, ApiError> {
    let widgets = widget::Entity::find()
        .filter(widget::Column::DashboardId.eq(dashboard_id))
        .all(db)
        .await?;
    zip_widgets_with_viz(db, widgets).await
}

/// Batched visualization + query lookups shared by `load_widgets_with_viz` and
/// `load_published_widgets_with_viz`.
async fn zip_widgets_with_viz(
    db: &impl ConnectionTrait,
    widgets: Vec<widget::Model>,
) -> Result<Vec<WidgetWithViz>, ApiError> {
    let viz_ids: Vec<i64> = widgets.iter().filter_map(|w| w.visualization_id).collect();
    let vizzes: HashMap<i64, visualization::Model> = if viz_ids.is_empty() {
        HashMap::new()
    } else {
        visualization::Entity::find()
            .filter(visualization::Column::Id.is_in(viz_ids))
            .all(db)
            .await?
            .into_iter()
            .map(|v| (v.id, v))
            .collect()
    };
    let query_ids: Vec<i64> = vizzes.values().map(|v| v.query_id).collect();
    let queries: HashMap<i64, query::Model> = if query_ids.is_empty() {
        HashMap::new()
    } else {
        query::Entity::find()
            .filter(query::Column::Id.is_in(query_ids))
            .all(db)
            .await?
            .into_iter()
            .map(|q| (q.id, q))
            .collect()
    };

    Ok(widgets
        .into_iter()
        .map(|w| {
            let viz_and_query = w.visualization_id.and_then(|viz_id| {
                let viz = vizzes.get(&viz_id)?.clone();
                let q = queries.get(&viz.query_id).cloned();
                Some((viz, q))
            });
            WidgetWithViz {
                widget: w,
                viz_and_query,
            }
        })
        .collect())
}

pub struct WidgetWithViz {
    pub widget: widget::Model,
    /// `None` for text-only widgets.
    pub viz_and_query: Option<(visualization::Model, Option<query::Model>)>,
}

/// Per-query freshness for a dashboard tile's quiet-corner indicator.
#[derive(Debug, Clone, Serialize)]
pub struct WidgetFreshness {
    pub retrieved_at: DateTime<Utc>,
    /// Cadence proxy in seconds for the amber-staleness gate; `null` means never amber.
    pub expected_interval_secs: Option<i64>,
    /// Status of the newest finalized run (may be newer than the one that produced `retrieved_at`);
    /// `success` when there is no run row.
    pub last_run_status: String,
}

/// Raw shape of [`load_widget_freshness`]'s join.
#[derive(Debug, FromQueryResult)]
struct FreshnessRow {
    query_id: i64,
    retrieved_at: sea_orm::prelude::DateTimeWithTimeZone,
    last_run_status: Option<String>,
    last_finished_at: Option<sea_orm::prelude::DateTimeWithTimeZone>,
    spec: Option<Value>,
    enabled: Option<bool>,
    paused_until: Option<sea_orm::prelude::DateTimeWithTimeZone>,
    next_run_at: Option<sea_orm::prelude::DateTimeWithTimeZone>,
}

/// Cadence proxy in seconds for the amber-staleness gate, or `None` (never amber) when the query
/// won't auto-refresh. `interval` yields its own period; `cron`/`at` use the gap to the next fire;
/// `None` if a timestamp is missing or the gap is non-positive.
fn expected_interval_secs(
    spec: Option<&Value>,
    enabled: bool,
    paused_until: Option<DateTime<Utc>>,
    next_run_at: Option<DateTime<Utc>>,
    last_finished_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<i64> {
    let spec = spec?;
    let paused = paused_until.is_some_and(|t| t > now);
    if !enabled || paused {
        return None;
    }
    if let arcodash_core::schedule_spec::ScheduleSpec::Interval { every_secs } =
        serde_json::from_value::<arcodash_core::schedule_spec::ScheduleSpec>(spec.clone()).ok()?
    {
        Some(every_secs)
    } else {
        let last = last_finished_at?;
        let next = next_run_at?;
        let gap = (next - last).num_seconds();
        (gap > 0).then_some(gap)
    }
}

/// Per-query freshness for a dashboard's tiles in one round trip, keyed by `query_id`; queries with
/// no serving result are absent.
///
/// `retrieved_at` and `last_run_status` are sourced independently: a failed run never moves the
/// result pointer.
pub async fn load_widget_freshness(
    db: &impl ConnectionTrait,
    query_ids: &[i64],
) -> Result<HashMap<i64, WidgetFreshness>, ApiError> {
    if query_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = FreshnessRow::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        r"
        SELECT
            q.id AS query_id,
            qr.retrieved_at AS retrieved_at,
            lr.status AS last_run_status,
            lr.finished_at AS last_finished_at,
            s.spec AS spec,
            s.enabled AS enabled,
            s.paused_until AS paused_until,
            s.next_run_at AS next_run_at
        FROM queries q
        JOIN query_results qr
          ON qr.id = CASE WHEN q.published_change_id IS NOT NULL AND q.unpublished_at IS NULL
                          THEN q.published_latest_query_data_id
                          WHEN q.published_change_id IS NULL
                          THEN q.latest_query_data_id
                          ELSE NULL END
        LEFT JOIN LATERAL (
            SELECT r.status, r.finished_at
            FROM runs r
            WHERE r.entity_type = 'query'
              AND r.entity_id = q.id
              AND r.status IN ('success', 'failed', 'timed_out', 'cancelled', 'skipped')
            ORDER BY r.id DESC
            LIMIT 1
        ) lr ON TRUE
        LEFT JOIN schedules s
          ON s.entity_type = 'query' AND s.entity_id = q.id
        WHERE q.id = ANY($1)
        ",
        [query_ids.to_vec().into()],
    ))
    .all(db)
    .await?;

    let now = Utc::now();
    Ok(rows
        .into_iter()
        .map(|row| {
            let freshness = WidgetFreshness {
                retrieved_at: row.retrieved_at.with_timezone(&Utc),
                expected_interval_secs: expected_interval_secs(
                    row.spec.as_ref(),
                    row.enabled.unwrap_or(false),
                    row.paused_until.map(|t| t.with_timezone(&Utc)),
                    row.next_run_at.map(|t| t.with_timezone(&Utc)),
                    row.last_finished_at.map(|t| t.with_timezone(&Utc)),
                    now,
                ),
                // No run row means the result predates run tracking: treat as success.
                last_run_status: row.last_run_status.unwrap_or_else(|| "success".to_string()),
            };
            (row.query_id, freshness)
        })
        .collect())
}

/// Whether `user` can read `query_id`'s results (same gate as `query_service::get_for_user`), so a
/// widget can say "you can't see this" instead of silently 404ing. Batched by distinct query id.
pub async fn load_viewer_query_access(
    db: &impl ConnectionTrait,
    user: &user::Model,
    query_ids: &[i64],
) -> Result<HashMap<i64, bool>, ApiError> {
    let groups = load_effective_groups(db, user).await?;
    let is_admin = crate::authz::permissions::is_admin(&groups);

    let mut distinct: Vec<i64> = query_ids.to_vec();
    distinct.sort_unstable();
    distinct.dedup();

    let mut access = HashMap::with_capacity(distinct.len());
    for query_id in distinct {
        let has_reach = authorize::evaluate_with_groups(
            db,
            user,
            &groups,
            is_admin,
            Action::Query {
                verb: QueryVerb::View,
                id: query_id,
            },
        )
        .await?
        .can_edit;
        let can_view =
            has_reach && crate::authz::access::can_view_object(db, user, "query", query_id).await?;
        access.insert(query_id, can_view);
    }
    Ok(access)
}

/// Rebuild the tile set for non-editor/public/embed reads from `published_snapshot`, resolving each
/// visualization and query LIVE. A `None` snapshot yields an empty set.
pub async fn load_published_widgets_with_viz(
    db: &impl ConnectionTrait,
    d: &dashboard::Model,
) -> Result<Vec<WidgetWithViz>, ApiError> {
    if !is_published(d) {
        return Ok(Vec::new());
    }
    let snapshot = parse_snapshot(d)?;

    let widgets: Vec<widget::Model> = snapshot
        .widgets
        .into_iter()
        .map(|sw| widget::Model {
            id: sw.id,
            dashboard_id: d.id,
            visualization_id: sw.visualization_id,
            text: sw.text,
            width: sw.width,
            options: sw.options,
            created_at: sw.created_at.unwrap_or(d.created_at),
            updated_at: sw.updated_at.unwrap_or(d.updated_at),
        })
        .collect();

    Ok(zip_widgets_with_viz(db, widgets)
        .await?
        .into_iter()
        // Skip ghost tiles whose visualization was deleted after publish; text-only tiles are
        // unaffected.
        .filter(|item| item.widget.visualization_id.is_none() || item.viz_and_query.is_some())
        .collect())
}

/// The committed tile arrangement in `dashboards.published_snapshot`, rebuilt by every [`publish`].
/// Holds references only, never a query's result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub name: String,
    pub layout: Value,
    pub options: Value,
    pub tags: Vec<String>,
    pub dashboard_filters_enabled: bool,
    pub widgets: Vec<SnapshotWidget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotWidget {
    pub id: i64,
    pub visualization_id: Option<i64>,
    /// Denormalized from the widget's visualization at publish time; data still resolves through
    /// the query's published pointer.
    pub query_id: Option<i64>,
    pub text: Option<String>,
    pub width: i32,
    pub options: Value,
    /// Absent in older snapshots; reconstruction falls back to the dashboard's timestamps.
    #[serde(default)]
    pub created_at: Option<sea_orm::prelude::DateTimeWithTimeZone>,
    #[serde(default)]
    pub updated_at: Option<sea_orm::prelude::DateTimeWithTimeZone>,
}

/// Project the live draft into the shape stored in `published_snapshot`.
pub async fn snapshot_from_draft(
    db: &impl ConnectionTrait,
    d: &dashboard::Model,
) -> Result<DashboardSnapshot, ApiError> {
    let widgets = load_widgets_with_viz(db, d.id).await?;
    Ok(DashboardSnapshot {
        name: d.name.clone(),
        layout: d.layout.clone(),
        options: d.options.clone(),
        tags: d.tags.clone().unwrap_or_default(),
        dashboard_filters_enabled: d.dashboard_filters_enabled,
        widgets: widgets
            .into_iter()
            .map(|w| {
                let query_id = w.viz_and_query.as_ref().map(|(viz, _)| viz.query_id);
                SnapshotWidget {
                    id: w.widget.id,
                    visualization_id: w.widget.visualization_id,
                    query_id,
                    text: w.widget.text,
                    width: w.widget.width,
                    options: w.widget.options,
                    created_at: Some(w.widget.created_at),
                    updated_at: Some(w.widget.updated_at),
                }
            })
            .collect(),
    })
}

/// `true` iff `d` has ever been published. Pure, no DB.
#[must_use]
pub const fn is_published(d: &dashboard::Model) -> bool {
    d.published_snapshot.is_some()
}

/// Guard for sites that need an already-published dashboard (share/embed, discard). Pure, no DB.
pub fn require_published(d: &dashboard::Model) -> Result<(), ApiError> {
    if !is_published(d) {
        return Err(ApiError::bad_request("publish this dashboard first"));
    }
    Ok(())
}

fn parse_snapshot(d: &dashboard::Model) -> Result<DashboardSnapshot, ApiError> {
    let raw = d
        .published_snapshot
        .clone()
        .ok_or_else(|| ApiError::internal("dashboard has no published_snapshot to parse"))?;
    serde_json::from_value(raw)
        .map_err(|e| ApiError::internal(format!("stored dashboard snapshot is malformed: {e}")))
}

/// `POST /api/dashboards/:slug/publish`: overwrite `published_snapshot` with the current draft
/// (first publish and republish are the same call). Requires a name and at least one widget.
pub async fn publish(
    db: &impl ConnectionTrait,
    user: &user::Model,
    d: dashboard::Model,
) -> Result<dashboard::Model, ApiError> {
    authorize_dashboard(db, user, DashboardVerb::Edit, d.id).await?;

    if d.name.trim().is_empty() {
        return Err(ApiError::bad_request(
            "name this dashboard before publishing",
        ));
    }

    let snapshot = snapshot_from_draft(db, &d).await?;
    if snapshot.widgets.is_empty() {
        return Err(ApiError::bad_request(
            "add at least one tile before publishing",
        ));
    }

    let snapshot_json = serde_json::to_value(&snapshot)
        .map_err(|e| ApiError::internal(format!("failed to serialize dashboard snapshot: {e}")))?;

    let mut active: dashboard::ActiveModel = d.into();
    active.published_snapshot = Set(Some(snapshot_json));
    active.updated_at = Set(Utc::now().into());
    let updated = active.update(db).await?;

    record_event(
        db,
        updated.org_id,
        Some(user.id),
        "dashboard.published",
        Some("dashboard"),
        Some(updated.id),
        serde_json::json!({ "name": updated.name }),
    )
    .await?;

    Ok(updated)
}

/// `POST /api/dashboards/:slug/unpublish`: return a live dashboard to draft by clearing
/// `published_snapshot` to `NULL` (no restore feature, nothing to preserve).
pub async fn unpublish(
    db: &impl ConnectionTrait,
    user: &user::Model,
    d: dashboard::Model,
) -> Result<dashboard::Model, ApiError> {
    authorize_dashboard(db, user, DashboardVerb::Edit, d.id).await?;

    if !is_published(&d) {
        return Err(ApiError::bad_request(
            "dashboard is not currently published",
        ));
    }

    let mut active: dashboard::ActiveModel = d.into();
    active.published_snapshot = Set(None);
    active.updated_at = Set(Utc::now().into());
    let updated = active.update(db).await?;

    record_event(
        db,
        updated.org_id,
        Some(user.id),
        "dashboard.unpublished",
        Some("dashboard"),
        Some(updated.id),
        serde_json::json!({ "name": updated.name }),
    )
    .await?;

    Ok(updated)
}

/// `POST /api/dashboards/:slug/discard`: overwrite the live draft from `published_snapshot`. Only
/// valid once published.
pub async fn discard_changes(
    db: &(impl ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    user: &user::Model,
    d: dashboard::Model,
) -> Result<dashboard::Model, ApiError> {
    authorize_dashboard(db, user, DashboardVerb::Edit, d.id).await?;
    require_published(&d)?;

    let org_id = d.org_id;
    let snapshot = parse_snapshot(&d)?;
    let updated = apply_snapshot_to_draft(db, user, d, &snapshot).await?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "dashboard.discarded",
        Some("dashboard"),
        Some(updated.id),
        serde_json::json!({ "name": updated.name }),
    )
    .await?;

    Ok(updated)
}

/// Reset `d`'s meta columns and reconcile its widget rows to `snapshot` in one transaction (update,
/// delete, insert).
async fn apply_snapshot_to_draft(
    db: &(impl ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    user: &user::Model,
    d: dashboard::Model,
    snapshot: &DashboardSnapshot,
) -> Result<dashboard::Model, ApiError> {
    let txn = db.begin().await?;
    let now = Utc::now();

    let current = widget::Entity::find()
        .filter(widget::Column::DashboardId.eq(d.id))
        .all(&txn)
        .await?;
    let current_ids: HashSet<i64> = current.iter().map(|w| w.id).collect();

    // A widget whose visualization was cascade-deleted can't be reinserted (the FK would abort
    // every discard); restore what is restorable.
    let referenced_viz_ids: Vec<i64> = snapshot
        .widgets
        .iter()
        .filter_map(|w| w.visualization_id)
        .collect();
    let live_viz_ids: HashSet<i64> = if referenced_viz_ids.is_empty() {
        HashSet::new()
    } else {
        visualization::Entity::find()
            .filter(visualization::Column::Id.is_in(referenced_viz_ids))
            .all(&txn)
            .await?
            .into_iter()
            .map(|v| v.id)
            .collect()
    };
    let restorable: Vec<&SnapshotWidget> = snapshot
        .widgets
        .iter()
        .filter(|sw| match sw.visualization_id {
            Some(viz_id) if !live_viz_ids.contains(&viz_id) => {
                tracing::warn!(
                    dashboard_id = d.id,
                    widget_id = sw.id,
                    visualization_id = viz_id,
                    "discard: dropping snapshot widget whose visualization no longer exists"
                );
                false
            }
            _ => true,
        })
        .collect();
    let snapshot_ids: HashSet<i64> = restorable.iter().map(|w| w.id).collect();

    let mut removed_viz_ids = Vec::new();
    for w in current {
        if !snapshot_ids.contains(&w.id) {
            widget::Entity::delete_by_id(w.id).exec(&txn).await?;
            removed_viz_ids.extend(w.visualization_id);
        }
    }

    for sw in restorable {
        if current_ids.contains(&sw.id) {
            let active = widget::ActiveModel {
                id: Set(sw.id),
                dashboard_id: Set(d.id),
                visualization_id: Set(sw.visualization_id),
                text: Set(sw.text.clone()),
                width: Set(sw.width),
                options: Set(sw.options.clone()),
                updated_at: Set(now.into()),
                ..Default::default()
            };
            active.update(&txn).await?;
        } else {
            widget::ActiveModel {
                dashboard_id: Set(d.id),
                visualization_id: Set(sw.visualization_id),
                text: Set(sw.text.clone()),
                width: Set(sw.width),
                options: Set(sw.options.clone()),
                created_at: Set(now.into()),
                updated_at: Set(now.into()),
                ..Default::default()
            }
            .insert(&txn)
            .await?;
        }
    }

    for viz_id in removed_viz_ids {
        crate::services::widget_service::cleanup_ai_widget_query_isolated(&txn, user, viz_id).await;
    }

    let mut active: dashboard::ActiveModel = d.into();
    active.name = Set(snapshot.name.clone());
    active.layout = Set(snapshot.layout.clone());
    active.options = Set(snapshot.options.clone());
    active.tags = Set(Some(snapshot.tags.clone()));
    active.dashboard_filters_enabled = Set(snapshot.dashboard_filters_enabled);
    active.updated_at = Set(now.into());
    let updated = active.update(&txn).await?;

    update_search_vector(&txn, updated.id).await?;

    txn.commit().await?;
    Ok(updated)
}

/// Whether the live draft differs from `published_snapshot` (id-independent content compare).
/// Always `false` if never published.
#[must_use]
pub fn has_unpublished_changes(d: &dashboard::Model, live_widgets: &[WidgetWithViz]) -> bool {
    if !is_published(d) {
        return false;
    }
    // An unparseable snapshot can't be trusted as "matches".
    let Ok(snapshot) = parse_snapshot(d) else {
        return true;
    };

    if d.name != snapshot.name
        || d.layout != snapshot.layout
        || d.options != snapshot.options
        || d.tags.as_deref().unwrap_or_default() != snapshot.tags
        || d.dashboard_filters_enabled != snapshot.dashboard_filters_enabled
        || live_widgets.len() != snapshot.widgets.len()
    {
        return true;
    }

    // O(n^2) multiset match: dashboards hold a few dozen widgets and `Value` has no Ord/Hash.
    let mut remaining: Vec<&SnapshotWidget> = snapshot.widgets.iter().collect();
    for w in live_widgets {
        let Some(i) = remaining.iter().position(|sw| {
            sw.visualization_id == w.widget.visualization_id
                && sw.text == w.widget.text
                && sw.width == w.widget.width
                && sw.options == w.widget.options
        }) else {
            return true;
        };
        remaining.swap_remove(i);
    }
    false
}

/// Draft-vs-published diff counts for the publish/discard confirm, matched by widget id; nothing is
/// stored.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ChangeSummary {
    pub added: usize,
    pub removed: usize,
    pub moved: usize,
    pub resized: usize,
    pub settings_changed: usize,
}

fn position_field<'a>(options: &'a Value, key: &str) -> Option<&'a Value> {
    options.get("position").and_then(|p| p.get(key))
}

fn options_sans_position(options: &Value) -> Value {
    let mut v = options.clone();
    if let Some(map) = v.as_object_mut() {
        map.remove("position");
    }
    v
}

#[must_use]
pub fn change_summary(
    live_widgets: &[WidgetWithViz],
    snapshot: &DashboardSnapshot,
) -> ChangeSummary {
    let mut summary = ChangeSummary::default();

    let snapshot_by_id: HashMap<i64, &SnapshotWidget> =
        snapshot.widgets.iter().map(|w| (w.id, w)).collect();
    let live_ids: HashSet<i64> = live_widgets.iter().map(|w| w.widget.id).collect();

    for w in live_widgets {
        match snapshot_by_id.get(&w.widget.id) {
            None => summary.added += 1,
            Some(sw) => {
                let differs = |keys: [&str; 2]| {
                    keys.iter().any(|k| {
                        position_field(&w.widget.options, k) != position_field(&sw.options, k)
                    })
                };
                let settings_changed = w.widget.text != sw.text
                    || w.widget.width != sw.width
                    || options_sans_position(&w.widget.options)
                        != options_sans_position(&sw.options);
                summary.moved += usize::from(differs(["col", "row"]));
                summary.resized += usize::from(differs(["sizeX", "sizeY"]));
                summary.settings_changed += usize::from(settings_changed);
            }
        }
    }

    summary.removed = snapshot_by_id
        .keys()
        .filter(|id| !live_ids.contains(id))
        .count();

    summary
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayoutPosition {
    pub widget_id: i64,
    pub position: Value,
}

/// `PUT /api/dashboards/:slug/layout`: apply every widget's new `options.position` in one
/// transaction. Last writer wins; no version/lock param.
pub async fn save_layout(
    db: &(impl ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    org_id: i64,
    user: &user::Model,
    slug: &str,
    positions: Vec<LayoutPosition>,
) -> Result<Vec<widget::Model>, ApiError> {
    let d = get_by_slug(db, org_id, user, slug).await?;
    authorize_dashboard(db, user, DashboardVerb::Edit, d.id).await?;

    let txn = db.begin().await?;
    let mut updated = Vec::with_capacity(positions.len());
    for p in positions {
        let w = widget::Entity::find_by_id(p.widget_id)
            .one(&txn)
            .await?
            .ok_or_else(|| ApiError::not_found("widget not found"))?;
        if w.dashboard_id != d.id {
            return Err(ApiError::bad_request(
                "widget does not belong to this dashboard",
            ));
        }

        let mut opts = w.options.clone();
        match opts.as_object_mut() {
            Some(map) => {
                map.insert("position".to_string(), p.position);
            }
            None => opts = serde_json::json!({ "position": p.position }),
        }

        let mut active: widget::ActiveModel = w.into();
        active.options = Set(opts);
        active.updated_at = Set(Utc::now().into());
        updated.push(active.update(&txn).await?);
    }
    txn.commit().await?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "dashboard.layout_saved",
        Some("dashboard"),
        Some(d.id),
        serde_json::json!({ "widget_count": updated.len() }),
    )
    .await?;

    Ok(updated)
}

/// Star a dashboard for the current user; access-checked like `GET`, so an unreachable dashboard
/// 404s.
pub async fn star(
    db: &(impl ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    org_id: i64,
    user: &user::Model,
    dashboard_id: i64,
) -> Result<(), ApiError> {
    get_by_id(db, org_id, user, dashboard_id).await?;

    let now = Utc::now();
    let active = favorite::ActiveModel {
        org_id: Set(org_id),
        object_type: Set("dashboard".to_string()),
        object_id: Set(dashboard_id),
        user_id: Set(user.id),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
        ..Default::default()
    };

    // Own savepoint so a lost race against a concurrent star doesn't poison the request
    // transaction.
    match crate::db::insert_scoped(db, active).await {
        Err(e) if !is_unique_violation(&e) => Err(e.into()),
        _ => Ok(()),
    }
}

/// Unstar a dashboard; same access check as [`star`].
pub async fn unstar(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    dashboard_id: i64,
) -> Result<(), ApiError> {
    get_by_id(db, org_id, user, dashboard_id).await?;

    favorite::Entity::delete_many()
        .filter(favorite::Column::UserId.eq(user.id))
        .filter(favorite::Column::ObjectType.eq("dashboard"))
        .filter(favorite::Column::ObjectId.eq(dashboard_id))
        .exec(db)
        .await?;
    Ok(())
}

/// `GET /api/dashboards/favorites`: filters the access-checked [`list`] to favorited ids, so a
/// stale favorite never leaks a dashboard the list would withhold.
pub async fn list_favorites(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
) -> Result<Vec<dashboard::Model>, ApiError> {
    let fav_ids: HashSet<i64> = favorite::Entity::find()
        .filter(favorite::Column::UserId.eq(user.id))
        .filter(favorite::Column::ObjectType.eq("dashboard"))
        .all(db)
        .await?
        .iter()
        .map(|f| f.object_id)
        .collect();
    if fav_ids.is_empty() {
        return Ok(vec![]);
    }

    let visible = list(db, org_id, user, false, None, None, None).await?;
    Ok(visible
        .into_iter()
        .filter(|d| fav_ids.contains(&d.id))
        .collect())
}

/// Gate for minting/rotating the deprecated `share`/`rotate_share` token: `create_embed` +
/// `share_public_anonymous` plus owner-or-admin, since it mints an equally anonymous link.
async fn require_can_share_publicly(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    dashboard_id: i64,
) -> Result<(), ApiError> {
    let groups = load_effective_groups(db, user).await?;
    if !user_has_permission(user, &groups, "create_embed") {
        return Err(ApiError::forbidden("missing the create_embed permission"));
    }
    if !user_has_permission(user, &groups, "share_public_anonymous") {
        return Err(ApiError::forbidden(
            "missing the share_public_anonymous permission",
        ));
    }
    crate::services::embed_service::require_can_share_object(
        db,
        org_id,
        user,
        &groups,
        EmbedObjectType::Dashboard,
        dashboard_id,
        true,
    )
    .await?;
    crate::services::embed_service::require_object_publicly_servable(
        db,
        org_id,
        EmbedObjectType::Dashboard,
        dashboard_id,
    )
    .await
}

/// Generate a public secret token in `options.secret_token` (idempotent) and return it. Deprecated
/// in favor of `embed_service::create_embed`; also mints a backing `embeds` row, with
/// `anon_authorized = true`.
pub async fn share(
    db: &impl ConnectionTrait,
    state: &crate::state::AppState,
    org_id: i64,
    user: &user::Model,
    slug: &str,
) -> Result<String, ApiError> {
    let d = get_by_slug(db, org_id, user, slug).await?;
    authorize_dashboard(db, user, DashboardVerb::Share, d.id).await?;
    require_published(&d)?;
    require_can_share_publicly(db, org_id, user, d.id).await?;

    let dashboard_id = d.id;

    // Return the stored token only if its backing `embeds` row is still live; a revoked one is
    // re-minted.
    if let Some(tok) = d.options["secret_token"].as_str() {
        let live = embed::Entity::find()
            .filter(embed::Column::OrgId.eq(org_id))
            .filter(embed::Column::Token.eq(tok))
            .filter(embed::Column::RevokedAt.is_null())
            .one(db)
            .await?;
        if live.is_some() {
            return Ok(tok.to_string());
        }
    }

    let token = Uuid::new_v4().to_string();
    let patch = serde_json::json!({ "secret_token": token });
    options_merge_query(dashboard_id, patch).exec(db).await?;

    let now = Utc::now();
    // No caller-supplied expiry, so the org's default TTL applies, as in
    // `embed_service::create_embed`.
    let expires_at =
        crate::services::embed_service::clamp_expiry(None, state.embed_default_expiry, now)?;
    let created = embed::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        object_type: Set("dashboard".to_string()),
        object_id: Set(dashboard_id),
        mode: Set("public".to_string()),
        token: Set(Some(token.clone())),
        locked_params: Set(serde_json::json!({})),
        frame_ancestors: Set(None),
        expires_at: Set(expires_at.map(Into::into)),
        created_by_id: Set(Some(user.id)),
        anon_authorized: Set(true),
        revoked_at: Set(None),
        created_at: Set(now.into()),
    }
    .insert(db)
    .await?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "dashboard.share_token_minted",
        Some("dashboard"),
        Some(dashboard_id),
        serde_json::json!({ "embed_id": created.id }),
    )
    .await?;

    Ok(token)
}

/// Revoke exactly the `embeds` row backing `options.secret_token`, not other embeds on the
/// dashboard. No-op if there is no token or its row is already gone.
pub async fn revoke_share(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    slug: &str,
) -> Result<(), ApiError> {
    let d = get_by_slug(db, org_id, user, slug).await?;
    authorize_dashboard(db, user, DashboardVerb::RevokeShare, d.id).await?;
    // Revoking a public link is as sensitive as minting one: same creator-or-admin policy as
    // `DELETE /api/embeds/{id}`.
    let groups = load_effective_groups(db, user).await?;
    crate::services::embed_service::require_can_share_object(
        db,
        org_id,
        user,
        &groups,
        EmbedObjectType::Dashboard,
        d.id,
        false,
    )
    .await?;

    let dashboard_id = d.id;
    let now = Utc::now();
    let live = match d.options["secret_token"].as_str() {
        Some(tok) => {
            embed::Entity::find()
                .filter(embed::Column::OrgId.eq(org_id))
                .filter(embed::Column::Token.eq(tok))
                .filter(embed::Column::ObjectType.eq(EmbedObjectType::Dashboard.as_str()))
                .filter(embed::Column::ObjectId.eq(dashboard_id))
                .filter(embed::Column::RevokedAt.is_null())
                .one(db)
                .await?
        }
        None => None,
    };
    let revoked_id = match live {
        Some(row) => {
            let mut active: embed::ActiveModel = row.into();
            active.revoked_at = Set(Some(now.into()));
            Some(active.update(db).await?.id)
        }
        None => None,
    };

    // Cleared even if no live row was found; `-` (not `||`) since this removes a key.
    dashboard::Entity::update_many()
        .col_expr(
            dashboard::Column::Options,
            Expr::cust("options - 'secret_token'"),
        )
        .col_expr(dashboard::Column::UpdatedAt, Expr::value(now))
        .filter(dashboard::Column::Id.eq(dashboard_id))
        .exec(db)
        .await?;

    if let Some(embed_id) = revoked_id {
        record_event(
            db,
            org_id,
            Some(user.id),
            "dashboard.link_revoked",
            Some("dashboard"),
            Some(dashboard_id),
            serde_json::json!({ "embed_id": embed_id }),
        )
        .await?;
    }

    Ok(())
}

/// Rotate the public secret token: revoke the current one and mint a fresh one, invalidating old
/// links (unlike idempotent [`share`]).
pub async fn rotate_share(
    db: &impl ConnectionTrait,
    state: &crate::state::AppState,
    org_id: i64,
    user: &user::Model,
    slug: &str,
) -> Result<String, ApiError> {
    let d = get_by_slug(db, org_id, user, slug).await?;
    require_can_share_publicly(db, org_id, user, d.id).await?;
    revoke_share(db, org_id, user, slug).await?;
    let token = share(db, state, org_id, user, slug).await?;

    if let Some(created) = embed::Entity::find()
        .filter(embed::Column::OrgId.eq(org_id))
        .filter(embed::Column::Token.eq(&token))
        .one(db)
        .await?
    {
        record_event(
            db,
            org_id,
            Some(user.id),
            "dashboard.link_rotated",
            Some("dashboard"),
            Some(d.id),
            serde_json::json!({ "embed_id": created.id }),
        )
        .await?;
    }

    Ok(token)
}

#[cfg(test)]
mod freshness_tests {
    use super::*;

    fn spec(json: Value) -> Option<Value> {
        Some(json)
    }

    #[test]
    fn interval_spec_yields_its_own_period_regardless_of_run_history() {
        let now = Utc::now();
        let s = expected_interval_secs(
            spec(serde_json::json!({"kind": "interval", "every_secs": 900})).as_ref(),
            true,
            None,
            None,
            None,
            now,
        );
        assert_eq!(s, Some(900));
    }

    #[test]
    fn cron_spec_uses_the_observed_next_fire_gap() {
        let now = Utc::now();
        let last = now - chrono::Duration::hours(24);
        let s = expected_interval_secs(
            spec(serde_json::json!({"kind": "cron", "expr": "0 30 9 * * *"})).as_ref(),
            true,
            None,
            Some(now), // next fire due now — 24h gap from `last`
            Some(last),
            now,
        );
        assert_eq!(s, Some(86_400));
    }

    #[test]
    fn cron_spec_with_a_non_positive_gap_is_never_amber() {
        let now = Utc::now();
        // Rewound `next_run_at` before `last_finished_at` must not divide by zero or go negative.
        let s = expected_interval_secs(
            spec(serde_json::json!({"kind": "cron", "expr": "0 30 9 * * *"})).as_ref(),
            true,
            None,
            Some(now - chrono::Duration::hours(1)),
            Some(now),
            now,
        );
        assert_eq!(s, None);
    }

    #[test]
    fn cron_spec_missing_either_timestamp_is_never_amber() {
        let now = Utc::now();
        let s = expected_interval_secs(
            spec(serde_json::json!({"kind": "cron", "expr": "0 30 9 * * *"})).as_ref(),
            true,
            None,
            None,
            Some(now),
            now,
        );
        assert_eq!(s, None);
    }

    #[test]
    fn at_spec_uses_the_observed_next_fire_gap_same_as_cron() {
        let now = Utc::now();
        let last = now - chrono::Duration::hours(24);
        let s = expected_interval_secs(
            spec(serde_json::json!({"kind": "at", "time": "09:30"})).as_ref(),
            true,
            None,
            Some(now),
            Some(last),
            now,
        );
        assert_eq!(s, Some(86_400));
    }

    #[test]
    fn disabled_schedule_never_goes_amber() {
        let now = Utc::now();
        let s = expected_interval_secs(
            spec(serde_json::json!({"kind": "interval", "every_secs": 300})).as_ref(),
            false,
            None,
            None,
            None,
            now,
        );
        assert_eq!(s, None);
    }

    #[test]
    fn currently_paused_schedule_never_goes_amber() {
        let now = Utc::now();
        let s = expected_interval_secs(
            spec(serde_json::json!({"kind": "interval", "every_secs": 300})).as_ref(),
            true,
            Some(now + chrono::Duration::hours(1)),
            None,
            None,
            now,
        );
        assert_eq!(s, None);
    }

    #[test]
    fn a_pause_that_has_already_elapsed_no_longer_suppresses_cadence() {
        let now = Utc::now();
        let s = expected_interval_secs(
            spec(serde_json::json!({"kind": "interval", "every_secs": 300})).as_ref(),
            true,
            Some(now - chrono::Duration::minutes(1)),
            None,
            None,
            now,
        );
        assert_eq!(s, Some(300));
    }

    #[test]
    fn unscheduled_query_never_goes_amber() {
        let now = Utc::now();
        let s = expected_interval_secs(None, false, None, None, None, now);
        assert_eq!(s, None);
    }
}

#[cfg(test)]
mod snapshot_widget_tests {
    use super::*;

    /// Snapshots stored before `created_at`/`updated_at` existed must still deserialize
    /// (`#[serde(default)]`).
    #[test]
    fn snapshot_widget_without_timestamps_deserializes_to_none() {
        let raw = serde_json::json!({
            "id": 1,
            "visualization_id": null,
            "query_id": null,
            "text": "legacy tile",
            "width": 6,
            "options": {}
        });
        let sw: SnapshotWidget = serde_json::from_value(raw).expect("must deserialize");
        assert!(sw.created_at.is_none());
        assert!(sw.updated_at.is_none());
    }
}

#[cfg(test)]
mod options_merge_tests {
    use super::*;
    use sea_orm::{DbBackend, QueryTrait};

    /// Confirms the exact SQL: a single `UPDATE ... SET options = options || $1`, never a `SELECT`
    /// first.
    #[test]
    fn options_merge_query_uses_jsonb_concat_not_a_read_then_write() {
        let sql = options_merge_query(7, serde_json::json!({ "secret_token": "abc" }))
            .build(DbBackend::Postgres)
            .to_string()
            .to_lowercase();
        assert!(sql.starts_with("update"), "sql: {sql}");
        assert!(sql.contains("options || "), "sql: {sql}");
        assert!(!sql.contains("select"), "must not read-then-write: {sql}");
    }

    /// `secret_token` is the writer's own key: a caller patch touching it must never reach
    /// `update`'s merge.
    #[test]
    fn strip_system_owned_option_keys_removes_secret_token_but_keeps_other_keys() {
        let mut patch = serde_json::json!({ "secret_token": "attacker-supplied", "theme": "dark" });
        strip_system_owned_option_keys(&mut patch);
        assert_eq!(patch, serde_json::json!({ "theme": "dark" }));
    }

    #[test]
    fn strip_system_owned_option_keys_is_a_no_op_on_a_non_object_patch() {
        let mut patch = serde_json::json!("not an object");
        strip_system_owned_option_keys(&mut patch);
        assert_eq!(patch, serde_json::json!("not an object"));
    }
}

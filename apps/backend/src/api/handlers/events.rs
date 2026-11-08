//! `/api/events` — audit log, admin-only read.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use crate::extract::Json;
use axum::extract::Query;
use axum::http::HeaderMap;
use chrono::{DateTime, NaiveDate, Utc};
use sea_orm::{
    sea_query::Expr, ColumnTrait, Condition, EntityTrait, Order, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use arcodash_core::entities::{
    dashboard, data_source, embed, event, group, monitor, notification_destination, organization,
    query, query_snippet, role, user, visualization, widget,
};

use crate::auth::{CurrentOrg, CurrentUser};
use crate::db::Db;
use crate::error::ApiError;
use crate::services::display_names;

/// Maps a stored `events.action` string to the name the API presents, applied on read (both in
/// the returned `action` field and in the filter condition below) — so a writer's stored spelling
/// and the client's vocabulary can differ without a data migration. Anything not listed here
/// passes through unchanged.
const LEGACY_ACTION_MAP: &[(&str, &str)] = &[
    ("embed_created", "embed.created"),
    ("login_failed", "user.login_failed"),
    ("monitor_acknowledged", "monitor.acknowledged"),
    ("execute_query", "query.executed"),
    ("login", "user.login"),
    (
        arcodash_core::events::MONITOR_STATE_CHANGED,
        "monitor.state_changed",
    ),
    ("embed_revoked", "embed.revoked"),
    ("embed_public_paused", "embed.public_paused"),
    ("embed_public_resumed", "embed.public_resumed"),
    ("monitor_escalated", "monitor.escalated"),
    ("execute_query_failed", "query.execution_failed"),
    ("cancel_query_execution", "query.execution_cancelled"),
    (
        "notification_delivery_failed",
        "notification.delivery_failed",
    ),
];

fn translate_legacy_action(raw: &str) -> &str {
    LEGACY_ACTION_MAP
        .iter()
        .find(|(old, _)| *old == raw)
        .map_or(raw, |(_, new)| new)
}

/// SQL `CASE` mirroring [`translate_legacy_action`], so a filter can match a legacy row by its
/// current name without pulling every row into Rust first. Built from a fixed literal list (never
/// request input), so string-formatting it into the query is safe.
fn legacy_action_case_sql() -> String {
    let whens = LEGACY_ACTION_MAP
        .iter()
        .fold(String::new(), |mut s, (old, new)| {
            let _ = write!(s, " WHEN '{old}' THEN '{new}'");
            s
        });
    format!(r#"CASE "action"{whens} ELSE "action" END"#)
}

/// A resolved `object_type#object_id` reference: a display name and, only when the object has a
/// real per-id page to send the reader to, a link.
#[derive(Clone, Serialize)]
pub struct ObjectRef {
    pub name: String,
    pub path: Option<String>,
}

#[derive(Serialize)]
pub struct EventResponse {
    pub id: i64,
    pub org_id: i64,
    pub user_id: Option<i64>,
    /// Author display name, resolved at read time — `None` if the user has since been deleted
    /// (or the event was system-generated, e.g. a scheduled monitor evaluation).
    pub user_name: Option<String>,
    pub action: String,
    pub object_type: Option<String>,
    pub object_id: Option<i64>,
    /// Resolved from `(object_type, object_id)` at read time — `None` when the object has since
    /// been deleted or is a type this endpoint doesn't know how to name (e.g. `query_result`,
    /// `embed`, neither of which carries a human-readable name).
    pub object: Option<ObjectRef>,
    pub additional_properties: Value,
    pub created_at: DateTime<Utc>,
}

fn to_resp(
    e: event::Model,
    user_names: &HashMap<i64, String>,
    object_names: &HashMap<(String, i64), ObjectRef>,
) -> EventResponse {
    let object = match (&e.object_type, e.object_id) {
        (Some(t), Some(id)) => object_names.get(&(t.clone(), id)).cloned(),
        _ => None,
    };
    EventResponse {
        id: e.id,
        org_id: e.org_id,
        user_name: e.user_id.and_then(|uid| user_names.get(&uid).cloned()),
        user_id: e.user_id,
        action: translate_legacy_action(&e.action).to_string(),
        object_type: e.object_type,
        object_id: e.object_id,
        object,
        additional_properties: e.additional_properties,
        created_at: e.created_at.into(),
    }
}

/// The org's dashboards among `ids`, keyed by id (no query for an empty set).
async fn org_dashboards(
    db: &Db,
    org_id: i64,
    ids: HashSet<i64>,
) -> Result<HashMap<i64, dashboard::Model>, ApiError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(dashboard::Entity::find()
        .filter(dashboard::Column::OrgId.eq(org_id))
        .filter(dashboard::Column::Id.is_in(ids))
        .all(db)
        .await?
        .into_iter()
        .map(|d| (d.id, d))
        .collect())
}

/// Batch-resolve every `(object_type, object_id)` pair referenced by `events` to a name and, for
/// types with a real per-id page, a link. Only object types actually shipped by `record_event`
/// call sites are handled; an unrecognized type (or one with no name field, like `query_result`
/// or `embed`) is simply absent from the result and the raw `type#id` fallback renders instead.
async fn resolve_object_refs(
    db: &Db,
    org_id: i64,
    events: &[event::Model],
) -> Result<HashMap<(String, i64), ObjectRef>, ApiError> {
    let mut by_type: HashMap<&str, HashSet<i64>> = HashMap::new();
    for e in events {
        if let (Some(t), Some(id)) = (e.object_type.as_deref(), e.object_id) {
            by_type.entry(t).or_default().insert(id);
        }
    }

    let mut out = HashMap::new();

    // Org-scoped types named by their own `name` column.
    macro_rules! named_in_org {
        ($object_type:literal, $entity:ident, |$m:ident| $path:expr) => {
            if let Some(ids) = by_type.get($object_type) {
                for $m in $entity::Entity::find()
                    .filter($entity::Column::OrgId.eq(org_id))
                    .filter($entity::Column::Id.is_in(ids.iter().copied()))
                    .all(db)
                    .await?
                {
                    let path = $path;
                    out.insert(
                        ($object_type.to_string(), $m.id),
                        ObjectRef {
                            name: $m.name,
                            path,
                        },
                    );
                }
            }
        };
    }
    named_in_org!("query", query, |m| Some(format!("/queries/{}", m.id)));
    named_in_org!("monitor", monitor, |m| Some(format!("/monitors/{}", m.id)));
    named_in_org!("data_source", data_source, |m| Some(format!(
        "/data_sources/{}",
        m.id
    )));
    named_in_org!("destination", notification_destination, |m| None);
    named_in_org!("group", group, |m| None);
    named_in_org!("role", role, |m| None);
    named_in_org!("user", user, |m| None);
    if let Some(ids) = by_type.get("organization") {
        for m in organization::Entity::find()
            .filter(organization::Column::Id.is_in(ids.iter().copied()))
            .all(db)
            .await?
        {
            out.insert(
                ("organization".to_string(), m.id),
                ObjectRef {
                    name: m.name,
                    path: Some("/organization".to_string()),
                },
            );
        }
    }
    named_in_org!("dashboard", dashboard, |m| Some(format!(
        "/dashboard/{}",
        m.slug
    )));
    // A visualization has no `org_id` column of its own — it's scoped through the query that
    // owns it, so only visualizations whose query belongs to this org are trusted.
    if let Some(ids) = by_type.get("visualization") {
        let visualizations = visualization::Entity::find()
            .filter(visualization::Column::Id.is_in(ids.iter().copied()))
            .all(db)
            .await?;
        let query_ids: HashSet<i64> = visualizations.iter().map(|v| v.query_id).collect();
        let org_query_ids: HashSet<i64> = if query_ids.is_empty() {
            HashSet::new()
        } else {
            query::Entity::find()
                .filter(query::Column::OrgId.eq(org_id))
                .filter(query::Column::Id.is_in(query_ids))
                .all(db)
                .await?
                .into_iter()
                .map(|q| q.id)
                .collect()
        };
        for m in visualizations {
            if org_query_ids.contains(&m.query_id) {
                out.insert(
                    ("visualization".to_string(), m.id),
                    ObjectRef {
                        name: m.name,
                        path: Some(format!("/queries/{}", m.query_id)),
                    },
                );
            }
        }
    }
    // A widget is a dashboard cell with no name of its own — name it after the dashboard that
    // hosts it, the same substitution the `query_result` block below makes for its owning query.
    if let Some(ids) = by_type.get("widget") {
        let widgets = widget::Entity::find()
            .filter(widget::Column::Id.is_in(ids.iter().copied()))
            .all(db)
            .await?;
        let dashboards =
            org_dashboards(db, org_id, widgets.iter().map(|w| w.dashboard_id).collect()).await?;
        for w in widgets {
            if let Some(d) = dashboards.get(&w.dashboard_id) {
                out.insert(
                    ("widget".to_string(), w.id),
                    ObjectRef {
                        name: d.name.clone(),
                        path: Some(format!("/dashboard/{}", d.slug)),
                    },
                );
            }
        }
    }
    if let Some(ids) = by_type.get("snippet") {
        for m in query_snippet::Entity::find()
            .filter(query_snippet::Column::OrgId.eq(org_id))
            .filter(query_snippet::Column::Id.is_in(ids.iter().copied()))
            .all(db)
            .await?
        {
            out.insert(
                ("snippet".to_string(), m.id),
                ObjectRef {
                    name: m.trigger,
                    path: None,
                },
            );
        }
    }
    // A `query_result` row has no name of its own — it's named after the query it belongs to.
    // `query.latest_query_data_id` is the only link back, so this only resolves a result that is
    // still its query's current one; a result superseded by a later run falls back to
    // `query_result#id` like any other unresolvable object.
    if let Some(ids) = by_type.get("query_result") {
        for m in query::Entity::find()
            .filter(query::Column::OrgId.eq(org_id))
            .filter(query::Column::LatestQueryDataId.is_in(ids.iter().copied()))
            .all(db)
            .await?
        {
            if let Some(rid) = m.latest_query_data_id {
                out.insert(
                    ("query_result".to_string(), rid),
                    ObjectRef {
                        name: m.name,
                        path: Some(format!("/queries/{}", m.id)),
                    },
                );
            }
        }
    }
    // An `embed` also has no name — it's named after the dashboard or query it makes public.
    if let Some(ids) = by_type.get("embed") {
        let embeds = embed::Entity::find()
            .filter(embed::Column::OrgId.eq(org_id))
            .filter(embed::Column::Id.is_in(ids.iter().copied()))
            .all(db)
            .await?;

        let dashboards = org_dashboards(
            db,
            org_id,
            embeds
                .iter()
                .filter(|e| e.object_type == "dashboard")
                .map(|e| e.object_id)
                .collect(),
        )
        .await?;

        let viz_ids: HashSet<i64> = embeds
            .iter()
            .filter(|e| e.object_type == "visualization")
            .map(|e| e.object_id)
            .collect();
        let visualizations: HashMap<i64, visualization::Model> = if viz_ids.is_empty() {
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
        let query_ids_for_viz: HashSet<i64> = visualizations.values().map(|v| v.query_id).collect();
        let queries_for_viz: HashMap<i64, query::Model> = if query_ids_for_viz.is_empty() {
            HashMap::new()
        } else {
            query::Entity::find()
                .filter(query::Column::OrgId.eq(org_id))
                .filter(query::Column::Id.is_in(query_ids_for_viz))
                .all(db)
                .await?
                .into_iter()
                .map(|q| (q.id, q))
                .collect()
        };

        for e in &embeds {
            let target = match e.object_type.as_str() {
                "dashboard" => dashboards.get(&e.object_id).map(|d| ObjectRef {
                    name: d.name.clone(),
                    path: Some(format!("/dashboard/{}", d.slug)),
                }),
                "visualization" => visualizations.get(&e.object_id).and_then(|v| {
                    queries_for_viz.get(&v.query_id).map(|q| ObjectRef {
                        name: q.name.clone(),
                        path: Some(format!("/queries/{}", q.id)),
                    })
                }),
                _ => None,
            };
            if let Some(target) = target {
                out.insert(("embed".to_string(), e.id), target);
            }
        }
    }

    Ok(out)
}

#[derive(Deserialize, Default)]
pub struct EventsQuery {
    pub page: Option<u64>,
    pub page_size: Option<u64>,
    pub user_id: Option<i64>,
    pub action: Option<String>,
    pub object_type: Option<String>,
    /// `YYYY-MM-DD`, inclusive, from the Audit Log page's native date filters — compared against
    /// `created_at` as whole UTC days regardless of the reader's own timezone.
    pub from: Option<String>,
    pub to: Option<String>,
}

fn parse_filter_date(field: &str, s: &str) -> Result<NaiveDate, ApiError> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| ApiError::bad_request(format!("{field} must be YYYY-MM-DD")))
}

fn events_condition(org_id: i64, q: &EventsQuery) -> Result<Condition, ApiError> {
    let mut cond = Condition::all().add(event::Column::OrgId.eq(org_id));
    if let Some(uid) = q.user_id {
        cond = cond.add(event::Column::UserId.eq(uid));
    }
    if let Some(action) = &q.action {
        // Actions render as a "Execute Query"-style label built from the settled dotted
        // convention — match on that normalized form so the filter accepts either separator, and
        // on a substring so a partial word narrows the list. Legacy pre-convention rows are
        // translated to their current name first (`legacy_action_case_sql`), so filtering by a
        // settled name also finds rows written under the old one.
        let needle = action.trim().to_lowercase().replace(['_', '.'], " ");
        let sql = format!(
            r"position($1 in lower(replace(replace(({}), '_', ' '), '.', ' '))) > 0",
            legacy_action_case_sql()
        );
        cond = cond.add(Expr::cust_with_values(sql, [needle]));
    }
    if let Some(ot) = &q.object_type {
        cond = cond.add(event::Column::ObjectType.eq(ot.clone()));
    }
    if let Some(from) = &q.from {
        let start: DateTime<Utc> = parse_filter_date("from", from)?
            .and_hms_opt(0, 0, 0)
            .expect("midnight is always a valid time")
            .and_utc();
        cond = cond.add(event::Column::CreatedAt.gte(start));
    }
    if let Some(to) = &q.to {
        // Exclusive upper bound at the start of the *next* day, so `to` itself is included.
        let end: DateTime<Utc> = (parse_filter_date("to", to)? + chrono::Duration::days(1))
            .and_hms_opt(0, 0, 0)
            .expect("midnight is always a valid time")
            .and_utc();
        cond = cond.add(event::Column::CreatedAt.lt(end));
    }
    Ok(cond)
}

/// `(page_size, offset)` for a `LIMIT`/`OFFSET` pagination query, clamped against an
/// attacker-controlled `page`/`page_size`: `page_size` floors at 1 (a 0 would otherwise multiply
/// out to an always-empty page) and ceilings at 250; the offset multiply saturates instead of
/// wrapping so a huge `page` can't panic a debug build or wrap around in release.
fn page_offset(page: Option<u64>, page_size: Option<u64>) -> (u64, u64) {
    let page_size = page_size.unwrap_or(25).clamp(1, 250);
    let offset = page
        .unwrap_or(1)
        .saturating_sub(1)
        .saturating_mul(page_size);
    (page_size, offset)
}

pub async fn list(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Query(q): Query<EventsQuery>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    super::require_admin(&db, &caller.0).await?;
    let (page_size, offset) = page_offset(q.page, q.page_size);
    let cond = events_condition(org.id, &q)?;

    let total = event::Entity::find()
        .filter(cond.clone())
        .count(&db)
        .await?;

    let events = event::Entity::find()
        .filter(cond)
        .order_by(event::Column::CreatedAt, Order::Desc)
        .limit(page_size)
        .offset(offset)
        .all(&db)
        .await?;

    let user_names =
        display_names::resolve_user_names(&db, org.id, events.iter().filter_map(|e| e.user_id))
            .await?;
    let object_names = resolve_object_refs(&db, org.id, &events).await?;

    let body = events
        .into_iter()
        .map(|e| to_resp(e, &user_names, &object_names))
        .collect::<Vec<_>>();

    // The body stays a plain array (existing callers depend on that shape) — the total is a
    // header, the same convention as e.g. GitHub's `X-Total-Count`, so pagination UI can show
    // "N events" without a breaking response-shape change.
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-total-count",
        total
            .to_string()
            .parse()
            .expect("a decimal number is always a valid header value"),
    );
    Ok((headers, Json(body)))
}

#[cfg(test)]
mod tests {
    use sea_orm::QueryTrait;

    use super::*;

    fn condition_sql(q: &EventsQuery) -> String {
        event::Entity::find()
            .filter(events_condition(1, q).unwrap())
            .build(sea_orm::DatabaseBackend::Postgres)
            .to_string()
    }

    #[test]
    fn page_offset_saturates_instead_of_overflowing() {
        let (page_size, offset) = page_offset(Some(u64::MAX), Some(25));
        assert_eq!(page_size, 25);
        assert_eq!(offset, u64::MAX);
    }

    #[test]
    fn page_offset_clamps_zero_page_size_to_a_non_empty_first_page() {
        let (page_size, offset) = page_offset(Some(1), Some(0));
        assert_eq!(page_size, 1);
        assert_eq!(offset, 0);
    }

    #[test]
    fn parse_filter_date_accepts_iso_date() {
        let d = parse_filter_date("from", "2026-08-06").unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2026, 8, 6).unwrap());
    }

    #[test]
    fn parse_filter_date_rejects_garbage() {
        assert!(parse_filter_date("from", "not-a-date").is_err());
        assert!(parse_filter_date("from", "08/06/2026").is_err());
    }

    #[test]
    fn events_condition_default_scopes_to_org_only() {
        let q = EventsQuery::default();
        // No date/action/user filters supplied: the condition must still exist (org scope) and
        // must not error just because every optional filter is absent.
        assert!(events_condition(1, &q).is_ok());
    }

    #[test]
    fn events_condition_rejects_bad_from_date() {
        let q = EventsQuery {
            from: Some("garbage".to_string()),
            ..Default::default()
        };
        assert!(events_condition(1, &q).is_err());
    }

    #[test]
    fn events_condition_action_filter_is_case_insensitive() {
        let q = EventsQuery {
            action: Some("Execute Query".to_string()),
            ..Default::default()
        };
        let sql = condition_sql(&q);
        assert!(sql.contains("position('execute query' in"));
        assert!(!sql.contains("= 'Execute Query'"));
    }

    #[test]
    fn events_condition_action_filter_normalizes_stored_separators() {
        let q = EventsQuery {
            action: Some("Data Source Created".to_string()),
            ..Default::default()
        };
        let sql = condition_sql(&q);
        assert!(sql.contains("position('data source created' in"));
        assert!(sql.contains("CASE \"action\""));
        assert!(sql.contains("ELSE \"action\" END"));
    }

    #[test]
    fn translate_legacy_action_maps_every_legacy_string() {
        for (old, new) in LEGACY_ACTION_MAP {
            assert_eq!(translate_legacy_action(old), *new);
        }
    }

    #[test]
    fn translate_legacy_action_passes_unknown_strings_through_untouched() {
        assert_eq!(
            translate_legacy_action("dashboard.created"),
            "dashboard.created"
        );
        assert_eq!(translate_legacy_action("something_else"), "something_else");
    }

    #[test]
    fn events_condition_action_filter_matches_a_legacy_row_by_its_current_name() {
        // A row still holding the old bare-snake `login_failed` string must be found when the
        // caller filters by the settled `user.login_failed` name — the whole point of the
        // translation living in the SQL, not just in the response mapper.
        let q = EventsQuery {
            action: Some("user.login_failed".to_string()),
            ..Default::default()
        };
        let sql = condition_sql(&q);
        assert!(sql.contains("WHEN 'login_failed' THEN 'user.login_failed'"));
        assert!(sql.contains("position('user login failed' in"));
    }
}

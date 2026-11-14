//! Saved-query business logic: CRUD, the draft/publish lifecycle, and the cache-or-enqueue
//! execution path. No HTTP types here.
//!
//! Access: admins, or users whose group scopes the query's data source; reads without access 404 (never
//! 403) so existence doesn't leak. Running binds `execute_query` and data-source reach to the same group.
//! `Text`/`QueryDropdown` parameters go into the SQL raw, so queries using them need manage access.

use std::fmt::Write as _;

use chrono::Utc;
use sea_orm::ActiveValue::Set;
use sea_orm::ConnectionTrait;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, Order, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, Statement, TransactionTrait,
};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use arcodash_core::entities::{
    data_source_group, group, monitor, query, query_result, user, visualization,
};
use arcodash_core::events::record_event;
use arcodash_core::hash::gen_query_hash;
use arcodash_core::models::{parse_options, QueryOptions};
use arcodash_core::queue::{JobStatus, Queue};

use crate::authz::authorize::{self, Action, DataSourceVerb, QueryVerb};
use crate::authz::data_source::{any_group_grants_with_reach, in_scope, postures};
use crate::authz::permissions::{load_effective_groups, user_has_permission};
use crate::error::ApiError;
use crate::services::change_service::{self, QuerySnapshot};
use crate::services::parameters;

/// Default cache TTL (seconds) when a request omits `max_age`; `max_age=0` always forces a fresh
/// run.
const DEFAULT_MAX_AGE_SECS: i64 = 24 * 60 * 60;

/// 404s unless `data_source_id` is in the caller's org; then `None` for an admin (no further check
/// needed), else the source's group links. The org check must precede the admin bypass: admin means
/// "everything in their own org", not another org's source.
async fn non_admin_ds_groups(
    db: &impl ConnectionTrait,
    user: &user::Model,
    groups: &[group::Model],
    data_source_id: i64,
) -> Result<Option<Vec<data_source_group::Model>>, ApiError> {
    arcodash_core::entities::data_source::Entity::find_by_id(data_source_id)
        .filter(arcodash_core::entities::data_source::Column::OrgId.eq(user.org_id))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("data source not found"))?;
    if crate::authz::permissions::is_admin(groups) {
        return Ok(None);
    }
    Ok(Some(
        data_source_group::Entity::find()
            .filter(data_source_group::Column::DataSourceId.eq(data_source_id))
            .all(db)
            .await?,
    ))
}

/// `Ok` if `user` has at least View access to `data_source_id`, else a 404 (no-access looks like
/// missing).
pub(crate) async fn require_data_source_access(
    db: &impl ConnectionTrait,
    user: &user::Model,
    groups: &[group::Model],
    data_source_id: i64,
) -> Result<(), ApiError> {
    let Some(dsgs) = non_admin_ds_groups(db, user, groups, data_source_id).await? else {
        return Ok(());
    };
    if in_scope(&postures(groups), data_source_id, &dsgs) {
        Ok(())
    } else {
        Err(ApiError::not_found("data source not found"))
    }
}

/// Execute-only sibling of [`require_data_source_access`]: binds `execute_query` capability and
/// data-source reach to the *same* group, so an unrestricted group can't lend reach to a restricted
/// group's grant.
pub(crate) async fn require_data_source_execute_access(
    db: &impl ConnectionTrait,
    user: &user::Model,
    groups: &[group::Model],
    data_source_id: i64,
) -> Result<(), ApiError> {
    let Some(dsgs) = non_admin_ds_groups(db, user, groups, data_source_id).await? else {
        return Ok(());
    };
    if any_group_grants_with_reach(user, groups, "execute_query", data_source_id, &dsgs) {
        Ok(())
    } else {
        Err(ApiError::not_found("data source not found"))
    }
}

/// Remaps a run-path data-source 404 to a clearer error when the caller holds an explicit ACL grant
/// on the query; create/update keep the opaque 404.
async fn friendly_ds_error_for_run(
    db: &impl ConnectionTrait,
    user: &user::Model,
    groups: &[group::Model],
    query_id: Option<i64>,
    original: ApiError,
) -> Result<ApiError, ApiError> {
    let Some(id) = query_id else {
        return Ok(original);
    };
    let Some(q) = query::Entity::find_by_id(id).one(db).await? else {
        return Ok(original);
    };
    let admin = crate::authz::permissions::is_admin(groups);
    let grants = crate::services::permission_service::list(db, q.org_id, "query", id).await?;
    let has_explicit_grant = crate::authz::access::explicit_role(
        user.id,
        &user.groups,
        admin,
        q.user_id,
        "query",
        id,
        &grants,
    )
    .is_some();
    if has_explicit_grant {
        return Ok(ApiError {
            status: axum::http::StatusCode::FORBIDDEN,
            message: "You have access to this item, but not its data source. Ask an admin for access to the data source to run it.".into(),
            error_code: Some("data_source_access_required".into()),
            extra: None,
        });
    }
    Ok(original)
}

/// Data-source gate for runs enqueued via `run_service::run_now`; same check and friendly remap as
/// `execute`.
pub async fn ensure_run_data_source_access(
    db: &impl ConnectionTrait,
    user: &user::Model,
    data_source_id: i64,
    query_id: i64,
) -> Result<(), ApiError> {
    let groups = load_effective_groups(db, user).await?;
    if let Err(e) = require_data_source_execute_access(db, user, &groups, data_source_id).await {
        return Err(friendly_ds_error_for_run(db, user, &groups, Some(query_id), e).await?);
    }
    Ok(())
}

/// `GET /api/queries` — queries the user can see (data-source reach and a non-`None`
/// `object_role`).
///
/// Archived are excluded unless `include_archived`. `search` is a case-insensitive substring match,
/// `tags` requires all tags, `scope` is `by-me`, `shared-with-me`, or anything else for all.
pub async fn list_for_user(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    include_archived: bool,
    search: Option<String>,
    tags: Option<Vec<String>>,
    scope: Option<&str>,
) -> Result<Vec<query::Model>, ApiError> {
    // Visibility is folded in Rust over the loaded rows, so the load is capped by `candidate_cap`;
    // `tags` goes into SQL.
    let candidate_cap: u64 = arcodash_core::config::env_parsed("LIST_QUERIES_MAX_CANDIDATES", 2000);
    let mut q = query::Entity::find().filter(query::Column::OrgId.eq(org_id));
    if !include_archived {
        q = q.filter(query::Column::IsArchived.eq(false));
    }
    if let Some(ref required_tags) = tags {
        // COALESCE: a NULL `tags` column must act as an empty array (`NULL @> ...` is NULL).
        q = q.filter(sea_orm::sea_query::Expr::cust_with_values(
            "COALESCE(tags, '{}') @> $1::text[]",
            [required_tags.clone()],
        ));
    }
    let all = q
        .order_by(query::Column::Id, Order::Desc)
        .limit(candidate_cap)
        .all(db)
        .await?;
    if all.len() as u64 == candidate_cap {
        tracing::warn!(
            org_id,
            candidate_cap,
            "list_for_user: candidate query load hit its cap; some queries may be missing from \
             this listing"
        );
    }

    let groups = load_effective_groups(db, user).await?;
    let admin = crate::authz::permissions::is_admin(&groups);
    // Capability is the ceiling: without `view_query` nothing lists, whatever the grants.
    let can_view_type = admin || user_has_permission(user, &groups, "view_query");

    // Batch-load grants once (no N+1); `object_role` is evaluated per row in memory.
    let ids: Vec<i64> = all.iter().map(|q| q.id).collect();
    let grants = arcodash_core::entities::access_permission::Entity::find()
        .filter(arcodash_core::entities::access_permission::Column::ObjectType.eq("query"))
        .filter(arcodash_core::entities::access_permission::Column::ObjectId.is_in(ids))
        .all(db)
        .await?;

    let visible: Vec<query::Model> = if admin {
        all
    } else if !can_view_type {
        Vec::new()
    } else {
        let dsgs = data_source_group::Entity::find()
            .filter(data_source_group::Column::GroupId.is_in(user.groups.clone()))
            .all(db)
            .await?;
        let postures = postures(&groups);

        all.into_iter()
            .filter_map(|mut q| {
                let explicit = crate::authz::access::explicit_role(
                    user.id,
                    &user.groups,
                    admin,
                    q.user_id,
                    "query",
                    q.id,
                    &grants,
                );
                // Explicit grant/owner/admin: visible regardless of data-source access. Org-open
                // default: visible only when published/workspace AND the user has data-source
                // access to the underlying source.
                let visible = explicit.is_some()
                    || (crate::authz::access::object_role(
                        user.id,
                        &user.groups,
                        admin,
                        q.user_id,
                        "query",
                        q.id,
                        &q.visibility,
                        !is_published(&q),
                        &grants,
                    )
                    .is_some()
                        && in_scope(&postures, q.data_source_id, &dsgs));
                if !visible {
                    return None;
                }
                // Non-editors get the published content; only editors see the draft on top. Owners
                // keep their own draft even without `edit_query`; grant recipients don't, or they'd
                // read draft SQL the detail route withholds.
                if q.user_id != user.id
                    && !crate::authz::authorize::can_edit_query(user, &groups, admin, explicit)
                {
                    overlay_serving_content(&mut q);
                }
                Some(q)
            })
            .collect()
    };

    // Substring match over what the caller is served (post-overlay), so search can't reveal more
    // than the response.
    let visible: Vec<query::Model> = match search {
        Some(term) => {
            let needle = term.to_lowercase();
            visible
                .into_iter()
                .filter(|q| {
                    q.name.to_lowercase().contains(&needle)
                        || q.description
                            .as_deref()
                            .unwrap_or("")
                            .to_lowercase()
                            .contains(&needle)
                        || q.query.to_lowercase().contains(&needle)
                })
                .collect()
        }
        None => visible,
    };

    Ok(match scope {
        Some("by-me") => visible
            .into_iter()
            .filter(|q| q.user_id == user.id)
            .collect(),
        Some("shared-with-me") => visible
            .into_iter()
            .filter(|q| {
                q.user_id != user.id
                    && crate::authz::access::has_any_grant(
                        user.id,
                        &user.groups,
                        "query",
                        q.id,
                        &grants,
                    )
            })
            .collect(),
        _ => visible,
    })
}

/// `GET /api/queries/tags` — distinct tags across non-archived queries in the org.
pub async fn list_tags(db: &impl ConnectionTrait, org_id: i64) -> Result<Vec<String>, ApiError> {
    let tags: std::collections::BTreeSet<String> = query::Entity::find()
        .filter(query::Column::OrgId.eq(org_id))
        .filter(query::Column::IsArchived.eq(false))
        .all(db)
        .await?
        .into_iter()
        .flat_map(|q| q.tags.unwrap_or_default())
        .collect();
    Ok(tags.into_iter().collect())
}

/// `GET /api/queries/<id>` — access-checked via `authorize` (capability plus reach); 404 on
/// no-access.
pub async fn get_for_user(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    id: i64,
) -> Result<query::Model, ApiError> {
    let q = query::Entity::find_by_id(id)
        .one(db)
        .await?
        .filter(|q| q.org_id == org_id)
        .ok_or_else(|| ApiError::not_found("query not found"))?;

    authorize::authorize(
        db,
        user,
        Action::Query {
            verb: QueryVerb::View,
            id,
        },
    )
    .await?;
    // Reach alone ignores the data-source axis: workspace-open queries also need data-source
    // visibility.
    if !crate::authz::access::can_view_object(db, user, "query", id).await? {
        return Err(ApiError::not_found("query not found"));
    }
    Ok(q)
}

/// Drops parameter definitions whose `{{ name }}` token no longer appears in `sql`, so a stale one
/// can't keep gating the query.
fn prune_orphaned_parameters(options: &mut Value, sql: &str) {
    let live = parameters::referenced_names(sql);
    if let Some(arr) = options.get_mut("parameters").and_then(Value::as_array_mut) {
        arr.retain(|p| {
            p.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| live.contains(n))
        });
    }
}

/// Rejects `options` that don't deserialize into [`QueryOptions`] with a 400 naming the field.
fn validate_options(options: &Value) -> Result<(), ApiError> {
    parse_options::<QueryOptions>(options)
        .map_err(|e| ApiError::bad_request(format!("invalid options: {e}")))?;
    Ok(())
}

/// Trims, drops empties, dedupes (first-seen order); rejects (400) over 25 tags or any tag over 50
/// chars. `None` leaves tags untouched.
fn normalize_tags(tags: Option<Vec<String>>) -> Result<Option<Vec<String>>, ApiError> {
    let Some(tags) = tags else {
        return Ok(None);
    };
    if tags.len() > 25 {
        return Err(ApiError::bad_request("at most 25 tags are allowed"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(tags.len());
    for tag in tags {
        let trimmed = tag.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.len() > 50 {
            return Err(ApiError::bad_request("tags must be 50 characters or fewer"));
        }
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    }
    Ok(Some(out))
}

/// `POST /api/queries` — requires `create_query` plus View access to `data_source_id`. `options`
/// defaults to `{}`.
pub async fn create(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    data_source_id: i64,
    name: String,
    description: Option<String>,
    query_text: String,
    options: Option<Value>,
    tags: Option<Vec<String>>,
) -> Result<query::Model, ApiError> {
    let groups = load_effective_groups(db, user).await?;
    if !user_has_permission(user, &groups, "create_query") {
        return Err(ApiError::forbidden("create_query permission required"));
    }
    require_data_source_access(db, user, &groups, data_source_id).await?;

    let query_hash = gen_query_hash(&query_text, &Value::Null);

    if let Some(ref options) = options {
        validate_options(options)?;
    }
    let mut options = options.unwrap_or_else(|| serde_json::json!({}));
    prune_orphaned_parameters(&mut options, &query_text);

    let tags = normalize_tags(tags)?;

    let model = query::ActiveModel {
        org_id: Set(org_id),
        data_source_id: Set(data_source_id),
        name: Set(name),
        description: Set(description),
        query: Set(query_text),
        query_hash: Set(query_hash),
        api_key: Set(generate_query_api_key()),
        user_id: Set(user.id),
        last_modified_by_id: Set(Some(user.id)),
        latest_query_data_id: Set(None),
        // New queries start unpublished; `publish` is an explicit action.
        is_archived: Set(false),
        options: Set(options),
        tags: Set(tags),
        ..Default::default()
    };

    let saved = model.insert(db).await?;
    update_search_vector(db, &saved).await;
    record_event(
        db,
        org_id,
        Some(user.id),
        "query.created",
        Some("query"),
        Some(saved.id),
        serde_json::json!({ "name": saved.name, "data_source_id": saved.data_source_id }),
    )
    .await?;
    Ok(saved)
}

/// `POST /api/queries/<id>` — updates a draft (requires `edit_query`; a new data source must be
/// View-accessible).
///
/// `options` replaces the whole JSON; `tags`, when `Some`, replace the tag array and apply
/// immediately. `expected_version`, if supplied, must match `working_version` or the update fails
/// with 409; every write bumps it. No `changes` row is written (see [`publish`]).
pub async fn update(
    db: &impl ConnectionTrait,
    user: &user::Model,
    existing: query::Model,
    name: Option<String>,
    description: Option<String>,
    query_text: Option<String>,
    data_source_id: Option<i64>,
    options: Option<Value>,
    is_archived: Option<bool>,
    expected_version: Option<i32>,
    tags: Option<Vec<String>>,
) -> Result<query::Model, ApiError> {
    authorize::authorize(
        db,
        user,
        Action::Query {
            verb: QueryVerb::Edit,
            id: existing.id,
        },
    )
    .await?;
    let groups = load_effective_groups(db, user).await?;

    if expected_version.is_some_and(|v| v != existing.working_version) {
        return Err(conflict_error(existing.working_version));
    }

    if let Some(new_ds_id) = data_source_id {
        require_data_source_access(db, user, &groups, new_ds_id).await?;
    }

    // Field names only (never values): the audit table is org-readable and must not carry SQL text.
    let changed: Vec<&'static str> = [
        ("name", name.is_some()),
        ("description", description.is_some()),
        ("query", query_text.is_some()),
        ("data_source_id", data_source_id.is_some()),
        ("options", options.is_some()),
        ("archived", is_archived.is_some()),
        ("tags", tags.is_some()),
    ]
    .into_iter()
    .filter_map(|(field, present)| present.then_some(field))
    .collect();

    let mut sets: Vec<(&str, sea_orm::Value)> = vec![("last_modified_by_id", user.id.into())];
    if let Some(name) = name {
        sets.push(("name", name.into()));
    }
    if let Some(description) = description {
        sets.push(("description", Some(description).into()));
    }
    if let Some(mut options) = options {
        validate_options(&options)?;
        // The effective SQL (incoming or stored) decides which parameter definitions stay live.
        let effective_sql = query_text.as_deref().unwrap_or(&existing.query);
        prune_orphaned_parameters(&mut options, effective_sql);
        sets.push(("options", options.into()));
    }
    if let Some(query_text) = query_text {
        sets.push((
            "query_hash",
            gen_query_hash(&query_text, &Value::Null).into(),
        ));
        sets.push(("query", query_text.into()));
    }
    if let Some(data_source_id) = data_source_id {
        sets.push(("data_source_id", data_source_id.into()));
    }
    if let Some(is_archived) = is_archived {
        sets.push(("is_archived", is_archived.into()));
    }
    if let Some(tags) = tags {
        let normalized = normalize_tags(Some(tags))?;
        sets.push(("tags", normalized.into()));
    }

    let updated = cas_update(db, existing.id, expected_version, sets).await?;
    update_search_vector(db, &updated).await;
    let mut props = serde_json::json!({ "name": updated.name, "changed": changed });
    if let Some(archived) = is_archived {
        props["archived"] = serde_json::json!(archived);
    }
    record_event(
        db,
        existing.org_id,
        Some(user.id),
        "query.updated",
        Some("query"),
        Some(updated.id),
        props,
    )
    .await?;
    Ok(updated)
}

/// Populates `search_vector` from name, description and query text (no trigger exists). Errors are
/// logged and swallowed.
async fn update_search_vector(db: &impl ConnectionTrait, q: &query::Model) {
    if let Err(e) = db
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE queries SET search_vector = to_tsvector('english', coalesce($1,'') || ' ' || coalesce($2,'') || ' ' || coalesce($3,'')) WHERE id = $4",
            [
                q.name.clone().into(),
                q.description.clone().unwrap_or_default().into(),
                q.query.clone().into(),
                q.id.into(),
            ],
        ))
        .await
    {
        tracing::warn!(query_id = q.id, error = %e, "update_search_vector: failed to update search_vector");
    }
}

/// `DELETE /api/queries/<id>` — requires `edit_query` and Owner-tier reach (stricter than edit).
pub async fn delete(
    db: &impl ConnectionTrait,
    user: &user::Model,
    existing: query::Model,
) -> Result<(), ApiError> {
    authorize::authorize(
        db,
        user,
        Action::Query {
            verb: QueryVerb::Delete,
            id: existing.id,
        },
    )
    .await?;

    query::Entity::delete_by_id(existing.id).exec(db).await?;

    record_event(
        db,
        existing.org_id,
        Some(user.id),
        "query.deleted",
        Some("query"),
        Some(existing.id),
        serde_json::json!({ "name": existing.name }),
    )
    .await?;

    Ok(())
}

/// `GET /api/queries/<id>/version` — version history, oldest first, with 1-indexed numbers;
/// readable by anyone who can read the query.
pub async fn versions(
    db: &impl ConnectionTrait,
    existing: &query::Model,
) -> Result<Vec<change_service::VersionedChange>, ApiError> {
    Ok(change_service::with_version_numbers(
        change_service::list_for_query(db, existing.id).await?,
    ))
}

/// `POST /api/queries/<id>/revert` — restores a historical snapshot's content onto the draft
/// columns, by `change_id` or 1-indexed `version` (see [`RevertTarget`]).
///
/// Requires `edit_query`. `expected_version` must match `working_version` or it fails with 409, so
/// a revert can't silently discard a concurrent edit. Writes no `changes` row and does not publish.
pub async fn revert(
    db: &impl ConnectionTrait,
    user: &user::Model,
    existing: query::Model,
    target: RevertTarget,
    expected_version: Option<i32>,
) -> Result<query::Model, ApiError> {
    authorize::authorize(
        db,
        user,
        Action::Query {
            verb: QueryVerb::Revert,
            id: existing.id,
        },
    )
    .await?;
    let groups = load_effective_groups(db, user).await?;

    if expected_version.is_some_and(|v| v != existing.working_version) {
        return Err(conflict_error(existing.working_version));
    }

    let snapshot = match target {
        RevertTarget::ChangeId(change_id) => {
            change_service::get_snapshot(db, existing.id, change_id).await?
        }
        RevertTarget::Version(version) => {
            change_service::get_snapshot_by_version(db, existing.id, version).await?
        }
    };

    // Same data-source access check as `update`; otherwise a snapshot could leak a data source's
    // id.
    require_data_source_access(db, user, &groups, snapshot.data_source_id).await?;

    let sets: Vec<(&str, sea_orm::Value)> = vec![
        ("name", snapshot.name.into()),
        ("description", snapshot.description.into()),
        (
            "query_hash",
            gen_query_hash(&snapshot.query, &Value::Null).into(),
        ),
        ("query", snapshot.query.into()),
        ("data_source_id", snapshot.data_source_id.into()),
        ("options", snapshot.options.into()),
        ("last_modified_by_id", user.id.into()),
        // The result pointer belongs to the replaced content; null shows "no result yet" instead of
        // stale rows.
        ("latest_query_data_id", None::<i64>.into()),
    ];

    let updated = cas_update(db, existing.id, expected_version, sets).await?;
    update_search_vector(db, &updated).await;
    let to_version = match target {
        RevertTarget::Version(v) => v,
        RevertTarget::ChangeId(change_id) => version_of_change(db, updated.id, change_id).await?,
    };
    record_event(
        db,
        updated.org_id,
        Some(user.id),
        "query.reverted",
        Some("query"),
        Some(updated.id),
        serde_json::json!({ "to_version": to_version }),
    )
    .await?;
    Ok(updated)
}

/// 1-indexed version number of `change_id` in `query_id`'s history; `0` if absent.
#[allow(
    clippy::cast_possible_wrap,
    reason = "a query's version count is nowhere near i64::MAX"
)]
async fn version_of_change(
    db: &impl ConnectionTrait,
    query_id: i64,
    change_id: i64,
) -> Result<i64, ApiError> {
    Ok(change_service::list_for_query(db, query_id)
        .await?
        .iter()
        .position(|c| c.id == change_id)
        .map_or(0, |i| i as i64 + 1))
}

/// Identifies which historical snapshot [`revert`] restores: by `change_id` or 1-indexed `version`.
#[derive(Debug, Clone, Copy)]
pub enum RevertTarget {
    ChangeId(i64),
    Version(i64),
}

/// `POST /api/queries/<id>/fork` — copies `source`'s data source, text, options and visualizations
/// into a new draft named `"Copy of <name>"` owned by `user`, with no history.
///
/// Requires `create_query` plus Viewer reach on `source`.
pub async fn fork(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    source: &query::Model,
) -> Result<query::Model, ApiError> {
    authorize::authorize(
        db,
        user,
        Action::Query {
            verb: QueryVerb::Fork,
            id: source.id,
        },
    )
    .await?;

    // A non-editor may only fork the published content, not the raw draft row.
    let can_edit = authorize::evaluate(
        db,
        user,
        Action::Query {
            verb: QueryVerb::Edit,
            id: source.id,
        },
    )
    .await?
    .can_edit;

    let (name, description, query_text, options, data_source_id) = if can_edit {
        (
            source.name.clone(),
            source.description.clone(),
            source.query.clone(),
            source.options.clone(),
            source.data_source_id,
        )
    } else {
        let resolved = resolve_serving_content(source);
        (
            resolved.name,
            resolved.description,
            resolved.query,
            resolved.options,
            resolved.data_source_id,
        )
    };

    let query_hash = gen_query_hash(&query_text, &Value::Null);

    let model = query::ActiveModel {
        org_id: Set(org_id),
        data_source_id: Set(data_source_id),
        name: Set(format!("Copy of {name}")),
        description: Set(description),
        query: Set(query_text),
        query_hash: Set(query_hash),
        api_key: Set(generate_query_api_key()),
        user_id: Set(user.id),
        last_modified_by_id: Set(Some(user.id)),
        latest_query_data_id: Set(None),
        is_archived: Set(false),
        options: Set(options),
        tags: Set(source.tags.clone()),
        ..Default::default()
    };

    let forked = model.insert(db).await?;

    // Visualizations have no draft/publish split; anyone who can view this query already reads
    // them.
    let source_visualizations = visualization::Entity::find()
        .filter(visualization::Column::QueryId.eq(source.id))
        .all(db)
        .await?;
    for viz in source_visualizations {
        let now = Utc::now();
        visualization::ActiveModel {
            query_id: Set(forked.id),
            r#type: Set(viz.r#type),
            name: Set(viz.name),
            description: Set(viz.description),
            options: Set(viz.options),
            created_at: Set(now.into()),
            updated_at: Set(now.into()),
            ..Default::default()
        }
        .insert(db)
        .await?;
    }

    record_event(
        db,
        org_id,
        Some(user.id),
        "query.forked",
        Some("query"),
        Some(forked.id),
        serde_json::json!({ "source_query_id": source.id }),
    )
    .await?;

    Ok(forked)
}

/// `true` iff `q` is currently live: published and not since unpublished. Pure, no DB.
#[must_use]
pub const fn is_published(q: &query::Model) -> bool {
    arcodash_core::authz::access::query_is_published(q)
}

/// Guard for sites that attach a consumer to a query (monitor, widget, schedule, embed): the query
/// must be live. Pure, no DB.
pub fn require_published(q: &query::Model) -> Result<(), ApiError> {
    if !is_published(q) {
        return Err(ApiError::bad_request("publish this query first"));
    }
    Ok(())
}

/// Result pointer safe for non-editor consumers (embed, monitor): the published pointer while live,
/// `None` once unpublished, never the draft's. Pure, no DB.
#[must_use]
pub fn published_result_pointer(q: &query::Model) -> Option<i64> {
    q.published_latest_query_data_id.filter(|_| is_published(q))
}

/// Content a non-editor read/execution serves: `published_*` once ever published, else the draft.
///
/// An unpublished query keeps its last-published name/SQL/options, but its result pointer goes to
/// `None`, never the draft's. `repoint_published` says which pointer an enqueued execution must
/// repoint. Pure, no DB.
#[derive(Debug, Clone)]
pub struct ResolvedContent {
    pub name: String,
    pub description: Option<String>,
    pub query: String,
    pub options: Value,
    pub data_source_id: i64,
    pub query_hash: String,
    pub latest_query_data_id: Option<i64>,
    pub repoint_published: bool,
}

#[must_use]
pub fn resolve_serving_content(q: &query::Model) -> ResolvedContent {
    if q.published_change_id.is_some() {
        ResolvedContent {
            name: q.published_name.clone().unwrap_or_else(|| q.name.clone()),
            description: q.published_description.clone(),
            query: q.published_query.clone().unwrap_or_else(|| q.query.clone()),
            options: q
                .published_options
                .clone()
                .unwrap_or_else(|| q.options.clone()),
            data_source_id: q.published_data_source_id.unwrap_or(q.data_source_id),
            query_hash: q
                .published_query_hash
                .clone()
                .unwrap_or_else(|| q.query_hash.clone()),
            latest_query_data_id: published_result_pointer(q),
            repoint_published: is_published(q),
        }
    } else {
        ResolvedContent {
            name: q.name.clone(),
            description: q.description.clone(),
            query: q.query.clone(),
            options: q.options.clone(),
            data_source_id: q.data_source_id,
            query_hash: q.query_hash.clone(),
            latest_query_data_id: q.latest_query_data_id,
            repoint_published: false,
        }
    }
}

/// Result pointer for "the last thing that ran": the draft's own pointer wins over the published
/// one.
///
/// Only call directly for callers who passed the Edit-tier `can_edit` check; non-editors must use
/// [`resolve_serving_content`], or they'd receive rows from an editor's draft run. Pure, no DB.
#[must_use]
pub fn resolve_latest_query_data_id(q: &query::Model) -> Option<i64> {
    q.latest_query_data_id
        .or_else(|| resolve_serving_content(q).latest_query_data_id)
}

/// Overlays [`resolve_serving_content`] onto `q`'s display fields so a non-editor sees live
/// content, not pending draft SQL.
pub fn overlay_serving_content(q: &mut query::Model) {
    let resolved = resolve_serving_content(q);
    q.name = resolved.name;
    q.description = resolved.description;
    q.query = resolved.query;
    q.options = resolved.options;
    q.data_source_id = resolved.data_source_id;
    q.query_hash = resolved.query_hash;
    q.latest_query_data_id = resolved.latest_query_data_id;
}

/// `true` iff `q` is published and its draft columns differ from `published_*`. Pure, no DB.
#[must_use]
pub fn has_unpublished_changes(q: &query::Model) -> bool {
    if !is_published(q) {
        return false;
    }
    Some(q.name.as_str()) != q.published_name.as_deref()
        || q.description != q.published_description
        || Some(q.query.as_str()) != q.published_query.as_deref()
        || Some(q.data_source_id) != q.published_data_source_id
        || Some(&q.options) != q.published_options.as_ref()
}

/// 409 for a stale `working_version`.
fn conflict_error(current_working_version: i32) -> ApiError {
    ApiError::conflict_with(
        "this query was changed since you last loaded it",
        serde_json::json!({ "working_version": current_working_version }),
    )
}

/// Applies `sets` to `queries.id = query_id`, bumping `working_version` in one statement
/// conditioned on `expected_version` (atomic compare-and-swap; `None` is last-writer-wins). On a
/// lost race the row is reloaded so the 409 reports the current version.
async fn cas_update(
    db: &impl ConnectionTrait,
    query_id: i64,
    expected_version: Option<i32>,
    sets: Vec<(&str, sea_orm::Value)>,
) -> Result<query::Model, ApiError> {
    let mut params: Vec<sea_orm::Value> = Vec::with_capacity(sets.len() + 2);
    let mut set_clause = String::from("working_version = working_version + 1, updated_at = now()");
    for (col, val) in sets {
        params.push(val);
        let _ = write!(set_clause, ", {col} = ${}", params.len());
    }
    params.push(query_id.into());
    let id_idx = params.len();
    let version_idx = expected_version.map(|v| {
        params.push(v.into());
        params.len()
    });

    let sql = version_idx.map_or_else(
        || format!("UPDATE queries SET {set_clause} WHERE id = ${id_idx}"),
        |vi| {
            format!(
                "UPDATE queries SET {set_clause} WHERE id = ${id_idx} AND working_version = ${vi}"
            )
        },
    );
    let res = db
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            params,
        ))
        .await?;

    if res.rows_affected() == 0 {
        return match version_idx {
            Some(_) => {
                let current = query::Entity::find_by_id(query_id)
                    .one(db)
                    .await?
                    .ok_or_else(|| ApiError::not_found("query not found"))?;
                Err(conflict_error(current.working_version))
            }
            None => Err(ApiError::not_found("query not found")),
        };
    }

    query::Entity::find_by_id(query_id)
        .one(db)
        .await?
        .ok_or_else(|| ApiError::internal("query not found after update"))
}

/// `edit_query` AND Editor-or-above reach on `q`; a pure delegate to `authorize`.
async fn require_edit_role(
    db: &impl ConnectionTrait,
    user: &user::Model,
    q: &query::Model,
) -> Result<(), ApiError> {
    authorize::authorize(
        db,
        user,
        Action::Query {
            verb: QueryVerb::Edit,
            id: q.id,
        },
    )
    .await
}

/// Publish/unpublish/restore/discard need `edit_query` AND Editor-or-above reach on the object (a
/// grant is inert without the capability), and `expected_version` must match `working_version`.
async fn require_edit_at_version(
    db: &impl ConnectionTrait,
    user: &user::Model,
    q: &query::Model,
    expected_version: i32,
) -> Result<(), ApiError> {
    require_edit_role(db, user, q).await?;
    if q.working_version != expected_version {
        return Err(conflict_error(q.working_version));
    }
    Ok(())
}

/// `POST /api/queries/<id>/publish` — copies draft columns into `published_*` and appends a
/// `changes` row.
///
/// First publish also seeds `published_latest_query_data_id` from the draft's pointer. The
/// version-guarded write runs before the `changes` append so a lost race can't orphan a row;
/// `published_change_id` is never cleared afterwards.
pub async fn publish(
    db: &(impl ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    user: &user::Model,
    q: query::Model,
    expected_version: i32,
) -> Result<query::Model, ApiError> {
    require_edit_at_version(db, user, &q, expected_version).await?;

    let first_publish = q.published_change_id.is_none();

    let mut sets: Vec<(&str, sea_orm::Value)> = vec![
        ("published_name", q.name.clone().into()),
        ("published_description", q.description.clone().into()),
        ("published_query", q.query.clone().into()),
        ("published_query_hash", q.query_hash.clone().into()),
        ("published_data_source_id", q.data_source_id.into()),
        ("published_options", q.options.clone().into()),
        ("published_at", Utc::now().into()),
        // (Re)publishing always makes the query live again.
        ("unpublished_at", None::<chrono::DateTime<Utc>>.into()),
    ];
    if first_publish {
        sets.push((
            "published_latest_query_data_id",
            q.latest_query_data_id.into(),
        ));
    }

    // CAS update, `changes` row and pointer flip must land together.
    let txn = db.begin().await?;

    let updated = cas_update(&txn, q.id, Some(expected_version), sets).await?;

    let change = change_service::record(
        &txn,
        q.id,
        user.id,
        &QuerySnapshot {
            name: q.name.clone(),
            description: q.description.clone(),
            query: q.query.clone(),
            data_source_id: q.data_source_id,
            options: q.options.clone(),
            is_archived: q.is_archived,
        },
    )
    .await?;

    let mut active: query::ActiveModel = updated.into();
    active.published_change_id = Set(Some(change.id));
    let published = active.update(&txn).await?;
    record_event(
        &txn,
        q.org_id,
        Some(user.id),
        "query.published",
        Some("query"),
        Some(published.id),
        serde_json::json!({ "name": q.name, "first_publish": first_publish }),
    )
    .await?;
    txn.commit().await?;
    Ok(published)
}

/// Sets or clears the admin verification stamp. The caller must already have passed the admin
/// check. Leaves `working_version` alone so an editor's in-flight draft isn't invalidated.
pub async fn set_verified(
    db: &(impl ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    org_id: i64,
    admin: &user::Model,
    query_id: i64,
    verified: bool,
) -> Result<query::Model, ApiError> {
    let txn = db.begin().await?;
    let q = query::Entity::find_by_id(query_id)
        .filter(query::Column::OrgId.eq(org_id))
        .one(&txn)
        .await?
        .ok_or_else(|| ApiError::not_found("query not found"))?;

    if q.verified_at.is_some() == verified {
        return Ok(q);
    }

    let mut active: query::ActiveModel = q.clone().into();
    if verified {
        active.verified_at = Set(Some(Utc::now().into()));
        active.verified_by = Set(Some(admin.id));
    } else {
        active.verified_at = Set(None);
        active.verified_by = Set(None);
    }
    let updated = active.update(&txn).await?;
    record_event(
        &txn,
        org_id,
        Some(admin.id),
        if verified {
            "query.verified"
        } else {
            "query.unverified"
        },
        Some("query"),
        Some(updated.id),
        serde_json::json!({ "name": q.name }),
    )
    .await?;
    txn.commit().await?;
    Ok(updated)
}

/// `POST /api/queries/<id>/unpublish` — returns a live query to draft by setting `unpublished_at`
/// only; publish history and [`restore`] survive. Writes no `changes` row; errors if the query
/// isn't live.
pub async fn unpublish(
    db: &impl ConnectionTrait,
    user: &user::Model,
    q: query::Model,
    expected_version: i32,
) -> Result<query::Model, ApiError> {
    require_edit_at_version(db, user, &q, expected_version).await?;
    if !is_published(&q) {
        return Err(ApiError::bad_request("query is not currently published"));
    }

    let updated = cas_update(
        db,
        q.id,
        Some(expected_version),
        vec![("unpublished_at", Utc::now().into())],
    )
    .await?;
    record_event(
        db,
        q.org_id,
        Some(user.id),
        "query.unpublished",
        Some("query"),
        Some(updated.id),
        serde_json::json!({ "name": q.name }),
    )
    .await?;
    Ok(updated)
}

/// `POST /api/queries/<id>/restore` — promotes a past publish point to the current published
/// version and resets the draft to match.
///
/// History is append-only: records a NEW `changes` row. Only valid once published; same
/// CAS-then-append order as [`publish`].
pub async fn restore(
    db: &(impl ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    user: &user::Model,
    q: query::Model,
    change_id: i64,
    expected_version: i32,
) -> Result<query::Model, ApiError> {
    require_edit_at_version(db, user, &q, expected_version).await?;
    if q.published_change_id.is_none() {
        return Err(ApiError::bad_request(
            "query has never been published — nothing to restore",
        ));
    }

    let snapshot = change_service::get_snapshot(db, q.id, change_id).await?;

    // Same data-source access check as `revert`.
    let groups = load_effective_groups(db, user).await?;
    require_data_source_access(db, user, &groups, snapshot.data_source_id).await?;

    let query_hash = gen_query_hash(&snapshot.query, &Value::Null);
    let sets: Vec<(&str, sea_orm::Value)> = vec![
        ("name", snapshot.name.clone().into()),
        ("description", snapshot.description.clone().into()),
        ("query", snapshot.query.clone().into()),
        ("query_hash", query_hash.clone().into()),
        ("data_source_id", snapshot.data_source_id.into()),
        ("options", snapshot.options.clone().into()),
        ("published_name", snapshot.name.clone().into()),
        ("published_description", snapshot.description.clone().into()),
        ("published_query", snapshot.query.clone().into()),
        ("published_query_hash", query_hash.into()),
        ("published_data_source_id", snapshot.data_source_id.into()),
        ("published_options", snapshot.options.clone().into()),
        ("published_at", Utc::now().into()),
        // Restoring makes the query live again.
        ("unpublished_at", None::<chrono::DateTime<Utc>>.into()),
        // Restore alone rewrites `published_*`, so it must also clear the published pointer.
        ("latest_query_data_id", None::<i64>.into()),
        ("published_latest_query_data_id", None::<i64>.into()),
    ];

    // Same atomicity requirement as `publish`.
    let txn = db.begin().await?;

    let updated = cas_update(&txn, q.id, Some(expected_version), sets).await?;

    let change = change_service::record(&txn, q.id, user.id, &snapshot).await?;

    let mut active: query::ActiveModel = updated.into();
    active.published_change_id = Set(Some(change.id));
    let updated = active.update(&txn).await?;
    update_search_vector(&txn, &updated).await;
    let from_version = version_of_change(&txn, updated.id, change_id).await?;
    record_event(
        &txn,
        updated.org_id,
        Some(user.id),
        "query.restored",
        Some("query"),
        Some(updated.id),
        serde_json::json!({ "from_version": from_version }),
    )
    .await?;
    txn.commit().await?;
    Ok(updated)
}

/// `POST /api/queries/<id>/discard` — resets the draft columns to the published content. Only valid
/// once published.
pub async fn discard_changes(
    db: &impl ConnectionTrait,
    user: &user::Model,
    q: query::Model,
    expected_version: i32,
) -> Result<query::Model, ApiError> {
    require_edit_at_version(db, user, &q, expected_version).await?;
    if q.published_change_id.is_none() {
        return Err(ApiError::bad_request(
            "query has never been published — nothing to discard back to",
        ));
    }

    // `publish` writes the `published_*` content fields together, so they're `Some` here.
    let sets: Vec<(&str, sea_orm::Value)> = vec![
        ("name", q.published_name.clone().into()),
        ("description", q.published_description.clone().into()),
        ("query", q.published_query.clone().into()),
        ("query_hash", q.published_query_hash.clone().into()),
        ("data_source_id", q.published_data_source_id.into()),
        ("options", q.published_options.clone().into()),
        // The pointer came from the discarded edit; null avoids showing rows from replaced SQL.
        ("latest_query_data_id", None::<i64>.into()),
    ];
    let updated = cas_update(db, q.id, Some(expected_version), sets).await?;
    update_search_vector(db, &updated).await;
    record_event(
        db,
        q.org_id,
        Some(user.id),
        "query.discarded",
        Some("query"),
        Some(updated.id),
        serde_json::json!({ "name": q.name }),
    )
    .await?;
    Ok(updated)
}

/// Result of [`execute`]: cache hit, enqueued job, or parked behind the data source's concurrency
/// cap.
#[derive(Debug, Clone)]
pub enum ExecuteOutcome {
    Cached(Box<query_result::Model>),
    Enqueued {
        job_id: String,
    },
    /// Accepted and durable but held behind the concurrency cap; starts when a slot frees.
    Parked {
        job_id: String,
    },
}

/// Cache-or-enqueue execution shared by `POST /api/queries/<id>/results` and `POST
/// /api/query_results`. Requires `execute_query` plus View access to `data_source_id`.
///
/// - `query_text` is the raw query; `options.parameters` definitions render `parameters` into it before
///   hashing/enqueueing, so the worker gets final SQL and `query_hash` reflects it. `options` is never an
///   authorization input.
/// - `Text`/`QueryDropdown` parameters still referenced by `query_text` require manage access to the data source.
/// - `max_age`: `Some(0)` forces a fresh run; `Some(n)` reuses a result within `n` seconds; `None` uses
///   [`DEFAULT_MAX_AGE_SECS`].
/// - `query_id`: authoritative binding; execution state and result pointer are kept in sync with this run.
/// - `acl_query_id`: an ad-hoc run's candidate query; promoted to `query_id` if the content matches the draft
///   exactly (see [`promotes_to_bound_execution`]).
pub async fn execute(
    db: &impl ConnectionTrait,
    queue: &dyn Queue,
    conveyor: Option<&arcodash_core::Conveyor>,
    redis: &redis::aio::ConnectionManager,
    org_id: i64,
    user: &user::Model,
    data_source_id: i64,
    query_text: String,
    options: Value,
    parameters: Value,
    max_age: Option<i64>,
    query_id: Option<i64>,
    acl_query_id: Option<i64>,
    repoint_published: bool,
    max_query_rows: u32,
) -> Result<ExecuteOutcome, ApiError> {
    let groups = load_effective_groups(db, user).await?;

    // Ad-hoc runs whose content equals the associated query's draft are promoted to a bound
    // execution so the worker updates that query's state; any real difference stays unbound.
    let mut query_id = query_id;
    let mut promoted_row: Option<query::Model> = None;
    // Callers with a bound `query_id` resolved it themselves; a promoted ad-hoc run derives it
    // below once the row is loaded.
    let mut repoint_published = repoint_published;
    if query_id.is_none() {
        if let Some(id) = acl_query_id {
            if let Some(row) = query::Entity::find_by_id(id)
                .one(db)
                .await?
                .filter(|q| q.org_id == org_id)
            {
                if promotes_to_bound_execution(
                    row.org_id,
                    &row.query,
                    row.data_source_id,
                    org_id,
                    &query_text,
                    data_source_id,
                ) {
                    // Promotion compares against the DRAFT only; repoint the published pointer only
                    // if the content also matches the resolved serving content, else this is a
                    // draft-only run.
                    let resolved = resolve_serving_content(&row);
                    repoint_published = resolved.repoint_published
                        && resolved.query == query_text
                        && resolved.data_source_id == data_source_id;
                    query_id = Some(id);
                    promoted_row = Some(row);
                }
            }
        }
    }

    // Saved queries also need Viewer reach on the query itself; ad-hoc execution has no object and
    // stays capability-only.
    match query_id {
        Some(id) => {
            authorize::authorize(
                db,
                user,
                Action::Query {
                    verb: QueryVerb::Execute,
                    id,
                },
            )
            .await?;
        }
        None if !user_has_permission(user, &groups, "execute_query") => {
            return Err(ApiError::forbidden("execute_query permission required"));
        }
        None => {}
    }
    if let Err(e) = require_data_source_execute_access(db, user, &groups, data_source_id).await {
        return Err(
            friendly_ds_error_for_run(db, user, &groups, query_id.or(acl_query_id), e).await?,
        );
    }

    // A malformed `options` blob is a 400, not a silent default: defaults would drop parameter
    // definitions and skip the gate below.
    let query_options: QueryOptions = parse_options(&options).map_err(|e| {
        tracing::warn!(query_id = ?query_id, error = %e, "execute: options failed to parse");
        ApiError::bad_request(format!("this query's saved options can't be read: {e}"))
    })?;

    // Text/QueryDropdown values go into the SQL unescaped, so running such a query requires
    // data-source manage access.
    if let Some(param) = parameters::manage_access_param(&query_text, &query_options.parameters) {
        let admin = crate::authz::permissions::is_admin(&groups);
        let can_manage = admin
            || authorize::evaluate_with_groups(
                db,
                user,
                &groups,
                admin,
                Action::DataSource {
                    verb: DataSourceVerb::Edit,
                    id: data_source_id,
                },
            )
            .await?
            .can_edit;
        if !can_manage {
            return Err(ApiError::forbidden(
                format!(
                    "this query's \"{param}\" parameter is inserted into the SQL unescaped, so anyone who can set its value could run arbitrary SQL — running it requires manage access to the data source. Use a Number, Date, or Dropdown parameter for a checkable value, or ask someone with manage access to run it."
                ),
            ));
        }
    }

    // Always render: a query with `{{ }}` tokens but no definitions must fail loudly rather than
    // run the literal text.
    let rendered_text =
        parameters::render_query(&query_text, &query_options.parameters, &parameters)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let query_hash = gen_query_hash(&rendered_text, &parameters);

    if max_age != Some(0) {
        let cached = find_fresh_cached_result(
            db,
            org_id,
            data_source_id,
            &query_hash,
            max_age.unwrap_or(DEFAULT_MAX_AGE_SECS),
            max_query_rows,
        )
        .await?;
        crate::metrics::incr_query_result_cache_lookup(cached.is_some());
        if let Some(cached) = cached {
            // Cache hits never reach the worker (the sole writer of execution state), so sync state
            // here; `working_version` is untouched.
            if let Some(id) = query_id {
                sync_cached_hit_state(db, org_id, id, promoted_row, repoint_published, &cached)
                    .await?;
            }
            crate::metrics::incr_query_execution("interactive", "cached");
            return Ok(ExecuteOutcome::Cached(Box::new(cached)));
        }
    }

    crate::services::data_source_service::require_not_paused(db, data_source_id).await?;

    let job_id = Uuid::new_v4().to_string();
    arcodash_core::queue::set_job_status(
        redis,
        &job_id,
        org_id,
        Some(user.id),
        JobStatus::Pending,
        None,
        None,
    )
    .await
    .map_err(|e| ApiError::internal(format!("failed to record job status: {e}")))?;

    let queued = arcodash_core::query_exec::enqueue_execute_query(
        db,
        queue,
        conveyor,
        arcodash_core::query_exec::ExecuteQueryPayload {
            job_id: job_id.clone(),
            org_id,
            data_source_id,
            query_text: rendered_text,
            parameters,
            user_id: user.id,
            query_id,
            scheduled: false,
            run_id: None,
            repoint_published,
            max_rows: Some(max_query_rows),
        },
    )
    .await
    .map_err(|e| ApiError::internal(format!("failed to enqueue query execution: {e}")))?;
    if !queued {
        // Interactive executions have no entity id, so only the data-source cap can park them;
        // report `Parked` so pollers don't see Pending until the Redis TTL expires.
        tracing::info!(
            job_id = %job_id,
            org_id,
            query_id = ?query_id,
            "interactive execution parked over the data source concurrency cap"
        );
        crate::metrics::incr_query_execution("interactive", "parked");
        return Ok(ExecuteOutcome::Parked { job_id });
    }

    crate::metrics::incr_query_execution("interactive", "enqueued");
    Ok(ExecuteOutcome::Enqueued { job_id })
}

/// Whether an ad-hoc run's raw content exactly equals the query's current draft, which promotes it
/// to a bound execution. Exact string equality (no trimming): `query_hash` is computed from
/// unrendered SQL, and a false negative just leaves the run unbound while a false positive could
/// repoint the result pointer wrongly.
fn promotes_to_bound_execution(
    row_org_id: i64,
    row_query: &str,
    row_data_source_id: i64,
    org_id: i64,
    query_text: &str,
    data_source_id: i64,
) -> bool {
    row_org_id == org_id && row_query == query_text && row_data_source_id == data_source_id
}

/// After a cache hit for a bound execution, syncs the result pointer and execution-state columns
/// with the result served.
async fn sync_cached_hit_state(
    db: &impl ConnectionTrait,
    org_id: i64,
    query_id: i64,
    preloaded: Option<query::Model>,
    repoint_published: bool,
    cached: &query_result::Model,
) -> Result<(), ApiError> {
    let row = match preloaded.filter(|row| row.id == query_id) {
        Some(row) => row,
        None => match query::Entity::find_by_id(query_id)
            .one(db)
            .await?
            .filter(|q| q.org_id == org_id)
        {
            Some(row) => row,
            None => return Ok(()),
        },
    };

    let (current_pointer, pointer_column) = if repoint_published {
        (
            row.published_latest_query_data_id,
            "published_latest_query_data_id",
        )
    } else {
        (row.latest_query_data_id, "latest_query_data_id")
    };
    if current_pointer == Some(cached.id) {
        return Ok(());
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "stored result rows are capped by max_query_rows, well below i32::MAX"
    )]
    let row_count = cached
        .data
        .as_ref()
        .and_then(|d| d["rows"].as_array())
        .map_or(0, |r| r.len() as i32);

    // Only moves the pointer forward: a cache hit may serve an older result than the latest real
    // execution.
    // COALESCE: a purged `query_results` row makes the subquery NULL, which would otherwise leave a
    // dangling pointer stuck.
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "UPDATE queries SET {pointer_column} = $1 \
             WHERE id = $2 \
               AND ({pointer_column} IS NULL \
                    OR COALESCE((SELECT retrieved_at FROM query_results WHERE id = {pointer_column}), \
                                '-infinity'::timestamptz) <= $3)"
        ),
        [
            cached.id.into(),
            query_id.into(),
            cached.retrieved_at.into(),
        ],
    ))
    .await?;

    // Only moves `last_execution_at` forward so an older success can't overwrite a newer failure.
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE queries SET last_execution_status = 'Success', last_execution_at = $1, \
         row_count = $2, last_execution_error = NULL \
         WHERE id = $3 AND (last_execution_at IS NULL OR last_execution_at <= $1)",
        [
            cached.retrieved_at.into(),
            row_count.into(),
            query_id.into(),
        ],
    ))
    .await?;
    Ok(())
}

/// Whether the worker's row cap truncated `data` at write time, and how many rows it holds.
fn truncated_row_count(data: &Value) -> Option<usize> {
    (data["truncated"] == true).then(|| data["rows"].as_array().map_or(0, Vec::len))
}

/// Returns the `query_results` row for `(org_id, data_source_id, query_hash)` if fresher than
/// `max_age_secs` and, when truncated, its row count still matches the current `max_query_rows`.
async fn find_fresh_cached_result(
    db: &impl ConnectionTrait,
    org_id: i64,
    data_source_id: i64,
    query_hash: &str,
    max_age_secs: i64,
    max_query_rows: u32,
) -> Result<Option<query_result::Model>, ApiError> {
    let existing = query_result::Entity::find()
        .filter(query_result::Column::OrgId.eq(org_id))
        .filter(query_result::Column::DataSourceId.eq(data_source_id))
        .filter(query_result::Column::QueryHash.eq(query_hash))
        .order_by(query_result::Column::RetrievedAt, Order::Desc)
        .one(db)
        .await?;

    Ok(existing.filter(|result| {
        let age_secs = (Utc::now() - result.retrieved_at.with_timezone(&Utc)).num_seconds();
        #[allow(
            clippy::cast_possible_truncation,
            reason = "truncated row counts are at most a max_query_rows cap, which is a u32"
        )]
        let truncated_to_other_cap = result
            .data
            .as_ref()
            .and_then(truncated_row_count)
            .is_some_and(|row_count| row_count as u32 != max_query_rows);
        age_secs <= max_age_secs && !truncated_to_other_cap
    }))
}

/// One `{name, value}` option pair for a Query-based Dropdown parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterValueOption {
    pub name: String,
    pub value: Value,
}

/// Result of [`parameter_values`]: resolved options, or a just-enqueued source query to poll via
/// `/api/jobs/<id>`.
#[derive(Debug, Clone)]
pub enum ParameterValuesOutcome {
    Options(Vec<ParameterValueOption>),
    Enqueued { job_id: String },
}

/// `GET /api/queries/<id>/parameter_values` — options for a `QueryDropdown` parameter
/// `parameter_name`, read from the source query's latest cached result.
///
/// - With `name` and `value` columns, each row becomes `{name, value}`.
/// - With exactly one column, it serves as both.
/// - Any other shape is a 422-equivalent [`ApiError`].
///
/// With no cached result it enqueues the source query and returns
/// [`ParameterValuesOutcome::Enqueued`].
pub async fn parameter_values(
    db: &impl ConnectionTrait,
    queue: &dyn Queue,
    conveyor: Option<&arcodash_core::Conveyor>,
    redis: &redis::aio::ConnectionManager,
    org_id: i64,
    user: &user::Model,
    query_id: i64,
    parameter_name: &str,
    max_query_rows: u32,
) -> Result<ParameterValuesOutcome, ApiError> {
    let mut q = get_for_user(db, org_id, user, query_id).await?;
    // Non-editors only see definitions from the content they're served, not unpublished draft ones.
    let can_edit = authorize::evaluate(
        db,
        user,
        Action::Query {
            verb: QueryVerb::Edit,
            id: query_id,
        },
    )
    .await?
    .can_edit;
    if !can_edit {
        overlay_serving_content(&mut q);
    }
    let options: QueryOptions = parse_options(&q.options).map_err(|e| {
        ApiError::bad_request(format!("this query's saved options can't be read: {e}"))
    })?;

    let definition = options
        .parameters
        .iter()
        .find(|p| p.name == parameter_name)
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "no parameter named \"{parameter_name}\" on this query"
            ))
        })?;

    let source_query_id = definition.query_id.ok_or_else(|| {
        ApiError::bad_request(format!(
            "parameter \"{parameter_name}\" is not a query-based dropdown (no query_id set)"
        ))
    })?;

    let source = get_for_user(db, org_id, user, source_query_id).await?;
    // Viewers use the source query's served content; never read a run of its unpublished draft.
    let resolved = resolve_serving_content(&source);

    if let Some(cached) = find_fresh_cached_result(
        db,
        org_id,
        resolved.data_source_id,
        &resolved.query_hash,
        i64::MAX,
        max_query_rows,
    )
    .await?
    {
        return Ok(ParameterValuesOutcome::Options(extract_name_value_options(
            &cached,
        )?));
    }

    // No cached result: enqueue a parameter-free run and tell the caller to poll.
    match execute(
        db,
        queue,
        conveyor,
        redis,
        org_id,
        user,
        resolved.data_source_id,
        resolved.query.clone(),
        resolved.options.clone(),
        Value::Null,
        None,
        Some(source.id),
        None,
        resolved.repoint_published,
        max_query_rows,
    )
    .await?
    {
        ExecuteOutcome::Cached(result) => Ok(ParameterValuesOutcome::Options(
            extract_name_value_options(&result)?,
        )),
        // Parked is still a pollable accepted job.
        ExecuteOutcome::Enqueued { job_id } | ExecuteOutcome::Parked { job_id } => {
            Ok(ParameterValuesOutcome::Enqueued { job_id })
        }
    }
}

/// Applies the `name`/`value` column convention of [`parameter_values`] to a result's `data` JSON.
fn extract_name_value_options(
    result: &query_result::Model,
) -> Result<Vec<ParameterValueOption>, ApiError> {
    let data = result
        .data
        .as_ref()
        .ok_or_else(|| ApiError::internal("source query's cached result has no data"))?;

    let columns: Vec<&str> = data["columns"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|c| c["name"].as_str()).collect())
        .unwrap_or_default();

    let rows = data["rows"]
        .as_array()
        .ok_or_else(|| ApiError::internal("source query's cached result has no rows array"))?;

    let has_name_value = columns.contains(&"name") && columns.contains(&"value");

    if has_name_value {
        Ok(rows
            .iter()
            .filter_map(|row| {
                let name = row.get("name")?.as_str()?.to_string();
                let value = row.get("value")?.clone();
                Some(ParameterValueOption { name, value })
            })
            .collect())
    } else if columns.len() == 1 {
        let col = columns[0];
        Ok(rows
            .iter()
            .filter_map(|row| {
                let value = row.get(col)?.clone();
                let name = match &value {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                Some(ParameterValueOption { name, value })
            })
            .collect())
    } else {
        Err(ApiError::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "query-based dropdown source query must return either a \"name\"+\"value\" column pair or exactly one column",
        ))
    }
}

/// Impact counts for a query. `query_id` must already be access-checked (View reach).
#[derive(Debug, Clone, Serialize)]
pub struct QueryConsumers {
    /// Widgets whose visualization targets this query.
    pub tile_count: i64,
    /// Distinct dashboards those widgets live on.
    pub dashboard_count: i64,
    pub monitor_count: i64,
}

pub async fn consumers(
    db: &impl ConnectionTrait,
    org_id: i64,
    query_id: i64,
) -> Result<QueryConsumers, ApiError> {
    query::Entity::find_by_id(query_id)
        .filter(query::Column::OrgId.eq(org_id))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("query not found"))?;

    // `query_id` is already org-checked; widgets/visualizations have no org_id of their own.
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS tiles, count(DISTINCT w.dashboard_id) AS dashboards \
             FROM widgets w JOIN visualizations v ON v.id = w.visualization_id \
             WHERE v.query_id = $1",
            [query_id.into()],
        ))
        .await?;
    let (tile_count, dashboard_count) = match row {
        Some(r) => (
            r.try_get::<i64>("", "tiles")?,
            r.try_get::<i64>("", "dashboards")?,
        ),
        None => (0, 0),
    };

    let monitor_count = monitor::Entity::find()
        .filter(monitor::Column::OrgId.eq(org_id))
        .filter(monitor::Column::QueryId.eq(query_id))
        .count(db)
        .await?
        .cast_signed();

    Ok(QueryConsumers {
        tile_count,
        dashboard_count,
        monitor_count,
    })
}

/// Populates the NOT NULL, UNIQUE `queries.api_key` column; not used for authentication.
fn generate_query_api_key() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stale cache hit arriving after a fresher one must not regress `latest_query_data_id`.
    /// Needs `TEST_DATABASE_URL`.
    #[tokio::test]
    async fn sync_cached_hit_state_never_regresses_the_pointer_to_an_older_result() {
        let Ok(db_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let db = arcodash_core::db::connect(&db_url).await.unwrap();

        let tag = uuid::Uuid::new_v4();
        let mut org = arcodash_core::test_support::new_organization();
        org.slug = Set(format!("stale-hit-{tag}"));
        let org = org.insert(&db).await.unwrap();
        let mut user = arcodash_core::test_support::new_user(org.id);
        user.email = Set(format!("stale-hit-{tag}@test.local"));
        user.api_key_hash = Set(arcodash_core::hash::hash_api_key(&format!(
            "stale-hit-{tag}"
        )));
        let user = user.insert(&db).await.unwrap();
        let ds = arcodash_core::test_support::new_data_source(org.id)
            .insert(&db)
            .await
            .unwrap();
        let query = arcodash_core::test_support::new_query(org.id, ds.id, user.id)
            .insert(&db)
            .await
            .unwrap();

        let now = Utc::now();
        let newer = arcodash_core::entities::query_result::ActiveModel {
            org_id: Set(org.id),
            data_source_id: Set(ds.id),
            query_hash: Set("hash-newer".to_string()),
            query: Set("SELECT 1".to_string()),
            data: Set(Some(serde_json::json!({ "columns": [], "rows": [] }))),
            data_location: Set(None),
            data_size_bytes: Set(None),
            runtime: Set(Some(0.01)),
            retrieved_at: Set(now.into()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        let older = arcodash_core::entities::query_result::ActiveModel {
            org_id: Set(org.id),
            data_source_id: Set(ds.id),
            query_hash: Set("hash-older".to_string()),
            query: Set("SELECT 1".to_string()),
            data: Set(Some(serde_json::json!({ "columns": [], "rows": [] }))),
            data_location: Set(None),
            data_size_bytes: Set(None),
            runtime: Set(Some(0.01)),
            retrieved_at: Set((now - chrono::Duration::seconds(30)).into()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        sync_cached_hit_state(&db, org.id, query.id, None, false, &newer)
            .await
            .unwrap();
        sync_cached_hit_state(&db, org.id, query.id, None, false, &older)
            .await
            .unwrap();

        let reloaded = query::Entity::find_by_id(query.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded.latest_query_data_id,
            Some(newer.id),
            "a stale cache hit landing after a fresher one must not regress the pointer"
        );
    }

    /// A missing `query_results` row must not freeze the pointer guard: the NULL subquery loses the
    /// `<=` via `COALESCE`. Drops the FK inside a transaction that is rolled back.
    #[tokio::test]
    async fn cache_hit_repoints_past_a_dangling_pointer() {
        let Ok(db_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let db = arcodash_core::db::connect(&db_url).await.unwrap();

        let tag = uuid::Uuid::new_v4();
        let mut org = arcodash_core::test_support::new_organization();
        org.slug = Set(format!("dangling-ptr-{tag}"));
        let org = org.insert(&db).await.unwrap();
        let mut user = arcodash_core::test_support::new_user(org.id);
        user.email = Set(format!("dangling-ptr-{tag}@test.local"));
        user.api_key_hash = Set(arcodash_core::hash::hash_api_key(&format!(
            "dangling-ptr-{tag}"
        )));
        let user = user.insert(&db).await.unwrap();
        let ds = arcodash_core::test_support::new_data_source(org.id)
            .insert(&db)
            .await
            .unwrap();
        let query = arcodash_core::test_support::new_query(org.id, ds.id, user.id)
            .insert(&db)
            .await
            .unwrap();

        let now = Utc::now();
        let fresh = arcodash_core::entities::query_result::ActiveModel {
            org_id: Set(org.id),
            data_source_id: Set(ds.id),
            query_hash: Set("hash-fresh".to_string()),
            query: Set("SELECT 1".to_string()),
            data: Set(Some(serde_json::json!({ "columns": [], "rows": [] }))),
            data_location: Set(None),
            data_size_bytes: Set(None),
            runtime: Set(Some(0.01)),
            retrieved_at: Set(now.into()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let txn = db.begin().await.unwrap();
        txn.execute_unprepared("SET LOCAL session_replication_role = replica")
            .await
            .unwrap();
        let dangling_pointer_id: i64 = fresh.id + 1_000_000; // never a real query_results row
        txn.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE queries SET latest_query_data_id = $1 WHERE id = $2",
            [dangling_pointer_id.into(), query.id.into()],
        ))
        .await
        .unwrap();

        sync_cached_hit_state(&txn, org.id, query.id, None, false, &fresh)
            .await
            .unwrap();

        let reloaded = query::Entity::find_by_id(query.id)
            .one(&txn)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded.latest_query_data_id,
            Some(fresh.id),
            "a dangling pointer must not block a cache hit from repointing"
        );

        txn.rollback().await.unwrap();
    }

    #[test]
    fn is_admin_detects_admin_group() {
        let admin_group =
            arcodash_core::test_support::group_model(1, 1, "admin", vec!["admin".to_string()]);
        let regular_group = arcodash_core::test_support::group_model(
            2,
            1,
            "default",
            vec!["view_query".to_string()],
        );
        assert!(crate::authz::permissions::is_admin(std::slice::from_ref(
            &admin_group
        )));
        assert!(!crate::authz::permissions::is_admin(&[regular_group]));
        assert!(crate::authz::permissions::is_admin(&[admin_group]));
    }

    #[test]
    fn generate_query_api_key_produces_unique_nonempty_values() {
        let a = generate_query_api_key();
        let b = generate_query_api_key();
        assert_ne!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn prune_orphaned_parameters_drops_a_dead_definition_keeps_a_live_one() {
        let mut options = serde_json::json!({
            "parameters": [
                {"name": "gone", "type": "text"},
                {"name": "kept", "type": "text"}
            ]
        });
        prune_orphaned_parameters(&mut options, "SELECT '{{ kept }}'");
        let names: Vec<&str> = options["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["kept"]);
    }

    #[test]
    fn prune_orphaned_parameters_is_a_no_op_when_every_token_is_still_live() {
        let mut options = serde_json::json!({
            "parameters": [{"name": "a", "type": "number"}, {"name": "b", "type": "number"}]
        });
        prune_orphaned_parameters(&mut options, "{{ a }} + {{ b }}");
        assert_eq!(options["parameters"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn normalize_tags_passes_none_through() {
        assert_eq!(normalize_tags(None).unwrap(), None);
    }

    #[test]
    fn query_options_with_a_bad_max_age_field_fails_to_parse() {
        // Pins all-or-nothing deserialization: one malformed field fails the whole value.
        let bad = serde_json::json!({ "max_age": "60" });
        assert!(parse_options::<QueryOptions>(&bad).is_err());
        assert!(validate_options(&bad).is_err());

        let good = serde_json::json!({ "max_age": 60 });
        assert!(validate_options(&good).is_ok());
    }

    #[test]
    fn normalize_tags_trims_drops_empties_and_dedupes_preserving_order() {
        let got = normalize_tags(Some(vec![
            "  x  ".to_string(),
            "x".to_string(),
            "".to_string(),
            "  ".to_string(),
            "y".to_string(),
        ]))
        .unwrap();
        assert_eq!(got, Some(vec!["x".to_string(), "y".to_string()]));
    }

    #[test]
    fn normalize_tags_rejects_more_than_25_tags() {
        let tags: Vec<String> = (0..26).map(|i| format!("t{i}")).collect();
        assert!(normalize_tags(Some(tags)).is_err());
    }

    #[test]
    fn promotes_to_bound_execution_matches_identical_content_same_org() {
        assert!(promotes_to_bound_execution(
            1, "SELECT 1", 10, 1, "SELECT 1", 10,
        ));
    }

    #[test]
    fn promotes_to_bound_execution_rejects_text_mismatch() {
        assert!(!promotes_to_bound_execution(
            1, "SELECT 1", 10, 1, "SELECT 2", 10,
        ));
    }

    #[test]
    fn promotes_to_bound_execution_rejects_data_source_mismatch() {
        assert!(!promotes_to_bound_execution(
            1, "SELECT 1", 10, 1, "SELECT 1", 99,
        ));
    }

    #[test]
    fn promotes_to_bound_execution_rejects_wrong_org() {
        assert!(!promotes_to_bound_execution(
            1, "SELECT 1", 10, 2, "SELECT 1", 10,
        ));
    }

    #[test]
    fn normalize_tags_rejects_a_tag_over_50_chars_after_trimming() {
        let long = "a".repeat(51);
        assert!(normalize_tags(Some(vec![long])).is_err());
        let exactly_50 = "a".repeat(50);
        assert!(normalize_tags(Some(vec![exactly_50])).is_ok());
    }

    #[test]
    fn normalize_tags_does_not_lowercase_or_slug() {
        let got = normalize_tags(Some(vec!["Data Quality".to_string()])).unwrap();
        assert_eq!(got, Some(vec!["Data Quality".to_string()]));
    }

    // ── Unpublish ──────────────────────────────────────────────────────────

    /// A never-published draft query; tests override only the fields they need.
    fn draft_query() -> query::Model {
        let now = Utc::now().into();
        query::Model {
            id: 1,
            org_id: 1,
            data_source_id: 1,
            name: "Test".to_string(),
            description: None,
            query: "SELECT 1".to_string(),
            query_hash: "h".to_string(),
            api_key: "k".to_string(),
            user_id: 1,
            last_modified_by_id: None,
            latest_query_data_id: None,
            is_archived: false,
            visibility: "workspace".to_string(),
            options: serde_json::json!({}),
            tags: None,
            last_execution_status: "New".to_string(),
            last_execution_at: None,
            row_count: None,
            last_execution_error: None,
            created_at: now,
            updated_at: now,
            published_name: None,
            published_description: None,
            published_query: None,
            published_query_hash: None,
            published_data_source_id: None,
            published_options: None,
            published_latest_query_data_id: None,
            published_change_id: None,
            published_at: None,
            working_version: 1,
            unpublished_at: None,
            verified_at: None,
            verified_by: None,
        }
    }

    fn published_query() -> query::Model {
        query::Model {
            published_change_id: Some(99),
            published_latest_query_data_id: Some(555),
            ..draft_query()
        }
    }

    #[test]
    fn is_published_true_only_for_published_and_not_unpublished() {
        assert!(!is_published(&draft_query()));
        assert!(is_published(&published_query()));
        let unpublished = query::Model {
            unpublished_at: Some(Utc::now().into()),
            ..published_query()
        };
        assert!(!is_published(&unpublished));
    }

    #[test]
    fn unpublished_query_still_counts_as_ever_published_for_restore() {
        // An unpublish must not erase publish history.
        let unpublished = query::Model {
            unpublished_at: Some(Utc::now().into()),
            ..published_query()
        };
        assert!(unpublished.published_change_id.is_some());
    }

    #[test]
    fn require_published_rejects_a_never_published_and_an_unpublished_query() {
        assert!(require_published(&draft_query()).is_err());
        let unpublished = query::Model {
            unpublished_at: Some(Utc::now().into()),
            ..published_query()
        };
        assert!(require_published(&unpublished).is_err());
        assert!(require_published(&published_query()).is_ok());
    }

    #[test]
    fn published_result_pointer_is_none_for_a_never_published_or_unpublished_query() {
        assert_eq!(published_result_pointer(&draft_query()), None);
        let unpublished = query::Model {
            unpublished_at: Some(Utc::now().into()),
            ..published_query()
        };
        assert_eq!(published_result_pointer(&unpublished), None);
        assert_eq!(published_result_pointer(&published_query()), Some(555));
    }

    #[test]
    fn resolve_serving_content_hides_the_result_pointer_but_keeps_the_frozen_text_once_unpublished()
    {
        let unpublished = query::Model {
            unpublished_at: Some(Utc::now().into()),
            published_name: Some("Live name".to_string()),
            ..published_query()
        };
        let resolved = resolve_serving_content(&unpublished);
        // Last-published text stays visible (already public); only the result pointer is
        // suppressed.
        assert_eq!(resolved.name, "Live name");
        assert_eq!(resolved.latest_query_data_id, None);
        assert!(!resolved.repoint_published);
    }

    #[test]
    fn resolve_serving_content_never_falls_back_to_the_drafts_pointer_once_unpublished() {
        // An unrelated draft result must not leak once the published one is hidden.
        let unpublished = query::Model {
            unpublished_at: Some(Utc::now().into()),
            latest_query_data_id: Some(777), // the draft's own, unrelated result
            ..published_query()
        };
        let resolved = resolve_serving_content(&unpublished);
        assert_ne!(resolved.latest_query_data_id, Some(777));
        assert_eq!(resolved.latest_query_data_id, None);
    }
}

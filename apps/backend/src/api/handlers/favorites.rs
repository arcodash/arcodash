//! `POST /api/queries/:id/favorite`, `DELETE /api/queries/:id/favorite`,
//! `GET /api/queries/favorites` — star/unstar a query and list all starred queries.

use crate::extract::{Json, Path};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::Utc;
use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use arcodash_core::entities::favorite;

use crate::api::handlers::queries::{query_callers_with_groups, QueryResponse};
use crate::auth::CurrentUser;
use crate::db::Db;
use crate::error::{is_unique_violation, ApiError};
use crate::services::query_service;

/// `POST /api/queries/:id/favorite`
pub async fn star_query(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    // Verify access — 404s on no-access, same as all query endpoints.
    query_service::get_for_user(&db, user.org_id, &user, id).await?;

    // NOTE: ON CONFLICT DO NOTHING via catch-and-ignore (sea-orm has no upsert shortcut
    // for this; a duplicate insert returns a DbErr::Query with unique-violation text).
    let active = favorite::ActiveModel {
        org_id: Set(user.org_id),
        object_type: Set("query".to_string()),
        object_id: Set(id),
        user_id: Set(user.id),
        created_at: Set(Utc::now().into()),
        updated_at: Set(Utc::now().into()),
        ..Default::default()
    };

    // Ignore unique constraint violations — already favorited is fine. Propagate other DB errors.
    // Wrapped in its own `insert_scoped` savepoint so a lost race doesn't poison the enclosing
    // per-request transaction for whatever runs after this handler in the same commit.
    match crate::db::insert_scoped(&db, active).await {
        Err(e) if !is_unique_violation(&e) => Err(e.into()),
        _ => Ok(StatusCode::NO_CONTENT),
    }
}

/// `DELETE /api/queries/:id/favorite`
pub async fn unstar_query(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    favorite::Entity::delete_many()
        .filter(favorite::Column::UserId.eq(user.id))
        .filter(favorite::Column::ObjectType.eq("query"))
        .filter(favorite::Column::ObjectId.eq(id))
        .exec(&db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/queries/favorites` — list queries starred by the current user.
///
/// Resolved by id directly (bounded by how many queries the caller has favorited) rather than
/// loading every query the org has and filtering in Rust.
pub async fn list_favorite_queries(
    db: Db,
    CurrentUser(user): CurrentUser,
) -> Result<impl IntoResponse, ApiError> {
    let favs = favorite::Entity::find()
        .filter(favorite::Column::UserId.eq(user.id))
        .filter(favorite::Column::ObjectType.eq("query"))
        .all(&db)
        .await?;

    // Same visibility gate a direct `GET /api/queries/:id` applies — a query the caller has
    // favorited but can no longer see (grant revoked, data source access removed, ...) is
    // silently dropped rather than erroring the whole list. Archived favorites are dropped too,
    // matching the non-favorites listing.
    let mut queries = Vec::with_capacity(favs.len());
    for f in &favs {
        match query_service::get_for_user(&db, user.org_id, &user, f.object_id).await {
            Ok(q) if q.is_archived => {}
            Ok(q) => queries.push(q),
            Err(e) if e.status == StatusCode::NOT_FOUND => {}
            Err(e) => return Err(e),
        }
    }

    let groups = crate::authz::permissions::load_effective_groups(&db, &user).await?;
    let is_admin = crate::authz::permissions::is_admin(&groups);

    let rows: Vec<(i64, i64)> = queries.iter().map(|q| (q.id, q.data_source_id)).collect();
    let mut callers = query_callers_with_groups(&db, &user, &groups, is_admin, &rows).await?;

    let response: Vec<QueryResponse> = queries
        .into_iter()
        .map(|mut q| {
            let caller = callers.remove(&q.id).unwrap_or_default();
            // Overlay gate matches `GET /api/queries/:id`: a non-editor of a published-pending
            // query is served the published content, never the draft sitting on top of it.
            if !caller.can_edit {
                query_service::overlay_serving_content(&mut q);
            }
            QueryResponse {
                is_favorite: true,
                caller,
                ..QueryResponse::from(q)
            }
        })
        .collect();
    Ok(Json(response))
}

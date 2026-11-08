//! `/api/query_snippets*` — Org-wide shared reference material: reads require `view_snippets`,
//! writes require `manage_snippets` (see `services::snippet_service`'s module docs for the full
//! rationale).

use crate::extract::{Json, Path};
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use arcodash_core::entities::query_snippet;

use crate::auth::CurrentUser;
use crate::db::Db;
use crate::error::ApiError;
use crate::services::snippet_service;

/// Snippet JSON as returned to clients.
#[derive(Debug, Serialize)]
pub struct SnippetResponse {
    pub id: i64,
    pub trigger: String,
    pub description: Option<String>,
    pub snippet: String,
    pub user_id: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<query_snippet::Model> for SnippetResponse {
    fn from(s: query_snippet::Model) -> Self {
        Self {
            id: s.id,
            trigger: s.trigger,
            description: s.description,
            snippet: s.snippet,
            user_id: s.user_id,
            created_at: s.created_at.with_timezone(&Utc),
            updated_at: s.updated_at.with_timezone(&Utc),
        }
    }
}

/// `GET /api/query_snippets` — every snippet in the org. Requires `view_snippets`.
pub async fn list(db: Db, CurrentUser(user): CurrentUser) -> Result<impl IntoResponse, ApiError> {
    let snippets = snippet_service::list_for_org(&db, user.org_id, &user).await?;
    let response: Vec<SnippetResponse> = snippets.into_iter().map(Into::into).collect();
    Ok(Json(response))
}

#[derive(Debug, Deserialize)]
pub struct CreateSnippetRequest {
    pub trigger: String,
    pub description: Option<String>,
    pub snippet: String,
}

/// `POST /api/query_snippets` — create. Requires `manage_snippets`.
pub async fn create(
    db: Db,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<CreateSnippetRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let s = snippet_service::create(
        &db,
        user.org_id,
        &user,
        payload.trigger,
        payload.description,
        payload.snippet,
    )
    .await?;
    Ok(Json(SnippetResponse::from(s)))
}

/// `GET /api/query_snippets/<id>` — org-scoped. Requires `view_snippets`.
pub async fn get(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let s = snippet_service::get_for_org(&db, user.org_id, id, &user).await?;
    Ok(Json(SnippetResponse::from(s)))
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateSnippetRequest {
    pub trigger: Option<String>,
    pub description: Option<String>,
    pub snippet: Option<String>,
}

/// `POST /api/query_snippets/<id>` — update. Requires `manage_snippets`.
pub async fn update(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateSnippetRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = snippet_service::get_for_org(&db, user.org_id, id, &user).await?;
    let updated = snippet_service::update(
        &db,
        &user,
        existing,
        payload.trigger,
        payload.description,
        payload.snippet,
    )
    .await?;
    Ok(Json(SnippetResponse::from(updated)))
}

/// `DELETE /api/query_snippets/<id>` — requires `manage_snippets`.
pub async fn delete(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = snippet_service::get_for_org(&db, user.org_id, id, &user).await?;
    snippet_service::delete(&db, &user, existing).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

//! `/api/visualizations*` — CRUD for query visualizations.

use crate::extract::{Json, Path};
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use arcodash_core::entities::visualization;

use crate::auth::CurrentUser;
use crate::db::Db;
use crate::error::ApiError;
use crate::services::visualization_service;

#[derive(Debug, Serialize)]
pub struct VisualizationResponse {
    pub id: i64,
    pub query_id: i64,
    #[serde(rename = "type")]
    pub viz_type: String,
    pub name: String,
    pub description: Option<String>,
    pub options: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<visualization::Model> for VisualizationResponse {
    fn from(v: visualization::Model) -> Self {
        Self {
            id: v.id,
            query_id: v.query_id,
            viz_type: v.r#type,
            name: v.name,
            description: v.description,
            options: v.options,
            created_at: v.created_at.with_timezone(&Utc),
            updated_at: v.updated_at.with_timezone(&Utc),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateVisualizationRequest {
    pub query_id: i64,
    #[serde(rename = "type")]
    pub viz_type: String,
    pub name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub options: Value,
}

/// `POST /api/visualizations`
pub async fn create(
    db: Db,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<CreateVisualizationRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let viz = visualization_service::create(
        &db,
        user.org_id,
        &user,
        payload.query_id,
        payload.viz_type,
        payload.name,
        payload.description,
        payload.options,
    )
    .await?;
    Ok(Json(VisualizationResponse::from(viz)))
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateVisualizationRequest {
    #[serde(rename = "type")]
    pub viz_type: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub options: Option<Value>,
}

/// `POST /api/visualizations/:id`
pub async fn update(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateVisualizationRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let viz = visualization_service::update(
        &db,
        user.org_id,
        &user,
        id,
        payload.viz_type,
        payload.name,
        payload.description,
        payload.options,
    )
    .await?;
    Ok(Json(VisualizationResponse::from(viz)))
}

/// `DELETE /api/visualizations/:id`
pub async fn delete(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    visualization_service::delete(&db, user.org_id, &user, id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

//! `/api/widgets*` — CRUD for dashboard widgets.

use crate::extract::{Json, Path};
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use arcodash_core::entities::widget;

use crate::auth::CurrentUser;
use crate::db::Db;
use crate::error::ApiError;
use crate::services::widget_service;

#[derive(Debug, Serialize)]
pub struct WidgetResponse {
    pub id: i64,
    pub dashboard_id: i64,
    pub visualization_id: Option<i64>,
    pub text: Option<String>,
    pub width: i32,
    pub options: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<widget::Model> for WidgetResponse {
    fn from(w: widget::Model) -> Self {
        Self {
            id: w.id,
            dashboard_id: w.dashboard_id,
            visualization_id: w.visualization_id,
            text: w.text,
            width: w.width,
            options: w.options,
            created_at: w.created_at.with_timezone(&Utc),
            updated_at: w.updated_at.with_timezone(&Utc),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateWidgetRequest {
    pub dashboard_id: i64,
    pub visualization_id: Option<i64>,
    pub text: Option<String>,
    #[serde(default = "default_width")]
    pub width: i32,
    #[serde(default)]
    pub options: Value,
}

const fn default_width() -> i32 {
    1
}

/// `POST /api/widgets`
pub async fn create(
    db: Db,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<CreateWidgetRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let widget = widget_service::create(
        &db,
        user.org_id,
        &user,
        payload.dashboard_id,
        payload.visualization_id,
        payload.text,
        payload.width,
        payload.options,
    )
    .await?;
    Ok(Json(WidgetResponse::from(widget)))
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateWidgetRequest {
    pub text: Option<String>,
    pub width: Option<i32>,
    pub options: Option<Value>,
}

/// `POST /api/widgets/:id`
pub async fn update(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateWidgetRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let widget = widget_service::update(
        &db,
        user.org_id,
        &user,
        id,
        payload.text,
        payload.width,
        payload.options,
    )
    .await?;
    Ok(Json(WidgetResponse::from(widget)))
}

/// `DELETE /api/widgets/:id`
pub async fn delete(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    widget_service::delete(&db, user.org_id, &user, id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

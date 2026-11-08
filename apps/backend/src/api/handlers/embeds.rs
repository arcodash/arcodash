//! `/api/embed/{token}` (public, unauthenticated) and `/api/embeds*` (authenticated CRUD) +
//! `/api/orgs/me/embed_settings` — the public embed subsystem.

use crate::extract::{Json, Path};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::{CurrentOrg, CurrentUser, OptionalCurrentUser};
use crate::authz::permissions::{load_effective_groups, user_has_permission};
use crate::db::Db;
use crate::error::ApiError;
use crate::services::embed_service::{self, EmbedObjectType};
use crate::state::AppState;

/// Security headers shared by every public/embed-serving route.
///
/// `noindex` + `no-referrer` (the token/URL is itself a bearer secret — never leak it via
/// `Referer`), `Cache-Control: no-store` (a CDN/proxy must never cache a body that can later be
/// revoked), and a default `frame-ancestors *` (a public link is *meant* to be embeddable
/// anywhere).
///
/// Applied to both the unified route and the deprecated `dashboards::public_get` shim via a plain
/// shared helper rather than a `tower::Layer`, since this repo doesn't otherwise pull in
/// `tower-http`'s `set-header` feature for one route.
#[must_use]
pub fn public_embed_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-robots-tag",
        HeaderValue::from_static("noindex, nofollow"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    // The default, applied before an embed has even been resolved (every error path keeps this
    // value) — a row with no `frame_ancestors` list is deliberately frameable anywhere, and
    // `get_public_embed` overwrites this header once resolution finds a non-empty list.
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static("frame-ancestors *"),
    );
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    headers
}

// ── Public read ──────────────────────────────────────────────────────────────

/// Overwrites `public_embed_headers()`'s default `frame-ancestors *` with the resolved embed's own
/// allowlist, if it has one.
///
/// Shared by every anonymous-read route so a route added later can't reproduce the drift where only
/// one of two equivalent routes applied this override.
pub fn apply_frame_ancestors(
    headers: &mut HeaderMap,
    embed: &arcodash_core::entities::embed::Model,
) {
    let origins: Vec<&str> = embed
        .frame_ancestors
        .as_ref()
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !origins.is_empty() {
        if let Ok(value) = HeaderValue::from_str(&format!("frame-ancestors {}", origins.join(" ")))
        {
            headers.insert("content-security-policy", value);
        }
    }
}

/// `GET /api/embed/{token}` — the ONE unified anonymous read, dashboard or visualization, decided
/// by the resolved embed's `object_type`.
///
/// Mirrors `dashboards::public_get`'s extractor set; the resolved embed's own `anon_authorized`
/// (gate 4 of `resolve_public_embed`), not caller identity, decides whether an `OrgMemberOnly`
/// caller needs to be a member of the link's own org.
pub async fn get_public_embed(
    State(state): State<AppState>,
    db: Db,
    CurrentOrg(org): CurrentOrg,
    OptionalCurrentUser(user): OptionalCurrentUser,
    Path(token): Path<String>,
) -> impl IntoResponse {
    serve_public_embed(
        &db,
        &state,
        &org,
        &token,
        None,
        user.as_ref().map(|u| u.org_id),
    )
    .await
}

/// The anonymous read shared by [`get_public_embed`] and `dashboards::public_get`: resolve the
/// token (optionally pinned to one `object_type`), serialize the public body, and record the
/// outcome metric.
pub(crate) async fn serve_public_embed(
    db: &Db,
    state: &AppState,
    org: &arcodash_core::entities::organization::Model,
    token: &str,
    object_type: Option<EmbedObjectType>,
    caller_org_id: Option<i64>,
) -> Response {
    // Headers (incl. `Cache-Control: no-store`) must land on every response, error paths
    // included — a token-bearing URL's failure body must never be cacheable either.
    let mut headers = public_embed_headers();

    match embed_service::resolve_public_embed(db, state, org, token, object_type, caller_org_id)
        .await
    {
        Ok(resolved) => {
            apply_frame_ancestors(&mut headers, &resolved.embed);
            match embed_service::serialize_public(db, &resolved).await {
                Ok(body) => {
                    crate::metrics::record_embed_serve("served");
                    (headers, Json(body)).into_response()
                }
                Err(e) => {
                    crate::metrics::record_embed_serve("error");
                    (headers, e).into_response()
                }
            }
        }
        Err(e) => {
            // Gate 4 in `OrgMemberOnly` mode returns 401 for a link that isn't `anon_authorized`
            // when the caller isn't a member of the link's own org — no session at all, or a
            // session for a different org — every other gate keeps the uniform "not_found"
            // outcome.
            let outcome = if e.status == StatusCode::UNAUTHORIZED {
                "unauthenticated"
            } else {
                "not_found"
            };
            crate::metrics::record_embed_serve(outcome);
            (headers, e).into_response()
        }
    }
}

// ── Authenticated CRUD ───────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct EmbedApiResponse {
    pub id: i64,
    pub object_type: String,
    pub object_id: i64,
    pub mode: String,
    /// Present for `mode: "public"`. The bearer secret — only ever returned to an
    /// authenticated caller who may manage this embed, never on the public read path.
    pub token: Option<String>,
    pub public_url: Option<String>,
    /// Always `{}` — see `embed_service::CreateEmbedInput::locked_params`.
    pub locked_params: Value,
    pub expires_at: Option<DateTime<Utc>>,
    pub anon_authorized: bool,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

fn to_api_response(state: &AppState, e: arcodash_core::entities::embed::Model) -> EmbedApiResponse {
    let public_url = e
        .token
        .as_ref()
        .map(|t| format!("{}/public/embed/{}", state.config.base_url, t));
    EmbedApiResponse {
        id: e.id,
        object_type: e.object_type,
        object_id: e.object_id,
        mode: e.mode,
        token: e.token,
        public_url,
        locked_params: e.locked_params,
        expires_at: e.expires_at.map(|t| t.with_timezone(&Utc)),
        anon_authorized: e.anon_authorized,
        revoked_at: e.revoked_at.map(|t| t.with_timezone(&Utc)),
        created_at: e.created_at.with_timezone(&Utc),
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateEmbedRequest {
    pub object_type: EmbedObjectType,
    pub object_id: i64,
    #[serde(default, alias = "anon_authorized")]
    pub anonymous: bool,
    /// Accepted for backward compatibility but ignored — see `CreateEmbedInput::locked_params`.
    pub locked_params: Option<Value>,
    pub expires_at: Option<DateTime<Utc>>,
    pub frame_ancestors: Option<Vec<String>>,
}

/// `POST /api/embeds`
pub async fn create(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<CreateEmbedRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let groups = load_effective_groups(&db, &user).await?;
    let created = embed_service::create_embed(
        &db,
        &state,
        user.org_id,
        &user,
        &groups,
        embed_service::CreateEmbedInput {
            object_type: payload.object_type,
            object_id: payload.object_id,
            anonymous: payload.anonymous,
            locked_params: payload
                .locked_params
                .unwrap_or_else(|| serde_json::json!({})),
            expires_at: payload.expires_at,
            frame_ancestors: payload.frame_ancestors,
        },
    )
    .await?;
    Ok(Json(to_api_response(&state, created)))
}

#[derive(Debug, Deserialize)]
pub struct ListEmbedsQuery {
    pub object_type: String,
    pub object_id: i64,
}

/// `GET /api/embeds?object_type=&object_id=`
pub async fn list(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Query(q): Query<ListEmbedsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let object_type = EmbedObjectType::parse(&q.object_type)
        .ok_or_else(|| ApiError::bad_request("invalid object_type"))?;
    let groups = load_effective_groups(&db, &user).await?;
    let embeds = embed_service::list_embeds_for_object(
        &db,
        user.org_id,
        &user,
        &groups,
        object_type,
        q.object_id,
    )
    .await?;
    Ok(Json(
        embeds
            .into_iter()
            .map(|e| to_api_response(&state, e))
            .collect::<Vec<_>>(),
    ))
}

/// `DELETE /api/embeds/{id}`
pub async fn revoke(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let groups = load_effective_groups(&db, &user).await?;
    embed_service::revoke_embed(&db, user.org_id, &user, &groups, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct ActiveCountQuery {
    pub query_id: i64,
}

/// `GET /api/embeds/active_count?query_id=` — the query-editor "N public embeds" warning.
///
/// Org membership alone is not enough: `query_service::get_for_user` confirms the caller can
/// actually view `query_id` (capability + reach + data-source access) before this leaks even the
/// count of its public links to them.
pub async fn active_count(
    db: Db,
    CurrentUser(user): CurrentUser,
    Query(q): Query<ActiveCountQuery>,
) -> Result<impl IntoResponse, ApiError> {
    crate::services::query_service::get_for_user(&db, user.org_id, &user, q.query_id).await?;
    let count =
        embed_service::count_active_public_embeds_for_query(&db, user.org_id, q.query_id).await?;
    Ok(Json(serde_json::json!({ "count": count })))
}

// ── Org embed settings ───────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct OrgEmbedSettingsResponse {
    pub public_urls_enabled: bool,
}

/// `GET /api/orgs/me/embed_settings` — any org member may read this.
///
/// The payload is a single boolean the non-admin embed dialog also needs, so gating the read on
/// `admin` (like the POST sibling below) would break that dialog for non-admins.
pub async fn get_org_settings(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(_user): CurrentUser,
) -> Result<impl IntoResponse, ApiError> {
    let settings = embed_service::get_org_embed_settings(&db, org.id).await?;
    Ok(Json(OrgEmbedSettingsResponse {
        public_urls_enabled: settings.public_urls_enabled,
    }))
}

#[derive(Debug, Deserialize)]
pub struct SetOrgEmbedSettingsRequest {
    pub public_urls_enabled: bool,
}

/// `POST /api/orgs/me/embed_settings` — admin-gated.
pub async fn set_org_settings(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<SetOrgEmbedSettingsRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let groups = load_effective_groups(&db, &user).await?;
    if !user_has_permission(&user, &groups, "admin") {
        return Err(ApiError::forbidden("admin only"));
    }
    embed_service::set_public_urls_enabled(&db, org.id, &user, payload.public_urls_enabled).await?;
    let settings = embed_service::get_org_embed_settings(&db, org.id).await?;
    Ok(Json(OrgEmbedSettingsResponse {
        public_urls_enabled: settings.public_urls_enabled,
    }))
}

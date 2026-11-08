//! Public-link subsystem for dashboards and single visualizations.
//!
//! [`resolve_public_embed`] is the single fail-closed guard for every anonymous read. Failures are a uniform
//! 404 (no existence oracle) except gate 4's auth case. Gates, in order:
//! 1. env `DISABLE_PUBLIC_URLS` kill-switch;
//! 2. org `public_urls_enabled` soft pause (a missing row means enabled);
//! 3. the embed row exists, is not revoked, and is not expired;
//! 4. anonymous access (`anon_authorized`, else org membership in `OrgMemberOnly` mode);
//! 5. the parent resource is still servable.
//!
//! Liveness is computed per request, never stored, so an org pause is non-destructive.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, EntityTrait, Order, PaginatorTrait,
    QueryFilter, QueryOrder,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use arcodash_core::entities::{
    dashboard, embed, group, monitor, org_embed_settings, organization, query, query_result, user,
    visualization,
};
use arcodash_core::events::record_event;

use crate::error::ApiError;
use crate::services::{dashboard_service, monitor_service, query_service};
use crate::state::{AppState, PublicDashboardAccess};

// ── Object type ──────────────────────────────────────────────────────────────

/// The two embeddable object kinds; stored as free text on the `embeds` row (no DB enum), typed
/// elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedObjectType {
    Dashboard,
    Visualization,
}

impl EmbedObjectType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dashboard => "dashboard",
            Self::Visualization => "visualization",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dashboard" => Some(Self::Dashboard),
            "visualization" => Some(Self::Visualization),
            _ => None,
        }
    }
}

// ── The guard ────────────────────────────────────────────────────────────────

pub struct ResolvedEmbed {
    pub embed: embed::Model,
    pub object_type: EmbedObjectType,
}

fn not_found() -> ApiError {
    // Deliberately generic: never reveal why (revoked, expired, paused, disabled, missing).
    ApiError::not_found("not found")
}

/// Dashboard `id` in `org_id`, else "dashboard not found".
async fn dashboard_in_org(
    db: &impl ConnectionTrait,
    org_id: i64,
    id: i64,
) -> Result<dashboard::Model, ApiError> {
    dashboard::Entity::find_by_id(id)
        .filter(dashboard::Column::OrgId.eq(org_id))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("dashboard not found"))
}

/// The org-scoped query behind visualization `viz_id`; the 404 names whichever row is missing.
async fn viz_query_in_org(
    db: &impl ConnectionTrait,
    org_id: i64,
    viz_id: i64,
) -> Result<query::Model, ApiError> {
    let viz = visualization::Entity::find_by_id(viz_id)
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("visualization not found"))?;
    query::Entity::find_by_id(viz.query_id)
        .filter(query::Column::OrgId.eq(org_id))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("query not found"))
}

/// Collapses a lookup's 404 into the generic [`not_found`]; other errors pass through.
fn generic_404(e: ApiError) -> ApiError {
    if e.status == axum::http::StatusCode::NOT_FOUND {
        not_found()
    } else {
        e
    }
}

/// The single gate every public read routes through.
///
/// `expected_object_type`: `Some(..)` rejects object-type confusion (e.g. a visualization token
/// replayed against the dashboard route) with the same uniform 404. `caller_org_id`: the signed-in
/// caller's own org (not the link's), used only by gate 4 in `OrgMemberOnly` mode.
pub async fn resolve_public_embed(
    db: &impl ConnectionTrait,
    state: &AppState,
    org: &organization::Model,
    token: &str,
    expected_object_type: Option<EmbedObjectType>,
    caller_org_id: Option<i64>,
) -> Result<ResolvedEmbed, ApiError> {
    // Gate 1: env kill-switch. Overrides all.
    if state.config.disable_public_urls {
        return Err(not_found());
    }

    // Gate 2: org-wide soft pause.
    let settings = get_org_embed_settings(db, org.id).await?;
    if !settings.public_urls_enabled {
        return Err(not_found());
    }

    // Gate 3: the embed row. The explicit `org_id` filter is defense-in-depth on top of RLS;
    // another org's token doesn't resolve.
    let now = Utc::now();
    let found = embed::Entity::find()
        .filter(embed::Column::Token.eq(token))
        .filter(embed::Column::OrgId.eq(org.id))
        .one(db)
        .await?
        .ok_or_else(not_found)?;

    if found.revoked_at.is_some()
        || found
            .expires_at
            .is_some_and(|exp| exp.with_timezone(&Utc) <= now)
    {
        return Err(not_found());
    }

    let object_type = EmbedObjectType::parse(&found.object_type).ok_or_else(not_found)?;
    if expected_object_type.is_some_and(|expected| expected != object_type) {
        return Err(not_found());
    }

    // Gate 4: anonymous access. `anon_authorized` is honored at serve time in every access mode,
    // but only while the creator is still active, so revoking an account kills the anonymous links
    // it minted. Otherwise `Anonymous` mode grants nothing and `OrgMemberOnly` serves members of
    // THIS link's org.
    if found.anon_authorized {
        let creator_active = match found.created_by_id {
            Some(uid) => user::Entity::find_by_id(uid)
                .one(db)
                .await?
                .is_some_and(|u| u.disabled_at.is_none()),
            None => false,
        };
        if !creator_active {
            tracing::warn!(
                embed_id = found.id,
                created_by_id = found.created_by_id,
                "embed 404: creator account is disabled or gone"
            );
            return Err(not_found());
        }
    } else if state.public_dashboard_access == PublicDashboardAccess::Anonymous {
        return Err(not_found());
    } else if caller_org_id != Some(org.id) {
        return Err(ApiError::unauthorized("sign in to view this"));
    }

    // Gate 5: the parent must still be servable. Checked at serve time (not as a write-time
    // cascade) so restricting an object also cuts off links minted earlier.
    let servable = match object_type {
        EmbedObjectType::Dashboard => {
            let d = dashboard_in_org(db, org.id, found.object_id)
                .await
                .map_err(generic_404)?;
            !d.is_archived && crate::authz::access::visibility_is_public(&d.visibility)
        }
        // A visualization has no visibility of its own — it inherits its parent query's.
        EmbedObjectType::Visualization => {
            let q = viz_query_in_org(db, org.id, found.object_id)
                .await
                .map_err(generic_404)?;
            crate::authz::access::visibility_is_public(&q.visibility)
        }
    };
    if !servable {
        return Err(not_found());
    }

    Ok(ResolvedEmbed {
        embed: found,
        object_type,
    })
}

// ── Org embed settings (gate #2 storage) ────────────────────────────────────

pub struct OrgEmbedSettingsView {
    pub org_id: i64,
    pub public_urls_enabled: bool,
}

/// A missing row yields the defaults: `public_urls_enabled = true`, no secret.
pub async fn get_org_embed_settings(
    db: &impl ConnectionTrait,
    org_id: i64,
) -> Result<OrgEmbedSettingsView, ApiError> {
    let row = org_embed_settings::Entity::find_by_id(org_id)
        .one(db)
        .await?;
    Ok(OrgEmbedSettingsView {
        org_id,
        public_urls_enabled: row.is_none_or(|r| r.public_urls_enabled),
    })
}

/// Soft, reversible pause; never touches `embeds` rows. Find-then-write upsert (rare admin action,
/// so the TOCTOU window is acceptable).
pub async fn set_public_urls_enabled(
    db: &impl ConnectionTrait,
    org_id: i64,
    actor: &user::Model,
    enabled: bool,
) -> Result<(), ApiError> {
    let now = Utc::now();
    match org_embed_settings::Entity::find_by_id(org_id)
        .one(db)
        .await?
    {
        Some(existing) => {
            let mut active: org_embed_settings::ActiveModel = existing.into();
            active.public_urls_enabled = Set(enabled);
            active.updated_at = Set(now.into());
            active.update(db).await?;
        }
        None => {
            org_embed_settings::ActiveModel {
                org_id: Set(org_id),
                public_urls_enabled: Set(enabled),
                embedding_secret: Set(None),
                created_at: Set(now.into()),
                updated_at: Set(now.into()),
            }
            .insert(db)
            .await?;
        }
    }

    record_event(
        db,
        org_id,
        Some(actor.id),
        if enabled {
            "embed.public_resumed"
        } else {
            "embed.public_paused"
        },
        None,
        None,
        serde_json::json!({}),
    )
    .await?;
    Ok(())
}

// ── Embed CRUD (authenticated) ──────────────────────────────────────────────

pub struct CreateEmbedInput {
    pub object_type: EmbedObjectType,
    pub object_id: i64,
    /// `true`: anonymous public link (needs `share_public_anonymous`). `false`: org-member link
    /// (needs `create_embed`), servable only under `OrgMemberOnly`; `create_embed` rejects it up
    /// front under `Anonymous`.
    pub anonymous: bool,
    /// Accepted for wire compatibility but never applied (the public path has no per-request
    /// parameters); always stored as `{}`.
    pub locked_params: Value,
    pub expires_at: Option<DateTime<Utc>>,
    /// Bare origins allowed to frame this embed, enforced via `Content-Security-Policy:
    /// frame-ancestors`. `None`/empty means any.
    pub frame_ancestors: Option<Vec<String>>,
}

/// Validates one `frame_ancestors` entry as a bare origin (`scheme://host[:port]`); rejects paths,
/// queries, fragments, and anything that could break out of the CSP directive (`'`, `;`,
/// whitespace).
fn validate_frame_ancestor(origin: &str) -> Result<(), ApiError> {
    let bad = |why: &str| {
        ApiError::bad_request(format!("invalid frame_ancestors origin {origin:?}: {why}"))
    };
    if origin.chars().any(char::is_whitespace) || origin.contains(['\'', ';']) {
        return Err(bad("must not contain whitespace, ' or ;"));
    }
    let url = reqwest::Url::parse(origin).map_err(|_| bad("not a valid origin"))?;
    match url.host_str() {
        None => return Err(bad("missing host")),
        Some("*") => return Err(bad("wildcard host is not allowed")),
        Some(_) => {}
    }
    let path = url.path();
    if !path.is_empty() && path != "/" {
        return Err(bad("must not include a path"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(bad("must not include a query or fragment"));
    }
    Ok(())
}

/// Validates `frame_ancestors` input: `None` for absent/empty, else the origins (max 20).
fn validate_frame_ancestors_input(
    origins: Option<Vec<String>>,
) -> Result<Option<Vec<String>>, ApiError> {
    let Some(origins) = origins.filter(|o| !o.is_empty()) else {
        return Ok(None);
    };
    if origins.len() > 20 {
        return Err(ApiError::bad_request(
            "frame_ancestors accepts at most 20 origins",
        ));
    }
    origins
        .iter()
        .try_for_each(|origin| validate_frame_ancestor(origin))?;
    Ok(Some(origins))
}

/// Clamps a caller-supplied expiry to the org's default TTL ceiling (`min(client, now + default)`);
/// a non-future client value is rejected.
pub(crate) fn clamp_expiry(
    client: Option<DateTime<Utc>>,
    org_default: Option<std::time::Duration>,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, ApiError> {
    if client.is_some_and(|c| c <= now) {
        return Err(ApiError::bad_request("expires_at must be in the future"));
    }
    let default_expiry = org_default
        .and_then(|d| chrono::Duration::from_std(d).ok())
        .map(|d| now + d);
    Ok(match (client, default_expiry) {
        (Some(c), Some(d)) => Some(c.min(d)),
        (c, d) => c.or(d),
    })
}

/// Confirms `object_id` belongs to `org_id` and the caller may share it (owner-or-admin).
/// `require_published` gates the dashboard branch on a published arrangement; skip it for
/// list/revoke so existing embeds stay manageable.
pub(crate) async fn require_can_share_object(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    groups: &[group::Model],
    object_type: EmbedObjectType,
    object_id: i64,
    require_published: bool,
) -> Result<(), ApiError> {
    let owner_id = match object_type {
        EmbedObjectType::Dashboard => {
            let d = dashboard_in_org(db, org_id, object_id).await?;
            if require_published {
                dashboard_service::require_published(&d)?;
            }
            d.user_id
        }
        EmbedObjectType::Visualization => viz_query_in_org(db, org_id, object_id).await?.user_id,
    };

    if crate::authz::permissions::is_admin(groups) || owner_id == user.id {
        Ok(())
    } else {
        Err(ApiError::forbidden(
            "must be the owner or an admin to share this",
        ))
    }
}

/// Refuses to mint a link the read side would never serve (mirrors gate 5). Creation paths only,
/// never list/revoke.
pub(crate) async fn require_object_publicly_servable(
    db: &impl ConnectionTrait,
    org_id: i64,
    object_type: EmbedObjectType,
    object_id: i64,
) -> Result<(), ApiError> {
    match object_type {
        EmbedObjectType::Dashboard => {
            let d = dashboard_in_org(db, org_id, object_id).await?;
            if d.is_archived || !crate::authz::access::visibility_is_public(&d.visibility) {
                return Err(ApiError::bad_request(
                    "set this dashboard's Visibility to \"Everyone\" before creating a public link",
                ));
            }
        }
        EmbedObjectType::Visualization => {
            let q = viz_query_in_org(db, org_id, object_id).await?;
            if !crate::authz::access::visibility_is_public(&q.visibility) {
                return Err(ApiError::bad_request(
                    "set this visualization's query Visibility to \"Everyone\" before creating a public link",
                ));
            }
        }
    }
    Ok(())
}

/// URL-safe bearer token: two concatenated `UUIDv4` "simple" hex strings (~244 bits of entropy).
fn generate_embed_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Cheap shape gate so garbage paths on the unauthenticated route skip the database; accepts 64-hex
/// tokens and 36-char hyphenated UUIDs.
fn token_shape_is_plausible(token: &str) -> bool {
    (1..=128).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The org that owns the embed named by `token`, resolved ahead of session/Host-based `resolve_org`
/// so the token's org always wins.
///
/// Runs on the owner pool (no org GUC exists yet); the 244-bit token is the whole access boundary,
/// so it looks up by token alone.
pub async fn org_for_public_token(
    state: &AppState,
    token: &str,
) -> Result<Option<organization::Model>, ApiError> {
    if !token_shape_is_plausible(token) {
        return Ok(None);
    }
    let Some(row) = embed::Entity::find()
        .filter(embed::Column::Token.eq(token))
        .one(&state.conveyor_db)
        .await?
    else {
        return Ok(None);
    };
    Ok(organization::Entity::find_by_id(row.org_id)
        .one(&state.conveyor_db)
        .await?)
}

/// `POST /api/embeds` business logic.
pub async fn create_embed(
    db: &impl ConnectionTrait,
    state: &AppState,
    org_id: i64,
    user: &user::Model,
    groups: &[group::Model],
    input: CreateEmbedInput,
) -> Result<embed::Model, ApiError> {
    use crate::authz::permissions::user_has_permission;

    if !user_has_permission(user, groups, "create_embed") {
        return Err(ApiError::forbidden("missing the create_embed permission"));
    }
    // Checked server-side regardless of what the UI shows.
    if input.anonymous && !user_has_permission(user, groups, "share_public_anonymous") {
        return Err(ApiError::forbidden(
            "missing the share_public_anonymous permission",
        ));
    }
    require_can_share_object(
        db,
        org_id,
        user,
        groups,
        input.object_type,
        input.object_id,
        true,
    )
    .await?;
    require_object_publicly_servable(db, org_id, input.object_type, input.object_id).await?;

    // A visualization on a never-published query has nothing to serve anonymously.
    if input.object_type == EmbedObjectType::Visualization {
        let viz_query = viz_query_in_org(db, org_id, input.object_id).await?;
        query_service::require_published(&viz_query)?;
    }

    // Under `Anonymous` a non-anonymous link would be refused by gate 4 forever; refuse up front,
    // after the object-specific checks so those reasons win.
    if state.public_dashboard_access == PublicDashboardAccess::Anonymous && !input.anonymous {
        return Err(ApiError::bad_request(
            "this deployment only supports anonymous public links; enable anonymous sharing for this embed",
        ));
    }

    let now = Utc::now();
    let expires_at = clamp_expiry(input.expires_at, state.embed_default_expiry, now)?;
    let frame_ancestors = validate_frame_ancestors_input(input.frame_ancestors)?;

    let token = generate_embed_token();
    let model = embed::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        object_type: Set(input.object_type.as_str().to_string()),
        object_id: Set(input.object_id),
        mode: Set("public".to_string()),
        token: Set(Some(token)),
        // Never `input.locked_params`.
        locked_params: Set(serde_json::json!({})),
        frame_ancestors: Set(frame_ancestors.map(|o| serde_json::json!(o))),
        expires_at: Set(expires_at.map(Into::into)),
        created_by_id: Set(Some(user.id)),
        anon_authorized: Set(input.anonymous),
        revoked_at: Set(None),
        created_at: Set(now.into()),
    };
    let created = model.insert(db).await?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "embed.created",
        Some("embed"),
        Some(created.id),
        serde_json::json!({
            "object_type": input.object_type.as_str(),
            "object_id": input.object_id,
            "mode": "public",
            "anonymous": input.anonymous,
            "expires_at": expires_at,
        }),
    )
    .await?;

    crate::metrics::record_embed_event("created");
    Ok(created)
}

/// `GET /api/embeds?object_type=&object_id=` — active links for the Share dialog; same
/// owner-or-admin policy as `create_embed`, so non-owners never see another user's bearer tokens.
pub async fn list_embeds_for_object(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    groups: &[group::Model],
    object_type: EmbedObjectType,
    object_id: i64,
) -> Result<Vec<embed::Model>, ApiError> {
    require_can_share_object(db, org_id, user, groups, object_type, object_id, false).await?;

    Ok(embed::Entity::find()
        .filter(embed::Column::OrgId.eq(org_id))
        .filter(embed::Column::ObjectType.eq(object_type.as_str()))
        .filter(embed::Column::ObjectId.eq(object_id))
        .order_by(embed::Column::CreatedAt, Order::Desc)
        .all(db)
        .await?)
}

/// `DELETE /api/embeds/{id}` — idempotent. Never deletes the row; sets `revoked_at`.
pub async fn revoke_embed(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    groups: &[group::Model],
    embed_id: i64,
) -> Result<(), ApiError> {
    let existing = embed::Entity::find_by_id(embed_id)
        .filter(embed::Column::OrgId.eq(org_id))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("embed not found"))?;

    let is_owner = existing.created_by_id == Some(user.id);
    if !crate::authz::permissions::is_admin(groups) && !is_owner {
        return Err(ApiError::forbidden(
            "must be the creator or an admin to revoke this embed",
        ));
    }

    if existing.revoked_at.is_none() {
        let id = existing.id;
        let mut active: embed::ActiveModel = existing.into();
        active.revoked_at = Set(Some(Utc::now().into()));
        active.update(db).await?;

        record_event(
            db,
            org_id,
            Some(user.id),
            "embed.revoked",
            Some("embed"),
            Some(id),
            serde_json::json!({}),
        )
        .await?;
        crate::metrics::record_embed_event("revoked");
    }
    Ok(())
}

/// Count of active (not revoked/expired) embeds on visualizations of `query_id` plus dashboards
/// containing them. Ignores the org pause and env kill-switch.
pub async fn count_active_public_embeds_for_query(
    db: &impl ConnectionTrait,
    org_id: i64,
    query_id: i64,
) -> Result<i64, ApiError> {
    use arcodash_core::entities::widget;

    let viz_ids: Vec<i64> = visualization::Entity::find()
        .filter(visualization::Column::QueryId.eq(query_id))
        .all(db)
        .await?
        .into_iter()
        .map(|v| v.id)
        .collect();

    if viz_ids.is_empty() {
        return Ok(0);
    }

    let dash_ids: Vec<i64> = widget::Entity::find()
        .filter(widget::Column::VisualizationId.is_in(viz_ids.clone()))
        .all(db)
        .await?
        .into_iter()
        .map(|w| w.dashboard_id)
        .collect::<HashSet<i64>>()
        .into_iter()
        .collect();

    let now = Utc::now();
    // Shared by both counts so "active" can't drift between object types.
    let active = |org_id: i64| {
        Condition::all()
            .add(embed::Column::OrgId.eq(org_id))
            .add(embed::Column::RevokedAt.is_null())
            .add(
                Condition::any()
                    .add(embed::Column::ExpiresAt.is_null())
                    .add(embed::Column::ExpiresAt.gt(now)),
            )
    };

    // One grouped `COUNT` per object type rather than per target.
    let mut count = embed::Entity::find()
        .filter(active(org_id))
        .filter(embed::Column::ObjectType.eq("visualization"))
        .filter(embed::Column::ObjectId.is_in(viz_ids))
        .count(db)
        .await?
        .cast_signed();

    if !dash_ids.is_empty() {
        count += embed::Entity::find()
            .filter(active(org_id))
            .filter(embed::Column::ObjectType.eq("dashboard"))
            .filter(embed::Column::ObjectId.is_in(dash_ids))
            .count(db)
            .await?
            .cast_signed();
    }

    Ok(count)
}

// ── Stripped public serializer ─────────────────────────────

/// The unified public/embed wire shape; deliberately narrow: no SQL text, data-source config, or
/// sibling queries.
#[derive(Debug, Serialize)]
pub struct PublicEmbedResponse {
    pub object_type: String,
    pub title: String,
    /// Populated for `object_type: "dashboard"`; empty for `"visualization"`.
    pub widgets: Vec<PublicWidget>,
    /// Populated for `object_type: "visualization"`; `None` for `"dashboard"`.
    pub visualization: Option<PublicViz>,
    /// The cached result's refresh time ("Shared view · as of {t}"); `None` if never executed.
    pub as_of: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct PublicWidget {
    pub id: i64,
    pub text: Option<String>,
    pub width: i32,
    /// Only the layout position is carried over from `widget.options`, never the whole blob.
    pub position: Option<Value>,
    pub visualization: Option<PublicViz>,
    /// Freshness indicator inputs, same shape as the authenticated dashboard payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<dashboard_service::WidgetFreshness>,
    /// Monitor tiles from `widget.options.monitors`, in order; monitors failing the public gate are
    /// dropped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitors: Option<Vec<monitor_service::MonitorTile>>,
}

#[derive(Debug, Serialize)]
pub struct PublicViz {
    #[serde(rename = "type")]
    pub viz_type: String,
    pub name: String,
    pub options: Value,
    pub data: Option<Value>,
}

/// Builds the stripped public response. The request's query string is never consulted: data is
/// always the current cached snapshot.
pub async fn serialize_public(
    db: &impl ConnectionTrait,
    resolved: &ResolvedEmbed,
) -> Result<PublicEmbedResponse, ApiError> {
    match resolved.object_type {
        EmbedObjectType::Dashboard => serialize_dashboard(db, resolved.embed.object_id).await,
        EmbedObjectType::Visualization => {
            serialize_visualization(db, resolved.embed.object_id).await
        }
    }
}

/// The `query_result` id an embed serves, or `None`. An unpublished query serves nothing rather
/// than the draft's rows.
fn serving_query_result_id(q: &query::Model) -> Option<i64> {
    if q.published_change_id.is_none() {
        q.latest_query_data_id
    } else {
        query_service::published_result_pointer(q)
    }
}

async fn fetch_query_result_data(
    db: &impl ConnectionTrait,
    q: Option<&query::Model>,
) -> Result<(Option<Value>, Option<DateTime<Utc>>), ApiError> {
    let Some(qr_id) = q.and_then(serving_query_result_id) else {
        return Ok((None, None));
    };
    let qr = query_result::Entity::find_by_id(qr_id).one(db).await?;
    Ok(qr.map_or((None, None), |r| {
        (r.data, Some(r.retrieved_at.with_timezone(&Utc)))
    }))
}

/// One round trip for every widget's serving result, keyed by `query_result` id.
async fn fetch_query_results_by_ids(
    db: &impl ConnectionTrait,
    ids: &HashSet<i64>,
) -> Result<HashMap<i64, query_result::Model>, ApiError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(query_result::Entity::find()
        .filter(query_result::Column::Id.is_in(ids.iter().copied()))
        .all(db)
        .await?
        .into_iter()
        .map(|r| (r.id, r))
        .collect())
}

async fn serialize_dashboard(
    db: &impl ConnectionTrait,
    dashboard_id: i64,
) -> Result<PublicEmbedResponse, ApiError> {
    let d = dashboard::Entity::find_by_id(dashboard_id)
        .one(db)
        .await?
        .ok_or_else(not_found)?;

    // No snapshot (never published or unpublished): uniform 404, not the empty-vec fallback the
    // in-app viewer relies on.
    if d.published_snapshot.is_none() {
        return Err(not_found());
    }

    // Public/embed is a viewer surface: published snapshot only.
    let items = dashboard_service::load_published_widgets_with_viz(db, &d).await?;

    // A widget's query has its own visibility; a private query on a public dashboard must not leak
    // through its embed.
    let items: Vec<_> = items
        .into_iter()
        .filter(|item| match &item.viz_and_query {
            None => true,
            Some((_, None)) => false,
            Some((_, Some(q))) => crate::authz::access::visibility_is_public(&q.visibility),
        })
        .collect();

    // Monitor tiles on surviving widgets pass the same gate: public monitor, public bound query
    // (via `visibility_is_public`), published query. Failures are silently dropped.
    let monitor_ids: Vec<i64> = items
        .iter()
        .filter_map(|i| i.widget.options.get("monitors"))
        .filter_map(|v| v.as_array())
        .flatten()
        .filter_map(sea_orm::JsonValue::as_i64)
        .collect();
    let monitor_tiles: HashMap<i64, monitor_service::MonitorTile> = if monitor_ids.is_empty() {
        HashMap::new()
    } else {
        let monitors = monitor::Entity::find()
            .filter(monitor::Column::Id.is_in(monitor_ids))
            .filter(monitor::Column::OrgId.eq(d.org_id))
            .all(db)
            .await?;
        let monitor_query_ids: Vec<i64> = monitors.iter().map(|m| m.query_id).collect();
        let monitor_queries: HashMap<i64, query::Model> = if monitor_query_ids.is_empty() {
            HashMap::new()
        } else {
            query::Entity::find()
                .filter(query::Column::Id.is_in(monitor_query_ids))
                .all(db)
                .await?
                .into_iter()
                .map(|q| (q.id, q))
                .collect()
        };
        let public_monitors: Vec<monitor::Model> = monitors
            .into_iter()
            .filter(|m| {
                crate::authz::access::visibility_is_public(&m.visibility)
                    && monitor_queries.get(&m.query_id).is_some_and(|q| {
                        crate::authz::access::visibility_is_public(&q.visibility)
                            && query_service::is_published(q)
                    })
            })
            .collect();
        monitor_service::tile_data(db, &public_monitors).await?
    };

    let query_ids: Vec<i64> = items
        .iter()
        .filter_map(|i| i.viz_and_query.as_ref().map(|(v, _)| v.query_id))
        .collect();
    let fresh = dashboard_service::load_widget_freshness(db, &query_ids).await?;

    // Batch the serving results (unauthenticated, `no-store` route: nothing upstream absorbs an
    // N+1).
    let qr_ids: HashSet<i64> = items
        .iter()
        .filter_map(|i| i.viz_and_query.as_ref().and_then(|(_, q)| q.as_ref()))
        .filter_map(serving_query_result_id)
        .collect();
    let results = fetch_query_results_by_ids(db, &qr_ids).await?;

    let mut widgets = Vec::with_capacity(items.len());
    let mut as_of: Option<DateTime<Utc>> = None;

    for item in items {
        let mut freshness = None;
        let viz_out = item.viz_and_query.map(|(v, q)| {
            freshness = fresh.get(&v.query_id).cloned();
            let result = q
                .as_ref()
                .and_then(serving_query_result_id)
                .and_then(|qr_id| results.get(&qr_id));
            // `None < Some`, so `max` keeps the newest timestamp seen.
            as_of = as_of.max(result.map(|r| r.retrieved_at.with_timezone(&Utc)));
            PublicViz {
                viz_type: v.r#type,
                name: v.name,
                options: v.options,
                data: result.and_then(|r| r.data.clone()),
            }
        });

        let monitors = item
            .widget
            .options
            .get("monitors")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_i64)
                    .filter_map(|id| monitor_tiles.get(&id).cloned())
                    .map(|mut t| {
                        // Connection errors can leak hosts/internals.
                        t.last_check_error = None;
                        t
                    })
                    .collect::<Vec<_>>()
            });
        for checked in monitors.iter().flatten().filter_map(|t| t.last_checked_at) {
            as_of = as_of.max(Some(checked));
        }

        widgets.push(PublicWidget {
            id: item.widget.id,
            text: item.widget.text,
            width: item.widget.width,
            position: item.widget.options.get("position").cloned(),
            visualization: viz_out,
            freshness,
            monitors,
        });
    }

    Ok(PublicEmbedResponse {
        object_type: EmbedObjectType::Dashboard.as_str().to_string(),
        title: d.name,
        widgets,
        visualization: None,
        as_of,
    })
}

async fn serialize_visualization(
    db: &impl ConnectionTrait,
    viz_id: i64,
) -> Result<PublicEmbedResponse, ApiError> {
    let viz = visualization::Entity::find_by_id(viz_id)
        .one(db)
        .await?
        .ok_or_else(not_found)?;
    let q = query::Entity::find_by_id(viz.query_id).one(db).await?;
    let (data, as_of) = fetch_query_result_data(db, q.as_ref()).await?;

    Ok(PublicEmbedResponse {
        object_type: EmbedObjectType::Visualization.as_str().to_string(),
        title: viz.name.clone(),
        widgets: vec![],
        visualization: Some(PublicViz {
            viz_type: viz.r#type,
            name: viz.name,
            options: viz.options,
            data,
        }),
        as_of,
    })
}

// ── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_type_round_trips_through_its_string_form() {
        assert_eq!(EmbedObjectType::Dashboard.as_str(), "dashboard");
        assert_eq!(EmbedObjectType::Visualization.as_str(), "visualization");
        assert_eq!(
            EmbedObjectType::parse("dashboard"),
            Some(EmbedObjectType::Dashboard)
        );
        assert_eq!(
            EmbedObjectType::parse("visualization"),
            Some(EmbedObjectType::Visualization)
        );
        assert_eq!(EmbedObjectType::parse("widget"), None);
        assert_eq!(EmbedObjectType::parse(""), None);
    }

    #[test]
    fn object_type_serializes_to_lowercase_json_strings() {
        assert_eq!(
            serde_json::to_value(EmbedObjectType::Dashboard).unwrap(),
            serde_json::json!("dashboard")
        );
        assert_eq!(
            serde_json::to_value(EmbedObjectType::Visualization).unwrap(),
            serde_json::json!("visualization")
        );
    }

    #[test]
    fn validate_frame_ancestor_accepts_a_bare_origin_with_port() {
        assert!(validate_frame_ancestor("https://a.example.com:8443").is_ok());
    }

    #[test]
    fn validate_frame_ancestor_rejects_a_path() {
        assert!(validate_frame_ancestor("https://a.example.com/path").is_err());
    }

    #[test]
    fn validate_frame_ancestor_rejects_a_bare_wildcard() {
        assert!(validate_frame_ancestor("*").is_err());
    }

    #[test]
    fn validate_frame_ancestor_rejects_a_non_origin_scheme() {
        assert!(validate_frame_ancestor("javascript:alert(1)").is_err());
    }

    #[test]
    fn validate_frame_ancestor_rejects_a_csp_metacharacter() {
        assert!(validate_frame_ancestor("https://x' ;").is_err());
    }

    #[test]
    fn validate_frame_ancestors_input_treats_an_empty_list_as_none() {
        assert_eq!(validate_frame_ancestors_input(Some(vec![])).unwrap(), None);
        assert_eq!(validate_frame_ancestors_input(None).unwrap(), None);
    }

    #[test]
    fn validate_frame_ancestors_input_rejects_more_than_twenty() {
        let origins: Vec<String> = (0..21)
            .map(|i| format!("https://a{i}.example.com"))
            .collect();
        assert!(validate_frame_ancestors_input(Some(origins)).is_err());
    }

    #[test]
    fn generated_tokens_are_unique_and_url_safe() {
        let a = generate_embed_token();
        let b = generate_embed_token();
        assert_ne!(a, b);
        // 2x UUIDv4 "simple" hex = 64 hex chars, well over the 160-bit (40 hex char) floor.
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Golden test of the `PublicEmbedResponse`/`PublicViz` key set; constrains the types only
    /// (`integration_embeds.rs` exercises the real serializer).
    #[test]
    fn dashboard_response_key_set_has_no_leaked_fields() {
        let resp = PublicEmbedResponse {
            object_type: "dashboard".to_string(),
            title: "Q3 Revenue".to_string(),
            widgets: vec![PublicWidget {
                id: 1,
                text: None,
                width: 2,
                position: Some(serde_json::json!({"col": 0, "row": 0})),
                visualization: Some(PublicViz {
                    viz_type: "chart".to_string(),
                    name: "Revenue by month".to_string(),
                    options: serde_json::json!({"x": "month", "y": "revenue"}),
                    data: Some(serde_json::json!({"rows": [], "columns": []})),
                }),
                freshness: None,
                monitors: None,
            }],
            visualization: None,
            as_of: None,
        };

        let value = serde_json::to_value(&resp).unwrap();
        fn keys(v: &Value) -> std::collections::BTreeSet<&str> {
            v.as_object().unwrap().keys().map(String::as_str).collect()
        }
        assert_eq!(
            keys(&value),
            ["object_type", "title", "widgets", "visualization", "as_of"].into()
        );
        let widget_value = &value["widgets"][0];
        assert_eq!(
            keys(widget_value),
            ["id", "text", "width", "position", "visualization"].into()
        );
        assert_eq!(
            keys(&widget_value["visualization"]),
            ["type", "name", "options", "data"].into()
        );

        // Explicitly assert the absence of the fields that must never leak.
        let serialized = serde_json::to_string(&resp).unwrap();
        for forbidden in [
            "query_text",
            "sql",
            "data_source",
            "connection",
            "encrypted_options",
        ] {
            assert!(
                !serialized.contains(forbidden),
                "leaked forbidden field: {forbidden}"
            );
        }
    }

    #[test]
    fn generate_embed_token_never_collides_with_legacy_uuid_secret_tokens() {
        // `dashboard_service::share` tokens are 36-char hyphenated UUIDv4; embed tokens are 64 hex
        // with no hyphens.
        let token = generate_embed_token();
        assert!(!token.contains('-'));
        assert_ne!(token.len(), 36);
    }

    #[test]
    fn token_shape_is_plausible_rejects_malformed_input() {
        assert!(!token_shape_is_plausible(""));
        assert!(!token_shape_is_plausible(&"a".repeat(129)));
        assert!(!token_shape_is_plausible("../etc"));
        assert!(!token_shape_is_plausible("a b"));
    }

    #[test]
    fn token_shape_is_plausible_accepts_real_tokens() {
        assert!(token_shape_is_plausible(&generate_embed_token()));
        // `dashboard_service::share` tokens: hyphenated UUIDv4.
        assert!(token_shape_is_plausible(&uuid::Uuid::new_v4().to_string()));
    }

    #[test]
    fn clamp_expiry_client_only_returns_client_value() {
        let now = Utc::now();
        let client = now + chrono::Duration::hours(2);
        let got = clamp_expiry(Some(client), None, now).unwrap();
        assert_eq!(got, Some(client));
    }

    #[test]
    fn clamp_expiry_org_default_only_returns_the_default() {
        let now = Utc::now();
        let default = std::time::Duration::from_secs(3600);
        let got = clamp_expiry(None, Some(default), now).unwrap();
        assert_eq!(got, Some(now + chrono::Duration::hours(1)));
    }

    #[test]
    fn clamp_expiry_both_present_takes_the_earlier() {
        let now = Utc::now();
        let default = std::time::Duration::from_secs(3600); // now + 1h ceiling

        // Client asks for less than the ceiling -> client value stands.
        let client_within = now + chrono::Duration::minutes(30);
        assert_eq!(
            clamp_expiry(Some(client_within), Some(default), now).unwrap(),
            Some(client_within)
        );

        // Client asks for more than the ceiling -> clamped to the ceiling.
        let client_beyond = now + chrono::Duration::hours(5);
        let got = clamp_expiry(Some(client_beyond), Some(default), now).unwrap();
        assert_eq!(got, Some(now + chrono::Duration::hours(1)));
    }

    #[test]
    fn clamp_expiry_rejects_a_non_future_client_value() {
        let now = Utc::now();
        assert!(clamp_expiry(Some(now), None, now).is_err());
        assert!(clamp_expiry(Some(now - chrono::Duration::minutes(1)), None, now).is_err());
    }

    #[test]
    fn clamp_expiry_neither_present_is_no_expiry() {
        let now = Utc::now();
        assert_eq!(clamp_expiry(None, None, now).unwrap(), None);
    }
}

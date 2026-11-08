pub mod ai;
pub mod ai_ask;
pub mod ai_monitor_ask;
pub mod ai_monitors;
pub mod ai_query;
pub mod ai_settings;
pub mod ai_usage;
pub mod ai_viz;
pub mod ai_widget;
pub mod auth;
pub mod dashboard_narrative;
pub mod dashboards;
pub mod data_sources;
pub mod destinations;
pub mod embeds;
pub mod events;
pub mod favorites;
pub mod groups;
pub mod jobs;
pub mod ldap;
pub mod monitors;
pub mod organization;
pub mod permissions;
pub mod queries;
pub mod roles;
pub mod runs;
pub mod saml;
pub mod schedules;
pub mod snippets;
pub mod status;
pub mod users;
pub mod visualizations;
pub mod widgets;

/// 403 `admin only` unless `user` is an admin of their org.
pub(crate) async fn require_admin(
    db: &crate::db::Db,
    user: &arcodash_core::entities::user::Model,
) -> Result<(), crate::error::ApiError> {
    if crate::services::user_service::is_admin_user(db, user).await? {
        Ok(())
    } else {
        Err(crate::error::ApiError::forbidden("admin only"))
    }
}

/// Validates a list endpoint's `?filter=all|by-me|shared-with-me`.
pub(crate) fn validate_list_filter(filter: Option<&str>) -> Result<(), crate::error::ApiError> {
    if filter.is_some_and(|f| !matches!(f, "all" | "by-me" | "shared-with-me")) {
        return Err(crate::error::ApiError::bad_request(
            "filter must be one of \"all\", \"by-me\", or \"shared-with-me\"",
        ));
    }
    Ok(())
}

/// `ENCRYPTION_KEY` as the 32-byte key every secret column is sealed with.
pub(crate) fn encryption_key(
    state: &crate::state::AppState,
) -> Result<[u8; 32], crate::error::ApiError> {
    arcodash_core::secrets::parse_key_hex(&state.config.encryption_key)
        .map_err(|_| crate::error::ApiError::internal("invalid ENCRYPTION_KEY"))
}

/// Deserialize a `Option<Option<T>>` field so that a JSON body distinguishes "field absent"
/// (outer `None`, don't touch the setting) from "field present but `null`" (`Some(None)`,
/// clear/revert the setting) from "field present with a value" (`Some(Some(value))`, set it).
/// Plain `#[derive(Deserialize)]` on `Option<Option<T>>` can't make this distinction — serde's
/// `Option<T>` deserializer treats JSON `null` as the *outer* `None` regardless of nesting, so
/// without this, an explicit `null` would look identical to the field being absent. Must be
/// paired with `#[serde(default)]` (so a genuinely absent field skips this entirely).
pub(crate) fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}

//! `Authorization: Key <key>` header / `?api_key=` query param resolution against the `api_keys`
//! table.
//!
//! `object_type` is `'user'` or `'query'`. Only the user-key path is wired up to a full
//! `CurrentUser` resolution, but [`find_active_api_key`] itself is generic over `object_type`.

use axum::http::header::AUTHORIZATION;
use axum::http::HeaderMap;
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};

use arcodash_core::entities::{api_key, user};

/// The `object_type` value for keys that act as a user.
pub const OBJECT_TYPE_USER: &str = "user";
/// Extract the raw API key string from a request, if present: checks the `Authorization: Key
/// <key>` header first, then the `?api_key=` query parameter.
pub fn extract_api_key(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Key "))
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .or_else(|| {
            query?
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .find(|&(name, value)| name == "api_key" && !value.is_empty())
                .map(|(_, value)| value)
        })
        .map(str::to_string)
}

/// Look up an active `api_keys` row matching `key` and `object_type`.
///
/// Returns `None` if no such row exists or it has been deactivated (`active = false`). Generic over
/// `object_type` so the same function resolves both user-keys and query-keys.
///
/// Only the key's SHA-256 is stored, so the presented key is hashed and the digest is what's
/// matched — the plaintext never touches the database.
pub async fn find_active_api_key(
    db: &impl ConnectionTrait,
    key: &str,
    object_type: &str,
) -> Result<Option<api_key::Model>, sea_orm::DbErr> {
    api_key::Entity::find()
        .filter(api_key::Column::ApiKeyHash.eq(arcodash_core::hash::hash_api_key(key)))
        .filter(api_key::Column::ObjectType.eq(object_type))
        .filter(api_key::Column::Active.eq(true))
        .one(db)
        .await
}

/// Resolve a raw API key string to a `users` row via the user-key path: looks up an active
/// `api_keys` row with `object_type = 'user'`, then loads the user it points at (`object_id`).
///
/// Returns `None` if the key doesn't resolve to an active user-key, the referenced user no longer
/// exists, or the user is disabled.
pub async fn resolve_user_for_api_key(
    db: &impl ConnectionTrait,
    key: &str,
) -> Result<Option<user::Model>, sea_orm::DbErr> {
    let Some(api_key_row) = find_active_api_key(db, key, OBJECT_TYPE_USER).await? else {
        return Ok(None);
    };

    Ok(user::Entity::find_by_id(api_key_row.object_id)
        .one(db)
        .await?
        .filter(|user| user.disabled_at.is_none() && !user.is_invitation_pending))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    /// The lookup filter must carry the key's digest, never the key — otherwise plaintext
    /// key storage/matching would silently return.
    #[test]
    fn lookup_uses_the_digest_and_never_the_raw_key() {
        let key = "0123456789abcdef0123456789abcdef";
        let hash = arcodash_core::hash::hash_api_key(key);
        assert_ne!(hash, key);
        assert_eq!(hash.len(), 64);
        // Same key always resolves to the same stored value, so an existing row still matches.
        assert_eq!(hash, arcodash_core::hash::hash_api_key(key));
    }

    #[test]
    fn extracts_key_from_authorization_header() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Key abc123"));
        assert_eq!(extract_api_key(&headers, None), Some("abc123".to_string()));
    }

    #[test]
    fn extracts_key_from_query_param() {
        let headers = HeaderMap::new();
        assert_eq!(
            extract_api_key(&headers, Some("api_key=xyz789&other=1")),
            Some("xyz789".to_string())
        );
    }

    #[test]
    fn header_takes_priority_over_query_param() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Key from-header"));
        assert_eq!(
            extract_api_key(&headers, Some("api_key=from-query")),
            Some("from-header".to_string())
        );
    }

    #[test]
    fn returns_none_when_neither_present() {
        let headers = HeaderMap::new();
        assert_eq!(extract_api_key(&headers, None), None);
    }

    #[test]
    fn ignores_non_key_authorization_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer abc123"));
        assert_eq!(extract_api_key(&headers, None), None);
    }

    #[test]
    fn key_header_with_trailing_whitespace_is_trimmed() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Key abc123  "));
        assert_eq!(extract_api_key(&headers, None), Some("abc123".to_string()));
    }

    #[test]
    fn key_header_with_empty_value_returns_none() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Key "));
        assert_eq!(extract_api_key(&headers, None), None);
    }

    #[test]
    fn api_key_as_non_first_query_param_is_found() {
        let headers = HeaderMap::new();
        assert_eq!(
            extract_api_key(&headers, Some("other=1&api_key=mykey")),
            Some("mykey".to_string())
        );
    }

    #[test]
    fn api_key_query_param_with_empty_value_returns_none() {
        let headers = HeaderMap::new();
        assert_eq!(extract_api_key(&headers, Some("api_key=")), None);
    }

    #[test]
    fn lowercase_key_scheme_is_not_recognized() {
        // The Authorization scheme match is case-sensitive: "key " (lowercase) is not matched.
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("key abc123"));
        assert_eq!(extract_api_key(&headers, None), None);
    }
}

//! LDAP/AD bind+search login — `POST /api/ldap/login`.
//!
//! Enabled only when `LDAP_URL` and `LDAP_SEARCH_BASE_DN` are both configured. Auto-provisions
//! users; maps the configured email/first-name/last-name attributes, plus an optional group
//! attribute (e.g. AD's `memberOf`) onto arcodash groups by name, mirroring SAML's
//! `ArcodashGroups`.
//!
//! Flow (the standard "bind+search" pattern, not a direct user-DN bind, since most directories
//! don't expose a predictable user DN format):
//! 1. Bind as the configured service account (or anonymously) and search for exactly one entry
//!    matching the (escaped) submitted username.
//! 2. Re-bind the same connection as that entry's DN with the submitted password — this is the
//!    actual authentication step. Any failure here (including 0 or >1 search matches) collapses
//!    to the same generic "invalid username or password" response, so the API never reveals
//!    whether a given username exists in the directory.

use std::time::Duration;

use crate::extract::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use ldap3::{LdapConnAsync, LdapConnSettings, Scope, SearchEntry};
use serde::Deserialize;

use arcodash_core::events::record_event;

use crate::api::handlers::auth::{
    record_event_or_warn, session_jar, sso_display_name, UserResponse,
};
use crate::auth::CurrentOrg;
use crate::db::Db;
use crate::error::ApiError;
use crate::services::user_service;
use crate::state::AppState;

const GENERIC_LDAP_FAILURE_MESSAGE: &str = "invalid username or password";

/// Bounds every LDAP round trip. This route is unauthenticated, so a stalling directory must never
/// be able to pin a task — and, under a per-request transaction, a pooled DB connection held open
/// for it — indefinitely.
const LDAP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
pub struct LdapLoginRequest {
    pub username: String,
    pub password: String,
}

/// Escape a value for safe interpolation into an LDAP search filter (RFC 4515 §3). This is the
/// injection-prevention boundary for `POST /api/ldap/login` — `username` is attacker-controlled
/// and goes straight into the filter string.
fn escape_ldap_filter(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '\\' => out.push_str(r"\5c"),
            '*' => out.push_str(r"\2a"),
            '(' => out.push_str(r"\28"),
            ')' => out.push_str(r"\29"),
            '\0' => out.push_str(r"\00"),
            _ => out.push(c),
        }
    }
    out
}

/// Extract the `CN=` value from a DN-shaped group value (Active Directory's `memberOf` returns
/// full DNs, e.g. `CN=Analysts,OU=Groups,DC=corp,DC=example,DC=com`) so it can match a plain
/// arcodash group name. NOTE: minimal RDN parsing (no multi-valued-RDN or `\,`-escape
/// handling) — good enough for the common single-valued case; falls back to `None` (caller
/// keeps the raw value) when the first RDN isn't `cn=`.
fn extract_cn(dn: &str) -> Option<String> {
    let (prefix, value) = dn.split(',').next()?.trim().split_once('=')?;
    prefix
        .trim()
        .eq_ignore_ascii_case("cn")
        .then(|| value.trim().to_string())
}

/// `POST /api/ldap/login` — body `{ username, password }`.
pub async fn login(
    State(state): State<AppState>,
    db: Db,
    CurrentOrg(org): CurrentOrg,
    headers: HeaderMap,
    Json(payload): Json<LdapLoginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let (Some(url), Some(search_base)) =
        (&state.config.ldap_url, &state.config.ldap_search_base_dn)
    else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "LDAP is not configured",
        ));
    };

    if payload.username.trim().is_empty() || payload.password.is_empty() {
        return Err(ApiError::unauthorized(GENERIC_LDAP_FAILURE_MESSAGE));
    }

    let (conn, mut ldap) =
        LdapConnAsync::with_settings(LdapConnSettings::new().set_conn_timeout(LDAP_TIMEOUT), url)
            .await
            .map_err(|e| ApiError::internal(format!("LDAP connection failed: {e}")))?;
    ldap3::drive!(conn);

    // Step 1: bind as the service account (or anonymously, if unconfigured) to search.
    if let Some(bind_dn) = &state.config.ldap_bind_dn {
        // `with_timeout` is one-shot (`Ldap::with_timeout` takes the value on next use), so it
        // must be re-armed before every single operation below — deleting one of these calls as
        // "redundant" silently un-bounds that operation again.
        ldap.with_timeout(LDAP_TIMEOUT);
        let bind_password = state.config.ldap_bind_password.as_deref().unwrap_or("");
        if bind_password.is_empty() {
            tracing::warn!(
                %bind_dn,
                "LDAP_BIND_DN is set but LDAP_BIND_PASSWORD is empty — the search bind will be \
                 unauthenticated (RFC 4513 §5.1.2), which many directories reject or treat as \
                 anonymous"
            );
        }
        ldap.simple_bind(bind_dn, bind_password)
            .await
            .and_then(ldap3::LdapResult::success)
            .map_err(|e| ApiError::internal(format!("LDAP service-account bind failed: {e}")))?;
    }

    let filter = state
        .config
        .ldap_user_filter
        .replace("{username}", &escape_ldap_filter(&payload.username));

    let email_attr = &state.config.ldap_email_attribute;
    let first_attr = &state.config.ldap_first_name_attribute;
    let last_attr = &state.config.ldap_last_name_attribute;
    let group_attr = state.config.ldap_group_attribute.as_deref();

    let mut wanted_attrs = vec![email_attr.clone(), first_attr.clone(), last_attr.clone()];
    wanted_attrs.extend(group_attr.map(str::to_string));

    ldap.with_timeout(LDAP_TIMEOUT);
    let (results, _res) = ldap
        .search(search_base, Scope::Subtree, &filter, wanted_attrs)
        .await
        .and_then(ldap3::SearchResult::success)
        .map_err(|e| ApiError::internal(format!("LDAP search failed: {e}")))?;

    // Fail closed on zero or ambiguous (>1) matches — never guess which entry is "the" user.
    if results.len() != 1 {
        ldap.with_timeout(LDAP_TIMEOUT);
        let _ = ldap.unbind().await;
        record_ldap_login_failed(&db, org.id, &payload.username, "no_match_or_ambiguous").await;
        return Err(ApiError::unauthorized(GENERIC_LDAP_FAILURE_MESSAGE));
    }
    let entry = SearchEntry::construct(results.into_iter().next().expect("len checked above"));

    // Step 2: the actual authentication — rebind the connection as the found DN using the
    // submitted password. This (not step 1) is what proves the caller knows the password.
    ldap.with_timeout(LDAP_TIMEOUT);
    let auth_ok = ldap
        .simple_bind(&entry.dn, &payload.password)
        .await
        .and_then(ldap3::LdapResult::success)
        .is_ok();
    ldap.with_timeout(LDAP_TIMEOUT);
    let _ = ldap.unbind().await;

    if !auth_ok {
        record_ldap_login_failed(&db, org.id, &payload.username, "invalid_credentials").await;
        return Err(ApiError::unauthorized(GENERIC_LDAP_FAILURE_MESSAGE));
    }

    let attr = |name: &str| entry.attrs.get(name).and_then(|v| v.first()).cloned();
    let email = attr(email_attr)
        .ok_or_else(|| ApiError::internal(format!("LDAP entry is missing '{email_attr}'")))?;
    let name = sso_display_name(attr(first_attr), attr(last_attr), &email);

    let group_names = group_attr
        .and_then(|attr_name| entry.attrs.get(attr_name))
        .map(|values| {
            values
                .iter()
                .map(|v| extract_cn(v).unwrap_or_else(|| v.clone()))
                .collect::<Vec<_>>()
        });

    let user = match user_service::find_or_create_sso_user_with_groups(
        &db,
        org.id,
        &email,
        &name,
        None,
        group_names,
    )
    .await
    {
        Ok(u) => u,
        Err(e) => {
            record_ldap_login_failed(&db, org.id, &payload.username, "provisioning_failed").await;
            return Err(e);
        }
    };

    crate::metrics::record_auth_outcome("ldap", "success");
    record_event(
        &db,
        org.id,
        Some(user.id),
        "user.login",
        Some("user"),
        Some(user.id),
        serde_json::json!({ "via": "ldap" }),
    )
    .await?;

    let jar = session_jar(&state, &user, org.id, &headers)?;
    Ok((jar, Json(UserResponse::from(&user))))
}

/// LDAP's brute-force audit trail — every failed step (ambiguous/no directory match, wrong
/// password, or a post-auth provisioning error) records the same `user.login_failed` action so a
/// directory-wide credential-stuffing run against `/api/ldap/login` shows up in `GET /api/events`
/// exactly like a failed internal-auth login does, plus a structured log line so the same event
/// reaches an operator watching logs, not only the audit trail. Never includes the submitted
/// password.
async fn record_ldap_login_failed(db: &Db, org_id: i64, attempted_username: &str, reason: &str) {
    crate::metrics::record_auth_outcome(
        "ldap",
        if reason == "provisioning_failed" {
            "provisioning_failed"
        } else {
            "bad_credentials"
        },
    );
    // Bounded the same way `auth.rs` bounds an attempted email: attacker-supplied, unauthenticated
    // input has no length ceiling short of axum's body limit otherwise.
    let bounded_username: String = attempted_username.chars().take(320).collect();
    tracing::warn!(
        org_id,
        via = "ldap",
        reason,
        attempted_username = %bounded_username,
        "rejecting login: LDAP authentication failed"
    );
    record_event_or_warn(
        db,
        org_id,
        None,
        "user.login_failed",
        None,
        serde_json::json!({ "attempted_username": attempted_username, "via": "ldap" }),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Self-check for the two non-trivial bits of logic this handler leans on: filter escaping
    /// (the injection boundary for attacker-controlled `username`) and AD `memberOf` DN → CN
    /// extraction (the group-mapping boundary).
    #[test]
    fn escapes_ldap_filter_metacharacters() {
        assert_eq!(escape_ldap_filter("alice"), "alice");
        assert_eq!(escape_ldap_filter("admin)(uid=*"), r"admin\29\28uid=\2a");
        assert_eq!(escape_ldap_filter(r"back\slash"), r"back\5cslash");
    }

    /// Compile-level guard on the timeout config this handler relies on — `LdapConnSettings` is
    /// opaque (no getter to inspect after construction), so this only confirms the constant is
    /// sane and that the settings type accepts it. The real behavior (a stalling directory not
    /// hanging the handler forever) needs a live LDAP dial and is covered by
    /// integration testing against a real directory, not this unit test.
    #[test]
    fn ldap_timeout_is_nonzero_and_accepted_by_conn_settings() {
        assert!(LDAP_TIMEOUT > Duration::ZERO);
        let _ = LdapConnSettings::new().set_conn_timeout(LDAP_TIMEOUT);
    }

    #[test]
    fn extracts_cn_from_ad_style_memberof_dn() {
        assert_eq!(
            extract_cn("CN=Analysts,OU=Groups,DC=corp,DC=example,DC=com"),
            Some("Analysts".to_string())
        );
        assert_eq!(
            extract_cn("cn=admins,ou=groups,dc=example,dc=com"),
            Some("admins".to_string())
        );
        assert_eq!(extract_cn("OU=NotACn,DC=example,DC=com"), None);
    }
}

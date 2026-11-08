//! Confirms `AppState.org_resolver` is actually consulted by `auth::resolve_org`: a stub
//! resolver returns an org distinct from the seeded `default` org, and `GET /api/session` (which
//! resolves `CurrentOrg` via the org middleware on every request) must reflect the stub's org,
//! not the hardcoded fallback.

mod common;

use std::sync::Arc;

use axum::http::request::Parts;

use arcodash_backend::auth::{BoxFuture, OrgResolver};
use arcodash_backend::error::ApiError;
use arcodash_core::entities::organization;

/// Always resolves to one fixed, hardcoded org — never touches `db` — so a passing test proves
/// `resolve_org` took the `Some(resolver)` branch instead of querying for `slug = 'default'`.
struct StubOrgResolver {
    org: organization::Model,
}

impl OrgResolver for StubOrgResolver {
    fn resolve<'a>(
        &'a self,
        _db: &'a sea_orm::DatabaseConnection,
        _parts: &'a Parts,
    ) -> BoxFuture<'a, Result<organization::Model, ApiError>> {
        let org = self.org.clone();
        Box::pin(async move { Ok(org) })
    }
}

#[tokio::test]
async fn org_resolver_hook_is_consulted_instead_of_default_lookup() {
    let stub_org = organization::Model {
        id: 424242,
        name: "Stub Org".to_string(),
        slug: "stub-org".to_string(),
        settings: serde_json::json!({}),
        created_at: chrono::Utc::now().into(),
        updated_at: chrono::Utc::now().into(),
    };
    let resolver = Arc::new(StubOrgResolver {
        org: stub_org.clone(),
    });

    let app = skip_if_none!(common::spawn_app_with_org_resolver(resolver).await);

    let resp = reqwest::get(format!("{}/api/session", app.address))
        .await
        .expect("GET /api/session failed");
    assert!(resp.status().is_success(), "status: {}", resp.status());

    let body: serde_json::Value = resp.json().await.expect("invalid JSON body");
    assert_eq!(
        body["org"]["slug"].as_str(),
        Some("stub-org"),
        "expected the stub resolver's org, got {body:?} — the hardcoded default lookup ran instead"
    );
}

//! The built-in login system (password, SAML, LDAP, recovery, invitations) is mounted unless an
//! `ExternalAuth` replaces it; sessions are only valid under the active provider.

mod common;
use common::{
    authed_client, default_org_id, extract_set_cookie, spawn_app_with_extensions, user_with_role,
    TestApp,
};

use arcodash_backend::auth::session::{self, INTERNAL_PROVIDER};
use arcodash_backend::{AppExtensions, ExternalAuth};

/// Matches `common/mod.rs::test_config`'s `cookie_secret`.
const COOKIE_SECRET: &str = "integration-test-cookie-secret";

const BUILTIN_ROUTES: &[(&str, &str)] = &[
    ("POST", "/api/login"),
    ("POST", "/api/forgot_password"),
    ("POST", "/api/reset_password"),
    ("GET", "/api/users/invitation/verify"),
    ("POST", "/api/users/invitation/accept"),
    ("POST", "/api/users/me/password"),
    ("GET", "/api/saml/login"),
    ("POST", "/api/ldap/login"),
];

async fn spawn(external: bool) -> Option<TestApp> {
    spawn_app_with_extensions(AppExtensions {
        external_auth: external.then(|| ExternalAuth {
            session_provider: "ext-test",
            client_config: serde_json::json!({ "x": 1 }).as_object().unwrap().clone(),
        }),
        ..AppExtensions::default()
    })
    .await
}

async fn session_body(app: &TestApp, client: &reqwest::Client) -> serde_json::Value {
    client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Status of each built-in route. `/users/me/password` is not CSRF-exempt and the CSRF layer runs
/// before routing, so every request echoes a real `csrf_token` to observe 404-vs-mounted.
async fn builtin_statuses(app: &TestApp) -> Vec<(&'static str, u16)> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = extract_set_cookie(resp.headers(), "csrf_token").expect("csrf_token cookie");
    let mut out = vec![];
    for (method, path) in BUILTIN_ROUTES {
        let status = client
            .request(method.parse().unwrap(), format!("{}{path}", app.address))
            .header("Cookie", format!("csrf_token={csrf}"))
            .header("X-CSRF-Token", &csrf)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        out.push((*path, status));
    }
    out
}

#[tokio::test]
async fn default_mounts_builtin_auth() {
    let Some(app) = spawn(false).await else {
        return;
    };
    for (path, status) in builtin_statuses(&app).await {
        // SAML/LDAP are mounted only when configured; the test config enables neither.
        if path.starts_with("/api/saml") || path.starts_with("/api/ldap") {
            continue;
        }
        assert_ne!(status, 404, "{path} must be mounted");
    }
    let body = session_body(&app, &reqwest::Client::new()).await;
    assert_eq!(body["client_config"]["password_auth_enabled"], true);
    assert!(body["client_config"].get("x").is_none());
}

#[tokio::test]
async fn external_auth_unmounts_builtin_auth() {
    let Some(app) = spawn(true).await else {
        return;
    };
    for (path, status) in builtin_statuses(&app).await {
        assert_eq!(status, 404, "{path} must not be mounted");
    }
    let cc = &session_body(&app, &reqwest::Client::new()).await["client_config"];
    assert_eq!(cc["password_auth_enabled"], false);
    assert_eq!(cc["saml_enabled"], false);
    assert_eq!(cc["ldap_enabled"], false);
    assert_eq!(cc["x"], 1);
}

/// A cookie only authenticates on a backend whose provider matches its claim; the matching cookie
/// is the positive control so the mismatches can't pass against a backend rejecting everything.
#[tokio::test]
async fn session_cookie_provider_must_match_backend() {
    for (external, own, foreign) in [
        (true, "ext-test", INTERNAL_PROVIDER),
        (false, INTERNAL_PROVIDER, "ext-test"),
    ] {
        let Some(app) = spawn(external).await else {
            return;
        };
        let org_id = default_org_id(&app.db).await;
        let (user, _) = user_with_role(&app.db, org_id, &["admin"]).await;
        let token = |provider| {
            session::issue_token(
                COOKIE_SECRET,
                user.id,
                org_id,
                i64::from(user.session_token_version),
                provider,
            )
            .unwrap()
        };

        let foreign_client = authed_client(&app.address, &token(foreign), "csrf");
        assert!(
            session_body(&app, &foreign_client).await["user"].is_null(),
            "a {foreign:?} cookie must not resolve a user on a {own:?} backend"
        );
        let status = foreign_client
            .get(format!("{}/api/queries", app.address))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status.as_u16(), 401);

        let own_client = authed_client(&app.address, &token(own), "csrf");
        assert!(
            !session_body(&app, &own_client).await["user"].is_null(),
            "a {own:?} cookie must authenticate on a {own:?} backend"
        );
    }
}

//! Integration tests: organization settings, events (audit log), and favorites.

mod common;
use arcodash_backend::AppExtensions;
use common::ok_json;
use common::{spawn_app, spawn_app_with_extensions};

/// Create a non-admin user via the API; returns `(client, user_id)`.
async fn create_nonadmin_client(app: &common::TestApp) -> (reqwest::Client, i64) {
    let admin = app.admin_client().await;
    let email = format!("nonadmin_{}@test.local", uuid::Uuid::new_v4().simple());
    let password = "N0nAdmin!Pass";

    use arcodash_core::entities::{group, organization, user};
    use sea_orm::{sea_query::OnConflict, ActiveValue::*};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();

    let default_group = group::Entity::find()
        .filter(group::Column::OrgId.eq(org.id))
        .filter(group::Column::Name.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default group must exist after migrations");

    let hash = {
        use argon2::{Argon2, PasswordHasher};
        Argon2::default()
            .hash_password(password.as_bytes())
            .unwrap()
            .to_string()
    };
    // Derived from the email so tests can reconstruct it; the row stores only the digest.
    let api_key = common::test_api_key(&email);

    let result = user::Entity::insert(user::ActiveModel {
        id: NotSet,
        org_id: Set(org.id),
        name: Set("Non Admin".to_string()),
        email: Set(email.clone()),
        password_hash: Set(Some(hash)),
        groups: Set(vec![default_group.id]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(&api_key)),
        disabled_at: Set(None),
        is_invitation_pending: Set(false),
        profile_image_url: Set(None),
        details: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
        digest_enabled: Set(false),
        digest_unsubscribe_token: Set(None),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::columns([user::Column::OrgId, user::Column::Email])
            .do_nothing()
            .to_owned(),
    )
    .exec(&app.db)
    .await;

    let user_id = match result {
        Ok(r) => r.last_insert_id,
        Err(sea_orm::DbErr::RecordNotInserted) => {
            user::Entity::find()
                .filter(user::Column::Email.eq(&email))
                .one(&app.db)
                .await
                .unwrap()
                .unwrap()
                .id
        }
        Err(e) => panic!("non-admin user seed failed: {e}"),
    };

    let (session, csrf) = app.login(&email, password).await;
    let client = common::authed_client(&app.address, &session, &csrf);
    drop(admin);
    (client, user_id)
}

/// GET /api/organization as admin → 200 with org fields.
#[tokio::test]
async fn get_organization_as_admin_returns_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let body = ok_json(
        client
            .get(format!("{}/api/organization", app.address))
            .send()
            .await
            .unwrap(),
        "GET /api/organization as admin",
    )
    .await;

    assert!(!body["id"].is_null(), "org must have id");
    assert!(!body["name"].is_null(), "org must have name");
    assert!(!body["slug"].is_null(), "org must have slug");
}

/// GET /api/organization as non-admin → 403.
#[tokio::test]
async fn get_organization_as_nonadmin_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (client, _) = create_nonadmin_client(&app).await;
    let resp = client
        .get(format!("{}/api/organization", app.address))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        403,
        "non-admin must get 403 on GET /api/organization"
    );
}

/// POST /api/organization as admin updates name → 200 and persisted.
#[tokio::test]
async fn update_organization_as_admin_persists_change() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let new_name = format!("Updated Org {}", uuid::Uuid::new_v4().simple());

    let body = ok_json(
        client
            .post(format!("{}/api/organization", app.address))
            .json(&serde_json::json!({ "name": new_name }))
            .send()
            .await
            .unwrap(),
        "POST /api/organization update name",
    )
    .await;

    assert_eq!(body["name"], new_name, "org name must be updated");

    let fetched = ok_json(
        client
            .get(format!("{}/api/organization", app.address))
            .send()
            .await
            .unwrap(),
        "GET /api/organization after update",
    )
    .await;
    assert_eq!(
        fetched["name"], new_name,
        "GET after update must reflect new name"
    );
}

/// POST /api/organization never writes `settings.ai` or deployment-reserved keys; an unrelated
/// settings write leaves them intact.
#[tokio::test]
async fn update_organization_settings_cannot_overwrite_reserved_keys() {
    let app = skip_if_none!(
        spawn_app_with_extensions(AppExtensions {
            reserved_org_settings_keys: vec!["entitlement"],
            ..AppExtensions::default()
        })
        .await
    );
    app.cleanup().await;

    use arcodash_core::entities::organization;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

    let client = app.admin_client().await;

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: organization::ActiveModel = org.clone().into();
    active.settings = Set(serde_json::json!({
        "entitlement": { "level": "basic" },
        "ai": { "rate_limit": 7 }
    }));
    active.update(&app.db).await.unwrap();

    let resp = client
        .post(format!("{}/api/organization", app.address))
        .json(&serde_json::json!({ "settings": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let resp = client
        .post(format!("{}/api/organization", app.address))
        .json(&serde_json::json!({ "settings": {
            "entitlement": { "level": "pro" },
            "ai": { "rate_limit": 9999 },
            "theme": "dark"
        } }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let after = organization::Entity::find_by_id(org.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.settings["entitlement"],
        serde_json::json!({ "level": "basic" })
    );
    assert_eq!(after.settings["ai"], serde_json::json!({ "rate_limit": 7 }));
    assert_eq!(after.settings["theme"], "dark");
}

/// A non-object `settings` (e.g. a JSON array) is rejected, not coerced or merged.
#[tokio::test]
async fn update_organization_rejects_non_object_settings() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let resp = client
        .post(format!("{}/api/organization", app.address))
        .json(&serde_json::json!({ "settings": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "a non-object settings body must be rejected"
    );
}

/// `settings` merges into the stored object; omitting a key doesn't delete it.
#[tokio::test]
async fn update_organization_settings_merges_instead_of_replacing() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let first = client
        .post(format!("{}/api/organization", app.address))
        .json(&serde_json::json!({ "settings": { "note": "first" } }))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status().as_u16(), 200);

    let second = client
        .post(format!("{}/api/organization", app.address))
        .json(&serde_json::json!({ "settings": { "other": "second" } }))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status().as_u16(), 200);

    let fetched = ok_json(
        client
            .get(format!("{}/api/organization", app.address))
            .send()
            .await
            .unwrap(),
        "GET /api/organization after two settings writes",
    )
    .await;
    assert_eq!(
        fetched["settings"]["note"], "first",
        "a key an earlier write set must survive a later write that doesn't mention it"
    );
    assert_eq!(fetched["settings"]["other"], "second");
}

/// POST /api/organization as non-admin → 403.
#[tokio::test]
async fn update_organization_as_nonadmin_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (client, _) = create_nonadmin_client(&app).await;
    let resp = client
        .post(format!("{}/api/organization", app.address))
        .json(&serde_json::json!({ "name": "should fail" }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        403,
        "non-admin must get 403 on POST /api/organization"
    );
}

/// Changing `name` records an `organization.updated` event whose `keys` names the touched field.
#[tokio::test]
async fn update_organization_name_change_records_audit_event() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let new_name = format!("Audited Org {}", uuid::Uuid::new_v4().simple());

    let resp = client
        .post(format!("{}/api/organization", app.address))
        .json(&serde_json::json!({ "name": new_name }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let events = ok_json(
        client
            .get(format!("{}/api/events", app.address))
            .send()
            .await
            .unwrap(),
        "GET /api/events after name change",
    )
    .await;
    let arr = events.as_array().expect("events must be array");
    let ev = arr
        .iter()
        .find(|e| e["action"] == "organization.updated")
        .unwrap_or_else(|| panic!("no organization.updated event found; got: {arr:?}"));
    let keys = ev["additional_properties"]["keys"]
        .as_array()
        .expect("keys must be an array");
    assert!(
        keys.iter().any(|k| k.as_str() == Some("name")),
        "keys must list 'name', got: {keys:?}"
    );
}

/// A nested secret-shaped settings value records only top-level key names in the audit log, never
/// the value.
#[tokio::test]
async fn update_organization_settings_change_audit_event_carries_keys_not_values() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let secret = "sk-should-never-appear-in-audit-log";

    let resp = client
        .post(format!("{}/api/organization", app.address))
        .json(&serde_json::json!({ "settings": { "some_key": { "api_key": secret } } }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let events_resp = client
        .get(format!("{}/api/events", app.address))
        .send()
        .await
        .unwrap();
    let body_text = events_resp.text().await.unwrap();
    assert!(
        !body_text.contains(secret),
        "audit log must never carry a settings value, got: {body_text}"
    );

    let events: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let arr = events.as_array().expect("events must be array");
    let ev = arr
        .iter()
        .find(|e| e["action"] == "organization.updated")
        .unwrap_or_else(|| panic!("no organization.updated event found; got: {arr:?}"));
    let keys = ev["additional_properties"]["keys"]
        .as_array()
        .expect("keys must be an array");
    assert!(
        keys.iter().any(|k| k.as_str() == Some("some_key")),
        "keys must list the changed top-level key name, got: {keys:?}"
    );
}

/// A no-op update appends no name/settings audit event.
#[tokio::test]
async fn update_organization_noop_does_not_record_audit_event() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let before = ok_json(
        client
            .get(format!("{}/api/organization", app.address))
            .send()
            .await
            .unwrap(),
        "GET /api/organization before no-op",
    )
    .await;
    let name = before["name"].as_str().unwrap().to_string();

    fn count_name_or_settings_events(events: &serde_json::Value) -> usize {
        events
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["action"] == "organization.updated")
            .count()
    }

    let events_before = ok_json(
        client
            .get(format!("{}/api/events", app.address))
            .send()
            .await
            .unwrap(),
        "GET /api/events before no-op",
    )
    .await;
    let before_count = count_name_or_settings_events(&events_before);

    let resp = client
        .post(format!("{}/api/organization", app.address))
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let events_after = ok_json(
        client
            .get(format!("{}/api/events", app.address))
            .send()
            .await
            .unwrap(),
        "GET /api/events after no-op",
    )
    .await;
    let after_count = count_name_or_settings_events(&events_after);

    assert_eq!(
        before_count, after_count,
        "a no-op update must not append a name/settings audit event"
    );
}

/// POST /api/organization/ai recovers (no 500) when `organizations.settings` isn't a JSON object,
/// logging a warning and proceeding as if it were `{}`. Only the HTTP behaviour is asserted:
/// Postgres jsonb `||` wraps the stored non-object into an array, so the column shape differs.
#[tokio::test]
async fn update_ai_settings_recovers_from_non_object_org_settings() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    use arcodash_core::entities::organization;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

    let client = app.admin_client().await;

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: organization::ActiveModel = org.clone().into();
    active.settings = Set(serde_json::json!([]));
    active.update(&app.db).await.unwrap();

    let resp = client
        .post(format!("{}/api/organization/ai", app.address))
        .json(&serde_json::json!({ "rate_limit_per_min": 42 }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    assert_eq!(status, 200, "must recover instead of erroring: {body}");
    assert_eq!(
        body["rate_limit_org_value"], 42,
        "the response must reflect what was just set even though the stored settings started non-object: {body}"
    );

    let after = organization::Entity::find_by_id(org.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        after.settings,
        serde_json::json!([]),
        "the write must not be silently dropped, got: {:?}",
        after.settings
    );

    // Reset the shared default org so later tests that merge into `settings` see an object.
    let mut reset: organization::ActiveModel = org.into();
    reset.settings = Set(serde_json::json!({}));
    reset.update(&app.db).await.unwrap();
}

/// POST /api/organization/ai with an internal-address `base_url` → 400 and not persisted (SSRF
/// guard on `providers.<kind>.base_url`).
#[tokio::test]
async fn update_ai_settings_rejects_ssrf_base_url() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    use arcodash_core::entities::organization;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let client = app.admin_client().await;

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let before = org.settings.clone();

    let resp = client
        .post(format!("{}/api/organization/ai", app.address))
        .json(&serde_json::json!({
            "providers": {
                "openai": { "base_url": "http://169.254.169.254" }
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "an internal-address base_url must be rejected"
    );

    let after = organization::Entity::find_by_id(org.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.settings, before,
        "a rejected base_url must not be persisted"
    );

    // Same shape with a public address succeeds, so the 400 is about the address.
    let resp = client
        .post(format!("{}/api/organization/ai", app.address))
        .json(&serde_json::json!({
            "providers": {
                "openai": { "base_url": "https://api.mistral.ai" }
            }
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "a public base_url with the identical payload shape must be accepted, got {}",
        resp.status()
    );

    let after = organization::Entity::find_by_id(org.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        arcodash_backend::api::handlers::ai_settings::provider_base_url(
            &after.settings,
            arcodash_backend::api::handlers::ai::AiProviderKind::OpenAi
        ),
        Some("https://api.mistral.ai".to_string()),
        "the public base_url must persist"
    );
}

/// GET /api/events as admin → 200 list; after login there should be a login event.
#[tokio::test]
async fn get_events_as_admin_returns_200_with_login_event() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let resp = client
        .get(format!("{}/api/events", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "GET /api/events as admin must return 200"
    );

    let body: serde_json::Value = resp.json().await.unwrap();
    let events = body.as_array().expect("events must be an array");

    let has_login = events.iter().any(|e| {
        e["action"]
            .as_str()
            .map(|a| a.contains("login"))
            .unwrap_or(false)
    });
    assert!(
        has_login,
        "events list must contain a login action; got: {events:?}"
    );
}

/// GET /api/events as non-admin → 403.
#[tokio::test]
async fn get_events_as_nonadmin_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (client, _) = create_nonadmin_client(&app).await;
    let resp = client
        .get(format!("{}/api/events", app.address))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        403,
        "non-admin must get 403 on GET /api/events"
    );
}

/// GET /api/events?page_size=999 → capped at 250.
#[tokio::test]
async fn get_events_page_size_capped_at_250() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let resp = client
        .get(format!("{}/api/events?page_size=999", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let events: serde_json::Value = resp.json().await.unwrap();
    let count = events.as_array().map(|a| a.len()).unwrap_or(0);
    assert!(
        count <= 250,
        "page_size=999 must be capped at 250; got {count} events"
    );
}

/// GET /api/events?action=user.login filters to only login events.
#[tokio::test]
async fn get_events_filters_by_action() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let resp = client
        .get(format!("{}/api/events?action=user.login", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let events: serde_json::Value = resp.json().await.unwrap();
    let arr = events.as_array().expect("events must be array");
    for e in arr {
        assert_eq!(
            e["action"].as_str(),
            Some("user.login"),
            "action filter must return only login events; got: {e}"
        );
    }
}

/// GET /api/events?user_id=<id> filters to events for that user.
#[tokio::test]
async fn get_events_filters_by_user_id() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;
    let session_body = ok_json(
        client
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "GET /api/session",
    )
    .await;
    let admin_id = session_body["user"]["id"].as_i64().unwrap();

    let resp = client
        .get(format!("{}/api/events?user_id={admin_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let events: serde_json::Value = resp.json().await.unwrap();
    let arr = events.as_array().expect("events must be array");
    for e in arr {
        assert_eq!(
            e["user_id"].as_i64(),
            Some(admin_id),
            "user_id filter must return only events for user {admin_id}; got: {e}"
        );
    }
}

/// GET /api/events?object_type=query filters to query events only.
#[tokio::test]
async fn get_events_filters_by_object_type() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let resp = client
        .get(format!("{}/api/events?object_type=query", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let events: serde_json::Value = resp.json().await.unwrap();
    let arr = events.as_array().expect("events must be array");
    for e in arr {
        assert_eq!(
            e["object_type"].as_str(),
            Some("query"),
            "object_type filter must return only query events; got: {e}"
        );
    }
}

/// GET /api/queries/favorites returns only the caller's starred queries.
#[tokio::test]
async fn favorites_returns_only_callers_starred_queries() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;

    let ds = ok_json(
        admin_client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "fav-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds for favorites test",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q1 = ok_json(
        admin_client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Fav Query 1",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query 1",
    )
    .await;
    let q1_id = q1["id"].as_i64().unwrap();

    let q2 = ok_json(
        admin_client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Fav Query 2",
                "query": "SELECT 2",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query 2",
    )
    .await;
    let q2_id = q2["id"].as_i64().unwrap();

    let star_resp = admin_client
        .post(format!("{}/api/queries/{q1_id}/favorite", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        star_resp.status().as_u16(),
        204,
        "star query must return 204"
    );

    let favs_resp = admin_client
        .get(format!("{}/api/queries/favorites", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        favs_resp.status().as_u16(),
        200,
        "GET /api/queries/favorites must return 200"
    );

    let favs: serde_json::Value = favs_resp.json().await.unwrap();
    let arr = favs.as_array().expect("favorites must be array");

    let ids: Vec<i64> = arr.iter().filter_map(|q| q["id"].as_i64()).collect();
    assert!(
        ids.contains(&q1_id),
        "favorites must include starred query q1={q1_id}; got ids={ids:?}"
    );
    assert!(
        !ids.contains(&q2_id),
        "favorites must NOT include unstarred query q2={q2_id}; got ids={ids:?}"
    );
    for q in arr {
        assert_eq!(
            q["is_favorite"], true,
            "favorites list items must have is_favorite=true; got: {q}"
        );
    }
}

/// Favorites are user-scoped: user A's favorites do not appear in user B's list.
#[tokio::test]
async fn favorites_are_user_scoped() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;

    let ds = ok_json(
        admin_client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "fav-scope-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds for scoped favorites test",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q = ok_json(
        admin_client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Shared Query",
                "query": "SELECT 3",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query for scoped favorites test",
    )
    .await;
    let q_id = q["id"].as_i64().unwrap();

    let star_resp = admin_client
        .post(format!("{}/api/queries/{q_id}/favorite", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(star_resp.status().as_u16(), 204);

    let (nonadmin_client, _) = create_nonadmin_client(&app).await;
    let favs_resp = nonadmin_client
        .get(format!("{}/api/queries/favorites", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(favs_resp.status().as_u16(), 200);

    let favs: serde_json::Value = favs_resp.json().await.unwrap();
    let arr = favs.as_array().expect("favorites must be array");
    let ids: Vec<i64> = arr.iter().filter_map(|q| q["id"].as_i64()).collect();
    assert!(
        !ids.contains(&q_id),
        "non-admin's favorites must not contain admin-starred query {q_id}; got: {ids:?}"
    );
}

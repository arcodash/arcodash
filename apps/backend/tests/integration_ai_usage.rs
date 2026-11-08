//! Integration tests: `GET /api/organization/ai/usage` — aggregate-only AI usage metering.
//! Rows are inserted directly (not via a real provider call — no live key needed to test
//! aggregation), then the endpoint's totals/by-provider/by-day rollups are checked against what
//! was inserted, including the empty-state (zero rows) case and the admin-only gate. Also asserts
//! the response never carries anything resembling an individual call (no `id` field, no
//! per-call timestamp).

mod common;
use common::ok_json;
use common::{authed_client, spawn_app, TestApp};

use arcodash_core::entities::ai_usage;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

/// Invite a fresh non-admin user (empty groups), accept the invitation, log them in, and return
/// an authenticated client for them — same invite→accept→login flow as
/// `integration_dashboards.rs::non_admin_client`.
async fn non_admin_client(app: &TestApp, admin: &reqwest::Client, email: &str) -> reqwest::Client {
    let password = "N0nAdm1n!Pass";
    let invited = ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({ "name": "Non Admin", "email": email, "groups": [] }))
            .send()
            .await
            .unwrap(),
        "invite non-admin user",
    )
    .await;
    let user_id = invited["id"].as_i64().unwrap();

    let token = arcodash_core::entities::user::Entity::find_by_id(user_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .details["invitation_token"]
        .as_str()
        .expect("invitation_token must be set on invited user")
        .to_string();

    let bare = reqwest::Client::builder()
        .cookie_store(false)
        .build()
        .unwrap();
    let accept = bare
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(&serde_json::json!({ "token": token, "name": "Non Admin", "password": password }))
        .send()
        .await
        .unwrap();
    assert!(
        accept.status().is_success(),
        "accept invitation failed: {}",
        accept.status()
    );

    let (session, csrf) = app.login(email, password).await;
    authed_client(&app.address, &session, &csrf)
}

async fn insert_row(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    provider: &str,
    status: &str,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
    days_ago: i64,
) {
    insert_feature_row(
        db,
        org_id,
        "sql_generation",
        provider,
        status,
        tokens_in,
        tokens_out,
        days_ago,
    )
    .await;
}

/// Insert a row with explicit user, cost and timestamp.
async fn insert_costed(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    feature: &str,
    user_id: Option<i64>,
    cost_usd: Option<f64>,
    created_at: chrono::DateTime<chrono::Utc>,
) {
    ai_usage::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        org_id: Set(org_id),
        provider: Set("openai".to_string()),
        model: Set("test-model".to_string()),
        feature: Set(feature.to_string()),
        status: Set("ok".to_string()),
        tokens_in: Set(Some(10)),
        tokens_out: Set(Some(5)),
        user_id: Set(user_id),
        cost_usd: Set(cost_usd),
        created_at: Set(created_at.into()),
    }
    .insert(db)
    .await
    .expect("ai_usage insert failed");
}

async fn admin_ids(app: &TestApp, admin: &reqwest::Client) -> (i64, i64) {
    let session = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    (
        session["org"]["id"].as_i64().expect("org id"),
        session["user"]["id"].as_i64().expect("user id"),
    )
}

async fn get_usage(app: &TestApp, admin: &reqwest::Client, qs: &str) -> serde_json::Value {
    ok_json(
        admin
            .get(format!("{}/api/organization/ai/usage{qs}", app.address))
            .send()
            .await
            .unwrap(),
        "get usage",
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn insert_feature_row(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    feature: &str,
    provider: &str,
    status: &str,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
    days_ago: i64,
) {
    let created_at = sea_orm::prelude::DateTimeWithTimeZone::from(
        chrono::Utc::now() - chrono::Duration::days(days_ago),
    );
    ai_usage::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        org_id: Set(org_id),
        provider: Set(provider.to_string()),
        model: Set("test-model".to_string()),
        feature: Set(feature.to_string()),
        status: Set(status.to_string()),
        tokens_in: Set(tokens_in),
        tokens_out: Set(tokens_out),
        user_id: Set(None),
        cost_usd: Set(None),
        created_at: Set(created_at),
    }
    .insert(db)
    .await
    .expect("ai_usage insert failed");
}

/// An org with zero `ai_usage` rows gets a valid, zero-valued response — not an error.
#[tokio::test]
async fn empty_state_returns_zeroed_response() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let body = ok_json(
        admin
            .get(format!("{}/api/organization/ai/usage", app.address))
            .send()
            .await
            .unwrap(),
        "get usage (empty)",
    )
    .await;

    assert_eq!(body["totals"]["total_calls"], 0);
    assert_eq!(body["totals"]["tokens_in"], 0);
    assert_eq!(body["totals"]["tokens_out"], 0);
    assert_eq!(body["totals"]["success_rate"], 0.0);
    assert_eq!(body["by_provider"].as_array().unwrap().len(), 0);
    assert_eq!(body["by_day"].as_array().unwrap().len(), 0);
}

/// Totals, by-provider, and by-day rollups all match a known set of directly-inserted rows, and
/// the response never carries an `id`/individual-call timestamp anywhere.
#[tokio::test]
async fn aggregates_match_inserted_rows() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let session = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    let org_id = session["org"]["id"].as_i64().expect("org id");

    // 3 openai calls (2 ok, 1 invalid_key), today.
    insert_row(&app.db, org_id, "openai", "ok", Some(100), Some(50), 0).await;
    insert_row(&app.db, org_id, "openai", "ok", Some(200), Some(75), 0).await;
    insert_row(&app.db, org_id, "openai", "invalid_key", None, None, 0).await;
    // 1 gemini call (ok), 2 days ago — a different day bucket than the rows above.
    insert_row(&app.db, org_id, "gemini", "ok", Some(30), Some(10), 2).await;
    // A row far outside the default 30-day window must not be counted.
    insert_row(
        &app.db,
        org_id,
        "anthropic",
        "ok",
        Some(999),
        Some(999),
        200,
    )
    .await;

    let body = ok_json(
        admin
            .get(format!("{}/api/organization/ai/usage", app.address))
            .send()
            .await
            .unwrap(),
        "get usage",
    )
    .await;

    // Totals: 4 in-window calls (3 openai + 1 gemini), 3 ok / 1 error.
    assert_eq!(body["totals"]["total_calls"], 4);
    assert_eq!(body["totals"]["tokens_in"], 330); // 100+200+30
    assert_eq!(body["totals"]["tokens_out"], 135); // 50+75+10
    assert_eq!(body["totals"]["success_rate"], 0.75); // 3/4

    let by_provider = body["by_provider"].as_array().unwrap();
    assert_eq!(
        by_provider.len(),
        2,
        "anthropic's out-of-window row must be excluded: {by_provider:#?}"
    );
    let openai = by_provider
        .iter()
        .find(|p| p["provider"] == "openai")
        .expect("openai row");
    assert_eq!(openai["calls"], 3);
    assert_eq!(openai["tokens_in"], 300);
    assert_eq!(openai["tokens_out"], 125);
    assert!((openai["error_rate"].as_f64().unwrap() - (1.0 / 3.0)).abs() < 1e-9);

    let gemini = by_provider
        .iter()
        .find(|p| p["provider"] == "gemini")
        .expect("gemini row");
    assert_eq!(gemini["calls"], 1);
    assert_eq!(gemini["error_rate"], 0.0);

    // by_day: 2 distinct days for openai(ok)+openai(invalid_key) today, gemini(ok) 2 days ago.
    let by_day = body["by_day"].as_array().unwrap();
    let total_by_day: i64 = by_day.iter().map(|r| r["count"].as_i64().unwrap()).sum();
    assert_eq!(
        total_by_day, 4,
        "by_day counts must sum to the in-window total: {by_day:#?}"
    );
    assert!(
        by_day
            .iter()
            .any(|r| r["status"] == "ok" && r["count"] == 2),
        "expected an ok/2 bucket for today's two successful openai calls: {by_day:#?}"
    );
    assert!(
        by_day
            .iter()
            .any(|r| r["status"] == "invalid_key" && r["count"] == 1),
        "expected an invalid_key/1 bucket: {by_day:#?}"
    );

    // Hard rule: never anything resembling a per-call row. `by_day` entries carry only a
    // day-bucketed date string, never a full timestamp; no row anywhere carries an `id`.
    let body_text = body.to_string();
    assert!(
        !body_text.contains("\"id\":"),
        "response must never expose a row id: {body_text}"
    );
    for row in by_day {
        let date = row["date"].as_str().expect("date must be a string");
        assert_eq!(
            date.len(),
            10,
            "date must be day-bucketed (YYYY-MM-DD), not a timestamp: {date}"
        );
        assert!(
            !date.contains('T'),
            "date must not carry a time component: {date}"
        );
    }
}

/// `?days=` narrows the lookback window — a row outside it is excluded from every rollup.
#[tokio::test]
async fn days_param_narrows_the_window() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let session = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    let org_id = session["org"]["id"].as_i64().expect("org id");

    insert_row(&app.db, org_id, "openai", "ok", Some(10), Some(5), 1).await;
    insert_row(&app.db, org_id, "openai", "ok", Some(10), Some(5), 10).await;

    let body = ok_json(
        admin
            .get(format!("{}/api/organization/ai/usage?days=7", app.address))
            .send()
            .await
            .unwrap(),
        "get usage (days=7)",
    )
    .await;
    assert_eq!(body["days"], 7);
    assert_eq!(body["totals"]["total_calls"], 1);
}

/// Non-admin users get 403, matching every other `/organization/ai*` handler's gate.
#[tokio::test]
async fn non_admin_is_forbidden() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let other = non_admin_client(&app, &admin, "ai-usage-non-admin@test.local").await;

    let resp = other
        .get(format!("{}/api/organization/ai/usage", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

/// Per-feature rollup: counts, tokens and error rate per feature, scoped to the caller's org and
/// window (another org's rows and out-of-window rows are excluded).
#[tokio::test]
async fn by_feature_groups_and_scopes_to_org() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let session = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    let org_id = session["org"]["id"].as_i64().expect("org id");
    let other_org = common::make_org(&app.db).await;

    let db = &app.db;
    insert_feature_row(
        db,
        org_id,
        "explain_alert",
        "openai",
        "ok",
        Some(10),
        Some(5),
        0,
    )
    .await;
    insert_feature_row(
        db,
        org_id,
        "explain_alert",
        "openai",
        "failed",
        None,
        None,
        0,
    )
    .await;
    insert_feature_row(
        db,
        org_id,
        "explain_alert",
        "openai",
        "ok",
        Some(20),
        Some(15),
        1,
    )
    .await;
    insert_feature_row(db, org_id, "ask", "gemini", "ok", Some(7), Some(3), 0).await;
    insert_feature_row(db, org_id, "ask", "gemini", "ok", Some(1), Some(1), 200).await;
    insert_feature_row(
        db,
        other_org,
        "ask",
        "gemini",
        "ok",
        Some(500),
        Some(500),
        0,
    )
    .await;
    insert_feature_row(
        db,
        other_org,
        "describe_schema",
        "gemini",
        "ok",
        Some(1),
        Some(1),
        0,
    )
    .await;

    let body = ok_json(
        admin
            .get(format!("{}/api/organization/ai/usage", app.address))
            .send()
            .await
            .unwrap(),
        "get usage",
    )
    .await;
    let by_feature = body["by_feature"].as_array().unwrap();
    assert_eq!(by_feature.len(), 2, "{by_feature:#?}");

    let alert = &by_feature[0];
    assert_eq!(alert["feature"], "explain_alert");
    assert_eq!(alert["calls"], 3);
    assert_eq!(alert["tokens_in"], 30);
    assert_eq!(alert["tokens_out"], 20);
    assert!((alert["error_rate"].as_f64().unwrap() - (1.0 / 3.0)).abs() < 1e-9);

    let ask = &by_feature[1];
    assert_eq!(ask["feature"], "ask");
    assert_eq!(ask["calls"], 1);
    assert_eq!(ask["tokens_in"], 7);
    assert_eq!(ask["error_rate"], 0.0);
}

/// Cost sums line up in every rollup; by_user has one null-user row plus joined email/name.
#[tokio::test]
async fn cost_and_by_user_rollups() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let (org_id, uid) = admin_ids(&app, &admin).await;
    let now = chrono::Utc::now();

    insert_costed(&app.db, org_id, "ask", Some(uid), Some(0.25), now).await;
    insert_costed(&app.db, org_id, "ask", Some(uid), Some(0.5), now).await;
    insert_costed(&app.db, org_id, "explain_alert", None, Some(0.125), now).await;
    insert_costed(&app.db, org_id, "explain_alert", None, None, now).await;

    let body = get_usage(&app, &admin, "").await;
    assert_eq!(body["totals"]["cost_usd"], 0.875);
    let feat = |f: &str| {
        body["by_feature"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["feature"] == f)
            .unwrap()
            .clone()
    };
    assert_eq!(feat("ask")["cost_usd"], 0.75);
    assert_eq!(feat("explain_alert")["cost_usd"], 0.125);
    assert_eq!(body["by_provider"][0]["cost_usd"], 0.875);
    let day_cost: f64 = body["by_day"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["cost_usd"].as_f64().unwrap_or(0.0))
        .sum();
    assert!((day_cost - 0.875).abs() < 1e-9);

    let by_user = body["by_user"].as_array().unwrap();
    assert_eq!(by_user.len(), 2, "{by_user:#?}");
    assert_eq!(by_user[0]["user_id"], uid);
    assert_eq!(by_user[0]["email"], "admin@test.local");
    assert!(by_user[0]["name"].is_string());
    assert_eq!(by_user[0]["calls"], 2);
    assert_eq!(by_user[0]["tokens_in"], 20);
    assert_eq!(by_user[0]["cost_usd"], 0.75);
    assert!(by_user[1]["user_id"].is_null());
    assert!(by_user[1]["email"].is_null());
    assert_eq!(by_user[1]["calls"], 2);
    assert_eq!(by_user[1]["cost_usd"], 0.125);
}

/// With no priced calls, cost is null (not 0) everywhere, and spent_month_usd is a plain 0.
#[tokio::test]
async fn unpriced_cost_serialises_as_null() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let (org_id, _) = admin_ids(&app, &admin).await;
    insert_costed(&app.db, org_id, "ask", None, None, chrono::Utc::now()).await;

    let body = get_usage(&app, &admin, "").await;
    assert!(body["totals"]["cost_usd"].is_null());
    assert!(body["by_provider"][0]["cost_usd"].is_null());
    assert!(body["by_feature"][0]["cost_usd"].is_null());
    assert!(body["by_day"][0]["cost_usd"].is_null());
    assert!(body["by_user"][0]["cost_usd"].is_null());
    assert_eq!(body["budget"]["spent_month_usd"], 0.0);
}

/// The budget block is calendar-month based (ignores `days`) and reflects the org setting.
#[tokio::test]
async fn budget_block_is_month_scoped_and_reads_setting() {
    use arcodash_core::entities::organization;
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let (org_id, _) = admin_ids(&app, &admin).await;
    let now = chrono::Utc::now();
    let (month_start, next_month) = arcodash_core::ai::budget::month_window(now);

    insert_costed(&app.db, org_id, "ask", None, Some(1.5), now).await;
    insert_costed(
        &app.db,
        org_id,
        "ask",
        None,
        Some(100.0),
        month_start - chrono::Duration::days(1),
    )
    .await;

    let body = get_usage(&app, &admin, "?days=90").await;
    assert_eq!(body["totals"]["cost_usd"], 101.5);
    assert_eq!(body["budget"]["spent_month_usd"], 1.5);
    assert!(body["budget"]["monthly_usd"].is_null());
    assert_eq!(
        body["budget"]["resets_at"],
        serde_json::Value::String(next_month.to_rfc3339())
    );

    let org = organization::Entity::find_by_id(org_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut settings = org.settings.clone();
    settings["ai"] = serde_json::json!({ "monthly_budget_usd": 25.0 });
    let mut am: organization::ActiveModel = org.into();
    am.settings = Set(settings);
    am.update(&app.db).await.unwrap();

    let body = get_usage(&app, &admin, "").await;
    assert_eq!(body["budget"]["monthly_usd"], 25.0);
}

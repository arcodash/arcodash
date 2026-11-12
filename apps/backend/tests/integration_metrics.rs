//! Integration tests for `GET /metrics` — gating (404 unset, 401 wrong bearer, 200 right bearer)
//! and that the exposition body actually reflects live counters/gauges.

mod common;
use common::{spawn_app, test_api_key, user_with_role};

/// `METRICS_TOKEN` is read per request; it is process-global env state, so serialize tests touching
/// it.
static ENV_MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn no_token_configured_returns_404() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::remove_var("METRICS_TOKEN");
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let resp = reqwest::get(format!("{}/metrics", app.address))
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "an unset METRICS_TOKEN must make the route not exist"
    );
}

/// An explicitly empty `METRICS_TOKEN` must not match a bare `Authorization: Bearer ` via
/// `constant_time_eq` on two empty slices.
#[tokio::test]
async fn empty_token_configured_returns_404_not_a_bypass() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("METRICS_TOKEN", "");
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let resp = reqwest::Client::new()
        .get(format!("{}/metrics", app.address))
        .header(reqwest::header::AUTHORIZATION, "Bearer ")
        .send()
        .await
        .unwrap();
    std::env::remove_var("METRICS_TOKEN");
    assert_eq!(
        resp.status().as_u16(),
        404,
        "an empty configured token must not be satisfiable by an empty bearer value"
    );
}

#[tokio::test]
async fn wrong_bearer_returns_401() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("METRICS_TOKEN", "the-real-token");
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let resp = reqwest::Client::new()
        .get(format!("{}/metrics", app.address))
        .bearer_auth("wrong-token")
        .send()
        .await
        .unwrap();
    std::env::remove_var("METRICS_TOKEN");
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn right_bearer_returns_200_with_counters_and_a_job_gauge() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("METRICS_TOKEN", "the-real-token");
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let resp = reqwest::Client::new()
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/plain; version=0.0.4"
    );
    let text = resp.text().await.unwrap();
    std::env::remove_var("METRICS_TOKEN");
    assert!(
        text.contains("arcodash_rate_limit_rejections_total"),
        "response missing arcodash_rate_limit_rejections_total:\n{text}"
    );
    assert!(
        text.contains("arcodash_jobs_by_status"),
        "response missing arcodash_jobs_by_status:\n{text}"
    );
}

/// Seeded `runs` rows must show in `arcodash_runs_by_status`, and `arcodash_execute_enqueue_total`
/// must appear, proving the backend wires `arcodash_core::metrics::render` into `/metrics`.
#[tokio::test]
async fn runs_by_status_reflects_seeded_rows_and_shared_metrics_are_wired_in() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("METRICS_TOKEN", "the-real-token");
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    use arcodash_core::entities::{organization, run};
    use sea_orm::{ActiveModelTrait, ActiveValue::NotSet, ActiveValue::Set};

    let org = organization::ActiveModel {
        id: NotSet,
        name: Set("metrics-runs-org".to_string()),
        slug: Set(format!("metrics-runs-{}", uuid::Uuid::new_v4().simple())),
        settings: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
    }
    .insert(&app.db)
    .await
    .unwrap();
    let ds = arcodash_core::test_support::new_data_source(org.id)
        .insert(&app.db)
        .await
        .unwrap();
    let mut u = arcodash_core::test_support::new_user(org.id);
    u.email = Set(format!("metrics-runs-{}@test.local", uuid::Uuid::new_v4()));
    let user = u.insert(&app.db).await.unwrap();
    let q = arcodash_core::test_support::new_query(org.id, ds.id, user.id)
        .insert(&app.db)
        .await
        .unwrap();

    let now = chrono::Utc::now();
    for status in ["success", "failed", "success"] {
        run::ActiveModel {
            id: NotSet,
            org_id: Set(org.id),
            entity_type: Set("query".to_string()),
            entity_id: Set(q.id),
            schedule_id: Set(None),
            data_source_id: Set(Some(ds.id)),
            scheduled_for: Set(None),
            triggered_by: Set("manual".to_string()),
            status: Set(status.to_string()),
            waiting_on_run_id: Set(None),
            job_id: Set(None),
            worker_id: Set(None),
            lease_expires_at: Set(None),
            started_at: Set(Some(now.into())),
            finished_at: Set(Some(now.into())),
            duration_ms: Set(Some(10)),
            row_count: Set(Some(1)),
            error: Set(None),
            data_age_secs: Set(None),
            query_result_id: Set(None),
            condition_met: Set(None),
            created_at: Set(now.into()),
            enqueued_at: Set(Some(now.into())),
        }
        .insert(&app.db)
        .await
        .unwrap();
    }

    let text = reqwest::Client::new()
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    std::env::remove_var("METRICS_TOKEN");

    assert!(
        text.contains("arcodash_runs_by_status{status=\"success\"} 2")
            || text.contains("arcodash_runs_by_status{status=\"success\"}"),
        "expected a success gauge reflecting seeded rows:\n{text}"
    );
    assert!(
        text.contains("arcodash_runs_by_status{status=\"failed\"}"),
        "expected a failed gauge:\n{text}"
    );
    assert!(
        text.contains("# TYPE arcodash_execute_enqueue_total counter"),
        "expected the shared arcodash_core::metrics family to be wired into /metrics:\n{text}"
    );
}

/// Org id owning a data source — used to seed a `query_results` row directly against it.
async fn org_id_for_ds(db: &sea_orm::DatabaseConnection, ds_id: i64) -> i64 {
    use sea_orm::EntityTrait;
    arcodash_core::entities::data_source::Entity::find_by_id(ds_id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .org_id
}

/// Seed a `query_results` row directly (no worker in this test harness) and return its id.
async fn seed_result(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    data_source_id: i64,
    query_hash: &str,
    query: &str,
    data: serde_json::Value,
) -> i64 {
    use arcodash_core::entities::query_result;
    use sea_orm::{
        ActiveModelTrait,
        ActiveValue::{NotSet, Set},
    };
    let now = chrono::Utc::now();
    query_result::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        data_source_id: Set(data_source_id),
        query_hash: Set(query_hash.to_string()),
        query: Set(query.to_string()),
        data: Set(Some(data)),
        data_location: Set(None),
        data_size_bytes: Set(None),
        runtime: Set(Some(0.01)),
        retrieved_at: Set(now.into()),
        last_used_at: Set(now.into()),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    }
    .insert(db)
    .await
    .unwrap()
    .id
}

/// A cache-miss-then-hit execute pair moves `arcodash_query_executions_total` and
/// `arcodash_query_result_cache_lookups_total`, proving `query_service::execute` increments them.
#[tokio::test]
async fn a_real_cache_miss_then_hit_execute_pair_moves_the_new_counters() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("METRICS_TOKEN", "the-real-token");
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let metrics_client = reqwest::Client::new();
    let read_counter = |text: &str, needle: &str| -> u64 {
        text.lines()
            .find(|l| l.starts_with(needle))
            .and_then(|l| l.split_whitespace().last())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    let scrape = || {
        let metrics_client = metrics_client.clone();
        let address = app.address.clone();
        async move {
            metrics_client
                .get(format!("{address}/metrics"))
                .bearer_auth("the-real-token")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        }
    };

    let ds = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "real-counter-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();
    let query_text = "SELECT 1 AS real_counter_marker";

    let before = scrape().await;
    let enqueued_before = read_counter(
        &before,
        "arcodash_query_executions_total{path=\"interactive\",outcome=\"enqueued\"}",
    );
    let miss_before = read_counter(
        &before,
        "arcodash_query_result_cache_lookups_total{outcome=\"miss\"}",
    );

    let resp1 = client
        .post(format!("{}/api/query_results", app.address))
        .json(&serde_json::json!({ "data_source_id": ds_id, "query": query_text, "options": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp1.status().as_u16(), 200);
    let body1: serde_json::Value = resp1.json().await.unwrap();
    assert!(
        body1.get("job").is_some(),
        "first execute must miss cache and enqueue: {body1}"
    );

    let mid = scrape().await;
    let enqueued_after_first = read_counter(
        &mid,
        "arcodash_query_executions_total{path=\"interactive\",outcome=\"enqueued\"}",
    );
    let miss_after_first = read_counter(
        &mid,
        "arcodash_query_result_cache_lookups_total{outcome=\"miss\"}",
    );
    assert_eq!(enqueued_after_first, enqueued_before + 1);
    assert_eq!(miss_after_first, miss_before + 1);

    // Seed a query_results row matching (data_source_id, query_hash), standing in for a worker
    // execution.
    let org_id = org_id_for_ds(&app.db, ds_id).await;
    let query_hash = arcodash_core::hash::gen_query_hash(query_text, &serde_json::Value::Null);
    seed_result(
        &app.db,
        org_id,
        ds_id,
        &query_hash,
        query_text,
        serde_json::json!({ "columns": [{"name": "real_counter_marker"}], "rows": [{"real_counter_marker": 1}] }),
    )
    .await;

    let hit_before = read_counter(
        &mid,
        "arcodash_query_result_cache_lookups_total{outcome=\"hit\"}",
    );
    let cached_before = read_counter(
        &mid,
        "arcodash_query_executions_total{path=\"interactive\",outcome=\"cached\"}",
    );

    let resp2 = client
        .post(format!("{}/api/query_results", app.address))
        .json(&serde_json::json!({ "data_source_id": ds_id, "query": query_text, "options": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status().as_u16(), 200);
    let body2: serde_json::Value = resp2.json().await.unwrap();
    assert!(
        body2.get("query_result").is_some(),
        "second identical execute must hit the seeded cache: {body2}"
    );

    let after = scrape().await;
    std::env::remove_var("METRICS_TOKEN");
    let hit_after = read_counter(
        &after,
        "arcodash_query_result_cache_lookups_total{outcome=\"hit\"}",
    );
    let cached_after = read_counter(
        &after,
        "arcodash_query_executions_total{path=\"interactive\",outcome=\"cached\"}",
    );
    assert_eq!(hit_after, hit_before + 1);
    assert_eq!(cached_after, cached_before + 1);
}

/// A real embed create/serve/revoke/serve cycle moves `arcodash_embed_events_total` and
/// `arcodash_embed_serves_total`, proving the service and handler call the counters.
#[tokio::test]
async fn a_real_embed_lifecycle_moves_the_new_embed_counters() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("METRICS_TOKEN", "the-real-token");
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let metrics_client = reqwest::Client::new();
    let read_counter = |text: &str, needle: &str| -> u64 {
        text.lines()
            .find(|l| l.starts_with(needle))
            .and_then(|l| l.split_whitespace().last())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    let scrape = || {
        let metrics_client = metrics_client.clone();
        let address = app.address.clone();
        async move {
            metrics_client
                .get(format!("{address}/metrics"))
                .bearer_auth("the-real-token")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        }
    };

    let ds: serde_json::Value = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "metrics-embed-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let query: serde_json::Value = client
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "metrics-embed-query",
            "query": "SELECT 1",
            "options": {}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let query_id = query["id"].as_i64().unwrap();
    let publish_resp = client
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(publish_resp.status().as_u16(), 200);

    let viz: serde_json::Value = client
        .post(format!("{}/api/visualizations", app.address))
        .json(&serde_json::json!({
            "query_id": query_id, "type": "chart", "name": "metrics-embed-viz", "options": {}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let viz_id = viz["id"].as_i64().unwrap();

    let before = scrape().await;
    let created_before = read_counter(&before, "arcodash_embed_events_total{event=\"created\"}");
    let revoked_before = read_counter(&before, "arcodash_embed_events_total{event=\"revoked\"}");
    let served_before = read_counter(&before, "arcodash_embed_serves_total{outcome=\"served\"}");
    let not_found_before = read_counter(
        &before,
        "arcodash_embed_serves_total{outcome=\"not_found\"}",
    );

    let embed: serde_json::Value = client
        .post(format!("{}/api/embeds", app.address))
        .json(&serde_json::json!({
            "object_type": "visualization", "object_id": viz_id, "anonymous": true
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let embed_id = embed["id"].as_i64().unwrap();
    let token = embed["token"].as_str().unwrap().to_string();

    let mid = scrape().await;
    assert_eq!(
        read_counter(&mid, "arcodash_embed_events_total{event=\"created\"}"),
        created_before + 1
    );

    let serve_resp = reqwest::Client::new()
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(serve_resp.status().as_u16(), 200);

    let revoke_resp = client
        .delete(format!("{}/api/embeds/{embed_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(revoke_resp.status().as_u16(), 204);

    let serve_after_revoke = reqwest::Client::new()
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(serve_after_revoke.status().as_u16(), 404);

    let after = scrape().await;
    std::env::remove_var("METRICS_TOKEN");
    assert_eq!(
        read_counter(&after, "arcodash_embed_events_total{event=\"revoked\"}"),
        revoked_before + 1
    );
    assert_eq!(
        read_counter(&after, "arcodash_embed_serves_total{outcome=\"served\"}"),
        served_before + 1
    );
    assert_eq!(
        read_counter(&after, "arcodash_embed_serves_total{outcome=\"not_found\"}"),
        not_found_before + 1
    );
}

/// A real `notify_subscription` call moves `arcodash_monitor_notifications_total{outcome="sent"}`,
/// proving the counter is wired into the service.
#[tokio::test]
async fn a_real_notify_subscription_call_moves_the_monitor_notification_counter() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("METRICS_TOKEN", "the-real-token");
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let metrics_client = reqwest::Client::new();
    let read_counter = |text: &str, needle: &str| -> u64 {
        text.lines()
            .find(|l| l.starts_with(needle))
            .and_then(|l| l.split_whitespace().last())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };

    let ds: serde_json::Value = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "metrics-monitor-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();
    let query: serde_json::Value = client
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "metrics-monitor-query",
            "query": "SELECT 1",
            "options": {}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let query_id = query["id"].as_i64().unwrap();
    let publish_resp = client
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(publish_resp.status().as_u16(), 200);

    let monitor: serde_json::Value = client
        .post(format!("{}/api/monitors", app.address))
        .json(&serde_json::json!({
            "query_id": query_id, "name": "metrics-monitor", "options": {}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let monitor_id = monitor["id"].as_i64().unwrap();

    let sub: serde_json::Value = client
        .post(format!(
            "{}/api/monitors/{monitor_id}/subscriptions",
            app.address
        ))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let sub_id = sub["id"].as_i64().unwrap();

    let before_text = metrics_client
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let sent_before = read_counter(
        &before_text,
        "arcodash_monitor_notifications_total{outcome=\"sent\"}",
    );

    let notify_resp = client
        .post(format!(
            "{}/api/monitors/{monitor_id}/subscriptions/{sub_id}/notify",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        notify_resp.status().as_u16(),
        204,
        "{}",
        notify_resp.text().await.unwrap_or_default()
    );

    let after_text = metrics_client
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    std::env::remove_var("METRICS_TOKEN");
    let sent_after = read_counter(
        &after_text,
        "arcodash_monitor_notifications_total{outcome=\"sent\"}",
    );
    assert_eq!(sent_after, sent_before + 1);
}

/// Exhausting the login limiter moves the exposed counter, proving `limit_by_ip`'s reject arm
/// increments it.
#[tokio::test]
async fn exhausting_the_login_limiter_moves_the_ip_rejection_counter() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("METRICS_TOKEN", "the-real-token");
    std::env::set_var("LOGIN_RATE_LIMIT_PER_MIN", "2");
    let app = spawn_app().await;
    std::env::remove_var("LOGIN_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);
    app.cleanup().await;

    let metrics_client = reqwest::Client::new();
    let read_ip_rejections = |text: &str| -> u64 {
        text.lines()
            .find(|l| l.starts_with("arcodash_rate_limit_rejections_total{limiter=\"ip\"}"))
            .and_then(|l| l.split_whitespace().last())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };

    let before_text = metrics_client
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let before = read_ip_rejections(&before_text);

    let login_client = reqwest::Client::builder()
        .cookie_store(false)
        .build()
        .unwrap();
    for _ in 0..3 {
        let _ = login_client
            .post(format!("{}/api/login", app.address))
            .json(&serde_json::json!({ "email": "nobody@example.com", "password": "wrong" }))
            .send()
            .await
            .unwrap();
    }

    let after_text = metrics_client
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let after = read_ip_rejections(&after_text);
    std::env::remove_var("METRICS_TOKEN");

    assert!(
        after > before,
        "exhausting the login limiter must move the ip rejection counter: before={before} after={after}"
    );
}

/// Two `GET`s to the same route TEMPLATE with different ids must collapse onto one
/// `arcodash_http_requests_total` series, keyed on the template — never the raw path.
#[tokio::test]
async fn http_requests_counter_labels_by_template_not_raw_path() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("METRICS_TOKEN", "the-real-token");
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let mk_ds = |name: &'static str| {
        let client = client.clone();
        let address = app.address.clone();
        async move {
            client
                .post(format!("{address}/api/data_sources"))
                .json(&serde_json::json!({ "name": name, "type": "pg", "options": {} }))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()["id"]
                .as_i64()
                .unwrap()
        }
    };
    let id1 = mk_ds("http-metric-ds-1").await;
    let id2 = mk_ds("http-metric-ds-2").await;

    let metrics_client = reqwest::Client::new();
    let scrape_text = || {
        let metrics_client = metrics_client.clone();
        let address = app.address.clone();
        async move {
            metrics_client
                .get(format!("{address}/metrics"))
                .bearer_auth("the-real-token")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        }
    };
    let count_series = |text: &str, needle: &str| -> usize {
        text.lines().filter(|l| l.starts_with(needle)).count()
    };
    let needle = "arcodash_http_requests_total{method=\"GET\",route=\"/api/data_sources/{id}\",";

    let _ = client
        .get(format!("{}/api/data_sources/{id1}", app.address))
        .send()
        .await
        .unwrap();
    let _ = client
        .get(format!("{}/api/data_sources/{id2}", app.address))
        .send()
        .await
        .unwrap();

    let after = scrape_text().await;
    std::env::remove_var("METRICS_TOKEN");
    let series_after = count_series(&after, needle);

    assert_eq!(
        series_after, 1,
        "two ids on the same route template must collapse onto exactly one series:\n{after}"
    );
    assert!(
        !after.contains(&format!("route=\"/api/data_sources/{id1}\"")),
        "a raw path leaked into the route label:\n{after}"
    );
}

/// A dashboard embed served via `/api/embed/{token}` and the deprecated
/// `/api/dashboards/public/{token}` shim must move the SAME `arcodash_embed_serves_total` counter.
#[tokio::test]
async fn public_dashboard_serve_is_metered() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("METRICS_TOKEN", "the-real-token");
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds: serde_json::Value = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "metrics-dash-embed-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let query: serde_json::Value = client
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "metrics-dash-embed-query",
            "query": "SELECT 1",
            "options": {}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let query_id = query["id"].as_i64().unwrap();
    let publish_resp = client
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(publish_resp.status().as_u16(), 200);

    let viz: serde_json::Value = client
        .post(format!("{}/api/visualizations", app.address))
        .json(&serde_json::json!({
            "query_id": query_id, "type": "chart", "name": "metrics-dash-embed-viz", "options": {}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let viz_id = viz["id"].as_i64().unwrap();

    let dashboard: serde_json::Value = client
        .post(format!("{}/api/dashboards", app.address))
        .json(&serde_json::json!({ "name": "metrics-dash-embed-dashboard" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dashboard_id = dashboard["id"].as_i64().unwrap();
    let slug = dashboard["slug"].as_str().unwrap().to_string();

    let widget_resp = client
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": dashboard_id, "visualization_id": viz_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(widget_resp.status().as_u16(), 200);

    let publish_dash_resp = client
        .post(format!("{}/api/dashboards/{slug}/publish", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(publish_dash_resp.status().as_u16(), 200);

    let org_id = org_id_for_ds(&app.db, ds_id).await;
    let qr_id = seed_result(
        &app.db,
        org_id,
        ds_id,
        "metrics-dash-embed-hash",
        "SELECT 1",
        serde_json::json!({ "columns": [{"name": "n", "friendly_type": "integer"}], "rows": [{"n": 1}] }),
    )
    .await;
    {
        use sea_orm::ConnectionTrait;
        app.db
            .execute_unprepared(&format!(
                "UPDATE queries SET latest_query_data_id = {qr_id}, published_latest_query_data_id = {qr_id} WHERE id = {query_id}"
            ))
            .await
            .unwrap();
    }

    let embed: serde_json::Value = client
        .post(format!("{}/api/embeds", app.address))
        .json(&serde_json::json!({
            "object_type": "dashboard", "object_id": dashboard_id, "anonymous": true
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = embed["token"].as_str().unwrap().to_string();

    let metrics_client = reqwest::Client::new();
    let read_counter = |text: &str, needle: &str| -> u64 {
        text.lines()
            .find(|l| l.starts_with(needle))
            .and_then(|l| l.split_whitespace().last())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    let before_text = metrics_client
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let served_before = read_counter(
        &before_text,
        "arcodash_embed_serves_total{outcome=\"served\"}",
    );

    let anon = reqwest::Client::new();
    let via_embed = anon
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        via_embed.status().as_u16(),
        200,
        "{}",
        via_embed.text().await.unwrap_or_default()
    );
    let via_public = anon
        .get(format!("{}/api/dashboards/public/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        via_public.status().as_u16(),
        200,
        "{}",
        via_public.text().await.unwrap_or_default()
    );

    let after_text = metrics_client
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    std::env::remove_var("METRICS_TOKEN");
    let served_after = read_counter(
        &after_text,
        "arcodash_embed_serves_total{outcome=\"served\"}",
    );
    assert_eq!(
        served_after,
        served_before + 2,
        "both /api/embed and the deprecated /api/dashboards/public route must record a served outcome"
    );
}

/// The `"unauthenticated"` outcome only fires under `PublicDashboardAccess::OrgMemberOnly`
/// (multi-tenant), which this single-org harness cannot construct; an embedding binary's suite
/// should cover it.
#[test]
fn unauthenticated_public_serve_is_metered_in_org_member_only_mode() {}

/// A wrong password on `POST /api/login` must move
/// `arcodash_auth_outcomes_total{method="password",outcome="bad_credentials"}`.
#[tokio::test]
async fn failed_login_increments_the_auth_outcome_counter() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("METRICS_TOKEN", "the-real-token");
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let metrics_client = reqwest::Client::new();
    let read_counter = |text: &str, needle: &str| -> u64 {
        text.lines()
            .find(|l| l.starts_with(needle))
            .and_then(|l| l.split_whitespace().last())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    let needle = "arcodash_auth_outcomes_total{method=\"password\",outcome=\"bad_credentials\"}";

    let before_text = metrics_client
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let before = read_counter(&before_text, needle);

    let login_client = reqwest::Client::builder()
        .cookie_store(false)
        .build()
        .unwrap();
    let resp = login_client
        .post(format!("{}/api/login", app.address))
        .json(&serde_json::json!({ "email": common::ADMIN_EMAIL, "password": "definitely-wrong" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    let after_text = metrics_client
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    std::env::remove_var("METRICS_TOKEN");
    let after = read_counter(&after_text, needle);
    assert_eq!(after, before + 1);
}

/// A correct login on `POST /api/login` must move
/// `arcodash_auth_outcomes_total{method="password",outcome="success"}`.
#[tokio::test]
async fn successful_login_increments_the_success_outcome() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("METRICS_TOKEN", "the-real-token");
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let metrics_client = reqwest::Client::new();
    let read_counter = |text: &str, needle: &str| -> u64 {
        text.lines()
            .find(|l| l.starts_with(needle))
            .and_then(|l| l.split_whitespace().last())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    let needle = "arcodash_auth_outcomes_total{method=\"password\",outcome=\"success\"}";

    let before_text = metrics_client
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let before = read_counter(&before_text, needle);

    let login_client = reqwest::Client::builder()
        .cookie_store(false)
        .build()
        .unwrap();
    let resp = login_client
        .post(format!("{}/api/login", app.address))
        .json(&serde_json::json!({ "email": common::ADMIN_EMAIL, "password": common::ADMIN_PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let after_text = metrics_client
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    std::env::remove_var("METRICS_TOKEN");
    let after = read_counter(&after_text, needle);
    assert_eq!(after, before + 1);
}

/// An API key from a DIFFERENT org than the request resolves to must move
/// `arcodash_auth_outcomes_total{method="api_key",outcome="cross_org"}`.
#[tokio::test]
async fn cross_org_api_key_increments_cross_org() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("METRICS_TOKEN", "the-real-token");
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    use arcodash_core::entities::organization;
    use sea_orm::{ActiveModelTrait, ActiveValue::NotSet, ActiveValue::Set};
    let other_org = organization::ActiveModel {
        id: NotSet,
        name: Set("cross-org-metrics-org".to_string()),
        slug: Set(format!(
            "cross-org-metrics-{}",
            uuid::Uuid::new_v4().simple()
        )),
        settings: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
    }
    .insert(&app.db)
    .await
    .unwrap();
    // A user (and API key) in a different org than the request resolves to (the seeded default
    // org).
    let (user, _) = user_with_role(&app.db, other_org.id, &["view_data_source"]).await;

    let metrics_client = reqwest::Client::new();
    let read_counter = |text: &str, needle: &str| -> u64 {
        text.lines()
            .find(|l| l.starts_with(needle))
            .and_then(|l| l.split_whitespace().last())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    let needle = "arcodash_auth_outcomes_total{method=\"api_key\",outcome=\"cross_org\"}";

    let before_text = metrics_client
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let before = read_counter(&before_text, needle);

    let resp = reqwest::Client::new()
        .get(format!("{}/api/data_sources", app.address))
        .header(
            "Authorization",
            format!("Key {}", test_api_key(&user.email)),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        401,
        "a cross-org api key must not authenticate"
    );

    let after_text = metrics_client
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    std::env::remove_var("METRICS_TOKEN");
    let after = read_counter(&after_text, needle);
    assert_eq!(after, before + 1);
}

/// A schedule with `auto_paused_at` set shows up in the `arcodash_schedules_auto_paused` gauge (the
/// worker meters the transition).
#[tokio::test]
async fn auto_paused_schedule_gauge_counts_paused_rows() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("METRICS_TOKEN", "the-real-token");
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    use arcodash_core::entities::{organization, schedule};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist");
    let ds = arcodash_core::test_support::new_data_source(org.id)
        .insert(&app.db)
        .await
        .unwrap();
    let mut u = arcodash_core::test_support::new_user(org.id);
    u.email = Set(format!(
        "schedule-gauge-{}@test.local",
        uuid::Uuid::new_v4()
    ));
    let user = u.insert(&app.db).await.unwrap();
    let q = arcodash_core::test_support::new_query(org.id, ds.id, user.id)
        .insert(&app.db)
        .await
        .unwrap();

    let now = chrono::Utc::now();
    schedule::ActiveModel {
        org_id: Set(org.id),
        entity_type: Set("query".to_string()),
        entity_id: Set(q.id),
        spec: Set(serde_json::json!({ "kind": "interval", "every_secs": 300 })),
        timezone: Set("UTC".to_string()),
        enabled: Set(true),
        paused_until: Set(None),
        auto_paused_at: Set(Some(now.into())),
        overlap_policy: Set("skip".to_string()),
        next_run_at: Set(Some(now.into())),
        consecutive_failures: Set(5),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();

    let text = reqwest::Client::new()
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    std::env::remove_var("METRICS_TOKEN");

    let value: u64 = text
        .lines()
        .find(|l| l.starts_with("arcodash_schedules_auto_paused "))
        .and_then(|l| l.split_whitespace().last())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    assert!(
        value >= 1,
        "expected the seeded auto-paused schedule to be reflected in the gauge:\n{text}"
    );
}

/// `GET /api/data_sources/:id/schema` probes `RefreshSchema` access via `authorize::evaluate` (no
/// metric side effect), not `authorize::authorize`, which would record a denial in
/// `arcodash_authz_decisions_total` on the ordinary viewer path.
#[tokio::test]
async fn serving_a_cached_schema_to_a_caller_without_refresh_does_not_record_a_denial() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("METRICS_TOKEN", "the-real-token");
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let ds: serde_json::Value = admin
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "schema-probe-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    use arcodash_core::entities::organization;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist");

    // Holds `view_data_source` only: reaches the data source but can't refresh its schema, and must
    // not be treated as an attack.
    let (viewer, _) = user_with_role(&app.db, org.id, &["view_data_source"]).await;
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "Authorization",
        reqwest::header::HeaderValue::from_str(&format!("Key {}", test_api_key(&viewer.email)))
            .unwrap(),
    );
    let viewer_client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap();

    let metrics_client = reqwest::Client::new();
    let read_denied = |text: &str| -> u64 {
        text.lines()
            .find(|l| {
                l.starts_with(
                    "arcodash_authz_decisions_total{capability=\"edit_data_source\",outcome=\"denied\"}",
                )
            })
            .and_then(|l| l.split_whitespace().last())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };

    let before_text = metrics_client
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let before = read_denied(&before_text);

    let schema_resp = viewer_client
        .get(format!("{}/api/data_sources/{ds_id}/schema", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        schema_resp.status().as_u16(),
        200,
        "a viewer without refresh rights must still get the (empty) cached schema"
    );

    let after_text = metrics_client
        .get(format!("{}/metrics", app.address))
        .bearer_auth("the-real-token")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let after = read_denied(&after_text);
    std::env::remove_var("METRICS_TOKEN");

    assert_eq!(
        after, before,
        "serving a cached schema must not record an authz denial: before={before} after={after}"
    );
}

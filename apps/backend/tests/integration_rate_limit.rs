//! Integration tests for rate limiting and router wiring: per-IP keying via `TRUSTED_PROXY_HOPS` +
//! `X-Forwarded-For`, shared login budgets, and download routes.
//!
//! The rate-limit env vars are read once at router-build time, so tests overriding them set them
//! before `spawn_app()` and hold `ENV_MUTATION_LOCK`.

mod common;
use common::spawn_app;

/// Serializes tests that mutate rate-limit env vars. A `tokio::sync::Mutex` because guards are held
/// across `.await` points.
static ENV_MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn bare() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(false)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn login_attempt(client: &reqwest::Client, address: &str, xff: Option<&str>) -> u16 {
    let mut req = client
        .post(format!("{address}/api/login"))
        .json(&serde_json::json!({
            "email": "nobody@example.com",
            "password": "wrong-password",
        }));
    if let Some(xff) = xff {
        req = req.header("X-Forwarded-For", xff);
    }
    req.send().await.unwrap().status().as_u16()
}

/// With `TRUSTED_PROXY_HOPS` unset, `X-Forwarded-For` must be ignored: all requests key on the peer
/// address (the spoofing guard).
#[tokio::test]
async fn xff_is_ignored_and_budget_shared_when_trusted_hops_unset() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::remove_var("TRUSTED_PROXY_HOPS");
    std::env::set_var("LOGIN_RATE_LIMIT_PER_MIN", "2");
    let app = spawn_app().await;
    std::env::remove_var("LOGIN_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);

    let client = bare();
    assert_ne!(
        login_attempt(&client, &app.address, Some("203.0.113.10")).await,
        429,
        "attempt 1 must be within quota"
    );
    assert_ne!(
        login_attempt(&client, &app.address, Some("203.0.113.20")).await,
        429,
        "attempt 2 must be within quota"
    );
    assert_eq!(
        login_attempt(&client, &app.address, Some("203.0.113.30")).await,
        429,
        "with hops unset, XFF must be ignored — all three requests share the peer's one budget"
    );
}

/// With `TRUSTED_PROXY_HOPS=1`, requests carrying different `X-Forwarded-For` clients get
/// independent budgets.
#[tokio::test]
async fn distinct_xff_clients_get_independent_budgets_when_trusted_hops_configured() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("LOGIN_RATE_LIMIT_PER_MIN", "2");
    std::env::set_var("TRUSTED_PROXY_HOPS", "1");
    let app = spawn_app().await;
    std::env::remove_var("LOGIN_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);

    let client = bare();
    assert_ne!(
        login_attempt(&client, &app.address, Some("203.0.113.11")).await,
        429
    );
    assert_ne!(
        login_attempt(&client, &app.address, Some("203.0.113.11")).await,
        429
    );
    assert_eq!(
        login_attempt(&client, &app.address, Some("203.0.113.11")).await,
        429,
        "client A's own budget must now be exhausted"
    );

    assert_ne!(
        login_attempt(&client, &app.address, Some("203.0.113.22")).await,
        429,
        "a distinct X-Forwarded-For client must have an independent budget"
    );

    std::env::remove_var("TRUSTED_PROXY_HOPS");
}

/// Exhaust the login limiter on `/api/login`, then confirm `/api/ldap/login` is also limited from
/// the same IP (the routes share one limiter).
#[tokio::test]
async fn login_and_ldap_share_one_budget_from_the_same_ip() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::remove_var("TRUSTED_PROXY_HOPS");
    std::env::set_var("LOGIN_RATE_LIMIT_PER_MIN", "2");
    let app = spawn_app().await;
    std::env::remove_var("LOGIN_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);

    let client = bare();
    assert_ne!(login_attempt(&client, &app.address, None).await, 429);
    assert_ne!(login_attempt(&client, &app.address, None).await, 429);

    let ldap_resp = client
        .post(format!("{}/api/ldap/login", app.address))
        .json(&serde_json::json!({ "username": "nobody", "password": "wrong" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        ldap_resp.status().as_u16(),
        429,
        "login and ldap must share one budget, not independent ones"
    );
}

/// A cached-pointer read (`GET /api/queries/{id}/results`) must never be execute-limited, while the
/// CSV download routes must be.
#[tokio::test]
async fn latest_result_is_unlimited_but_downloads_are_limited() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("QUERY_EXECUTE_RATE_LIMIT_PER_MIN", "2");
    std::env::set_var("RESULT_DOWNLOAD_RATE_LIMIT_PER_MIN", "2");
    let app = spawn_app().await;
    std::env::remove_var("QUERY_EXECUTE_RATE_LIMIT_PER_MIN");
    std::env::remove_var("RESULT_DOWNLOAD_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);
    app.cleanup().await;
    let admin = app.admin_client().await;

    for i in 0..5 {
        let resp = admin
            .get(format!("{}/api/queries/999999/results", app.address))
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            429,
            "GET .../results must not be execute-limited (request {i})"
        );
    }

    let download_csv = || {
        admin
            .get(format!("{}/api/query_results/999999.csv", app.address))
            .send()
    };
    assert_ne!(download_csv().await.unwrap().status().as_u16(), 429);
    assert_ne!(download_csv().await.unwrap().status().as_u16(), 429);
    assert_eq!(
        download_csv().await.unwrap().status().as_u16(),
        429,
        "query_results download route must be limited"
    );

    // ...and /runs/{id}/result.csv shares that exhausted download-tier budget.
    let run_resp = admin
        .get(format!("{}/api/runs/999999/result.csv", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        run_resp.status().as_u16(),
        429,
        "runs/.../result.csv must be limited too"
    );
}

/// Plain `GET /api/query_results/{id}` (the JSON read dashboard widgets fire) must never draw from
/// the download-tier budget, while the `.csv` branch of the same handler is still limited.
#[tokio::test]
async fn plain_json_result_reads_never_429_even_past_the_download_quota() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("RESULT_DOWNLOAD_RATE_LIMIT_PER_MIN", "2");
    let app = spawn_app().await;
    std::env::remove_var("RESULT_DOWNLOAD_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);
    app.cleanup().await;
    let admin = app.admin_client().await;

    for i in 0..25 {
        let resp = admin
            .get(format!("{}/api/query_results/999999", app.address))
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            429,
            "GET /query_results/{{id}} (plain read) must not be download-limited (request {i})"
        );
    }

    let download_csv = || {
        admin
            .get(format!("{}/api/query_results/999999.csv", app.address))
            .send()
    };
    assert_ne!(download_csv().await.unwrap().status().as_u16(), 429);
    assert_ne!(download_csv().await.unwrap().status().as_u16(), 429);
    assert_eq!(
        download_csv().await.unwrap().status().as_u16(),
        429,
        "the download branch must still be limited after its own quota is exhausted"
    );
}

/// `X-Forwarded-For` sent as two separate header lines must not let a client dodge the limiter by
/// rotating the first line: `limit_by_ip` must not read only the first line via `HeaderMap::get`.
#[tokio::test]
async fn duplicate_xff_header_lines_dont_let_an_attacker_rotate_the_rate_limit_key() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("LOGIN_RATE_LIMIT_PER_MIN", "2");
    std::env::set_var("TRUSTED_PROXY_HOPS", "1");
    let app = spawn_app().await;
    std::env::remove_var("LOGIN_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);

    let client = bare();
    let attempt = |rotating_first_line: &str| {
        client
            .post(format!("{}/api/login", app.address))
            // Two header lines: the rotating first is attacker-controlled, the second is what the
            // trusted hop appended.
            .header("X-Forwarded-For", rotating_first_line)
            .header("X-Forwarded-For", "203.0.113.77")
            .json(&serde_json::json!({
                "email": "nobody@example.com",
                "password": "wrong-password",
            }))
            .send()
    };

    assert_ne!(
        attempt("203.0.113.1").await.unwrap().status().as_u16(),
        429,
        "attempt 1 must be within quota"
    );
    assert_ne!(
        attempt("203.0.113.2").await.unwrap().status().as_u16(),
        429,
        "attempt 2 must be within quota"
    );
    assert_eq!(
        attempt("203.0.113.3").await.unwrap().status().as_u16(),
        429,
        "rotating the first XFF line on every request must not grant a fresh budget — the key \
         must stay anchored on the constant (trusted) second line"
    );

    // A genuinely different trusted (second-line) client must have an independent budget; this
    // fails if the key collapses to the peer address.
    let other_trusted = client
        .post(format!("{}/api/login", app.address))
        .header("X-Forwarded-For", "203.0.113.9")
        .header("X-Forwarded-For", "203.0.113.88")
        .json(&serde_json::json!({
            "email": "nobody@example.com",
            "password": "wrong-password",
        }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_ne!(
        other_trusted, 429,
        "a distinct trusted second-line client must hold its own, unexhausted budget"
    );

    std::env::remove_var("TRUSTED_PROXY_HOPS");
}

/// `POST /api/users` sends a real invitation email per call, so it must be rate-limited;
/// re-inviting the same email resends, so the same payload exhausts quota.
#[tokio::test]
async fn invite_route_is_rate_limited() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("USER_INVITE_RATE_LIMIT_PER_MIN", "2");
    let app = spawn_app().await;
    std::env::remove_var("USER_INVITE_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let invite = || {
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({
                "name": "Invitee",
                "email": "invitee@example.com",
            }))
            .send()
    };

    assert_ne!(invite().await.unwrap().status().as_u16(), 429);
    assert_ne!(invite().await.unwrap().status().as_u16(), 429);
    assert_eq!(
        invite().await.unwrap().status().as_u16(),
        429,
        "POST /api/users must be rate-limited past its per-user quota"
    );
}

/// Manually running a monitor enqueues real work, so it must be rate-limited per user; the limiter
/// runs ahead of the handler, so it trips even for a nonexistent monitor id.
#[tokio::test]
async fn monitor_run_route_is_rate_limited() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("MONITOR_RUN_RATE_LIMIT_PER_MIN", "2");
    let app = spawn_app().await;
    std::env::remove_var("MONITOR_RUN_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let run = || {
        admin
            .post(format!("{}/api/monitors/1/run", app.address))
            .send()
    };

    assert_ne!(run().await.unwrap().status().as_u16(), 429);
    assert_ne!(run().await.unwrap().status().as_u16(), 429);
    assert_eq!(
        run().await.unwrap().status().as_u16(),
        429,
        "POST /api/monitors/{{id}}/run must be rate-limited past its per-user quota"
    );
}

/// "Send now" enqueues a real outbound notification per call, so it must be rate-limited; like the
/// run route, it trips ahead of the handler.
#[tokio::test]
async fn monitor_notify_is_rate_limited_per_user() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("MONITOR_NOTIFY_RATE_LIMIT_PER_MIN", "1");
    let app = spawn_app().await;
    std::env::remove_var("MONITOR_NOTIFY_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let notify = || {
        admin
            .post(format!(
                "{}/api/monitors/1/subscriptions/1/notify",
                app.address
            ))
            .send()
    };

    assert_ne!(notify().await.unwrap().status().as_u16(), 429);
    assert_eq!(
        notify().await.unwrap().status().as_u16(),
        429,
        "POST .../notify must be rate-limited past its per-user quota"
    );
}

/// The monitor run and notify routes must draw from independent budgets: each is its own
/// `route_layer`-scoped sub-router.
#[tokio::test]
async fn monitor_run_quota_is_not_consumed_by_notify() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("MONITOR_NOTIFY_RATE_LIMIT_PER_MIN", "1");
    std::env::set_var("MONITOR_RUN_RATE_LIMIT_PER_MIN", "60");
    let app = spawn_app().await;
    std::env::remove_var("MONITOR_NOTIFY_RATE_LIMIT_PER_MIN");
    std::env::remove_var("MONITOR_RUN_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let notify = admin
        .post(format!(
            "{}/api/monitors/1/subscriptions/1/notify",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_ne!(notify.status().as_u16(), 429);
    let notify_again = admin
        .post(format!(
            "{}/api/monitors/1/subscriptions/1/notify",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        notify_again.status().as_u16(),
        429,
        "notify budget must now be exhausted"
    );

    let run = admin
        .post(format!("{}/api/monitors/1/run", app.address))
        .send()
        .await
        .unwrap();
    assert_ne!(
        run.status().as_u16(),
        429,
        "the run route's budget must be independent of the exhausted notify budget"
    );
}

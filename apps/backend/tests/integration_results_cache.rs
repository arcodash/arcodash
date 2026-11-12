//! Integration tests: query result fetch/caching, latest-result, download formats and
//! parameter_values. Results are seeded directly into `query_results` (no in-process worker): the
//! cache-hit path keys off `(org_id, data_source_id, query_hash)`, while the latest-result path
//! reads the query's `latest_query_data_id` pointer. Skips without `TEST_DATABASE_URL`.

mod common;
use common::{authed_client, spawn_app};
use common::{default_org_id, ok_json};

use sea_orm::ActiveModelTrait;
use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::EntityTrait;

use arcodash_core::entities::{query, query_result};
use arcodash_core::hash::gen_query_hash;

/// Create a `pg` data source via the admin client, returning its id.
async fn create_ds(client: &reqwest::Client, address: &str, name: &str) -> i64 {
    let ds = ok_json(
        client
            .post(format!("{address}/api/data_sources"))
            .json(&serde_json::json!({ "name": name, "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    ds["id"].as_i64().unwrap()
}

/// Create a (published) query, returning `(query_id, query_hash)`; the hash matches
/// `query_service::create` for a parameter-free query, so a seeded row with it is a cache hit.
async fn create_query(
    client: &reqwest::Client,
    address: &str,
    ds_id: i64,
    name: &str,
    sql: &str,
) -> (i64, String) {
    let q = ok_json(
        client
            .post(format!("{address}/api/queries"))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": name,
                "query": sql,
                "options": {},
                "is_draft": false
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let id = q["id"].as_i64().unwrap();
    let hash = q["query_hash"].as_str().unwrap().to_string();
    assert_eq!(
        hash,
        gen_query_hash(sql, &serde_json::Value::Null),
        "query_hash from API must equal gen_query_hash(sql, Null) for a param-free query"
    );
    (id, hash)
}

/// Seed a `query_results` row directly (no worker) and return its id. `data` is the
/// `{columns, rows}` jsonb the format/extract paths read.
async fn seed_result(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    data_source_id: i64,
    query_hash: &str,
    query: &str,
    data: serde_json::Value,
) -> i64 {
    let now = chrono::Utc::now().into();
    let model = query_result::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        data_source_id: Set(data_source_id),
        query_hash: Set(query_hash.to_string()),
        query: Set(query.to_string()),
        data: Set(Some(data)),
        data_location: Set(None),
        data_size_bytes: Set(None),
        runtime: Set(Some(0.01)),
        retrieved_at: Set(now),
        last_used_at: Set(now),
        created_at: Set(now),
        updated_at: Set(now),
    };
    query_result::Entity::insert(model)
        .exec(db)
        .await
        .expect("seed query_result failed")
        .last_insert_id
}

fn sample_data() -> serde_json::Value {
    serde_json::json!({
        "columns": [
            { "name": "id",    "friendly_name": "ID"    },
            { "name": "label", "friendly_name": "Label" }
        ],
        "rows": [
            { "id": 1, "label": "alpha" },
            { "id": 2, "label": "beta"  }
        ]
    })
}

/// A `truncated: true` result with exactly `row_count` rows, as `execute_query` writes on hitting
/// `MAX_QUERY_ROWS`.
fn truncated_data(row_count: usize) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = (0..row_count)
        .map(|i| serde_json::json!({ "id": i }))
        .collect();
    serde_json::json!({
        "columns": [{ "name": "id", "friendly_name": "ID" }],
        "rows": rows,
        "truncated": true
    })
}

/// Invite a non-admin user, accept, and log in; returns `(user_id, authed_client)`.
async fn invite_and_login(
    app: &common::TestApp,
    admin: &reqwest::Client,
    email: &str,
    name: &str,
    password: &str,
    group_ids: Vec<i64>,
) -> (i64, reqwest::Client) {
    let invite = ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({ "name": name, "email": email, "groups": group_ids }))
            .send()
            .await
            .unwrap(),
        "invite user",
    )
    .await;
    let user_id = invite["id"].as_i64().expect("invite must return id");

    let token = {
        use arcodash_core::entities::user;
        let u = user::Entity::find_by_id(user_id)
            .one(&app.db)
            .await
            .expect("db query failed")
            .expect("invited user must exist");
        u.details["invitation_token"]
            .as_str()
            .expect("invitation_token must be in details")
            .to_string()
    };

    let accept_client = reqwest::Client::builder()
        .cookie_store(false)
        .build()
        .unwrap();
    let sess = accept_client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = common::extract_set_cookie(sess.headers(), "csrf_token").unwrap();
    let accept = accept_client
        .post(format!("{}/api/users/invitation/accept", app.address))
        .header("Cookie", format!("csrf_token={csrf}"))
        .header("X-CSRF-Token", &csrf)
        .json(&serde_json::json!({ "token": token, "name": name, "password": password }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        accept.status().as_u16(),
        200,
        "accept invitation: {}",
        accept.text().await.unwrap_or_default()
    );

    let (session, csrf_login) = app.login(email, password).await;
    let client = authed_client(&app.address, &session, &csrf_login);
    (user_id, client)
}

/// GET /api/queries/:id/results (latest) with no result row → 404 "no results yet".
#[tokio::test]
async fn latest_result_404_when_none() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address, "latest-none-ds").await;
    let (query_id, _hash) =
        create_query(&client, &app.address, ds_id, "No Results", "SELECT 1").await;

    let resp = client
        .get(format!("{}/api/queries/{query_id}/results", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "latest result with no cached row must be 404"
    );
    let body = resp.text().await.unwrap_or_default();
    assert!(
        body.contains("no results yet"),
        "404 body should mention 'no results yet'; got: {body}"
    );
}

/// GET /api/queries/:id/results (latest) returns the seeded result once one exists.
#[tokio::test]
async fn latest_result_returns_seeded_row() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "latest-ds").await;
    let sql = "SELECT 1";
    let (query_id, hash) = create_query(&client, &app.address, ds_id, "Has Result", sql).await;
    let result_id = seed_result(&app.db, org_id, ds_id, &hash, sql, sample_data()).await;

    let q = query::Entity::find_by_id(query_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: query::ActiveModel = q.into();
    active.latest_query_data_id = Set(Some(result_id));
    active.update(&app.db).await.unwrap();

    let body = ok_json(
        client
            .get(format!("{}/api/queries/{query_id}/results", app.address))
            .send()
            .await
            .unwrap(),
        "latest result",
    )
    .await;
    assert_eq!(
        body["query_result"]["id"].as_i64(),
        Some(result_id),
        "latest must return the seeded result; got: {body}"
    );
}

/// POST /api/queries/:id/results with a fresh cached row + a non-zero max_age → returns
/// `{query_result}` (cache hit), NOT `{job}`.
#[tokio::test]
async fn execute_returns_cached_when_fresh() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "cache-hit-ds").await;
    let sql = "SELECT 1";
    let (query_id, hash) = create_query(&client, &app.address, ds_id, "Cache Hit", sql).await;
    let result_id = seed_result(&app.db, org_id, ds_id, &hash, sql, sample_data()).await;

    let body = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/results", app.address))
            .json(&serde_json::json!({ "parameters": {}, "max_age": 86400 }))
            .send()
            .await
            .unwrap(),
        "execute (expect cache hit)",
    )
    .await;
    assert!(
        body.get("query_result").is_some(),
        "fresh cache must return query_result, not job; got: {body}"
    );
    assert!(
        body.get("job").is_none(),
        "fresh cache must NOT enqueue a job; got: {body}"
    );
    assert_eq!(
        body["query_result"]["id"].as_i64(),
        Some(result_id),
        "cache hit must return the seeded result id; got: {body}"
    );
}

/// max_age=0 forces a fresh run (`{job}`) even with a cached row, so the cache check is conditional
/// on max_age.
#[tokio::test]
async fn execute_enqueues_when_max_age_zero() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "force-run-ds").await;
    let sql = "SELECT 1";
    let (query_id, hash) = create_query(&client, &app.address, ds_id, "Force Run", sql).await;
    let _ = seed_result(&app.db, org_id, ds_id, &hash, sql, sample_data()).await;

    let body = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/results", app.address))
            .json(&serde_json::json!({ "parameters": {}, "max_age": 0 }))
            .send()
            .await
            .unwrap(),
        "execute (expect enqueue)",
    )
    .await;
    assert!(
        body.get("job").is_some(),
        "max_age=0 must enqueue a job even with a cached row; got: {body}"
    );
    assert!(
        !body["job"]["id"].as_str().unwrap_or("").is_empty(),
        "enqueued job must have an id; got: {body}"
    );
}

/// A `truncated: true` cached row whose row count no longer matches the current `MAX_QUERY_ROWS`
/// must NOT be served as a cache hit even within `max_age`: it reflects a row cap that has since
/// changed.
#[tokio::test]
async fn execute_enqueues_when_truncated_row_count_does_not_match_current_cap() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "stale-cap-ds").await;
    let sql = "SELECT 1";
    let (query_id, hash) = create_query(&client, &app.address, ds_id, "Stale Cap", sql).await;
    let _ = seed_result(&app.db, org_id, ds_id, &hash, sql, truncated_data(5_000)).await;

    let body = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/results", app.address))
            .json(&serde_json::json!({ "parameters": {}, "max_age": 86400 }))
            .send()
            .await
            .unwrap(),
        "execute (expect enqueue despite fresh max_age)",
    )
    .await;
    assert!(
        body.get("job").is_some(),
        "a truncated row whose count no longer matches MAX_QUERY_ROWS must be treated as stale \
         and enqueue a fresh run; got: {body}"
    );
    assert!(
        body.get("query_result").is_none(),
        "a row-cap-stale truncated row must not be served as a cache hit; got: {body}"
    );
}

/// A truncated row whose count DOES match the current `MAX_QUERY_ROWS` is a legitimate cache hit;
/// only rows from a changed cap are rejected.
#[tokio::test]
async fn execute_returns_cached_when_truncated_row_count_matches_current_cap() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "matching-cap-ds").await;
    let sql = "SELECT 1";
    let (query_id, hash) = create_query(&client, &app.address, ds_id, "Matching Cap", sql).await;
    let result_id = seed_result(&app.db, org_id, ds_id, &hash, sql, truncated_data(10_000)).await;

    let body = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/results", app.address))
            .json(&serde_json::json!({ "parameters": {}, "max_age": 86400 }))
            .send()
            .await
            .unwrap(),
        "execute (expect cache hit)",
    )
    .await;
    assert_eq!(
        body["query_result"]["id"].as_i64(),
        Some(result_id),
        "truncated row matching the current cap must still be a cache hit; got: {body}"
    );
    assert!(body.get("job").is_none(), "must not enqueue; got: {body}");
}

/// GET /api/query_results/:id for a result whose data source the user can't access → 404 even
/// though the row exists.
#[tokio::test]
async fn get_result_404_for_inaccessible_data_source() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&admin, &app.address, "private-result-ds").await;
    let sql = "SELECT secret";
    let (_query_id, hash) = create_query(&admin, &app.address, ds_id, "Private", sql).await;
    let result_id = seed_result(&app.db, org_id, ds_id, &hash, sql, sample_data()).await;

    let admin_get = admin
        .get(format!("{}/api/query_results/{result_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        admin_get.status().as_u16(),
        200,
        "admin must be able to fetch the result"
    );

    // Non-admin with no DS access must get 404 (not 403 — no enumeration).
    let (_uid, user) = invite_and_login(
        &app,
        &admin,
        "noresult@test.local",
        "No Result",
        "P@ssw0rd!",
        vec![],
    )
    .await;
    let resp = user
        .get(format!("{}/api/query_results/{result_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "user without DS access must get 404 for the result"
    );
}

/// GET /api/query_results/:id.csv / .tsv / .json → 200 with the right Content-Type and an
/// attachment Content-Disposition.
#[tokio::test]
async fn download_csv_tsv_json_have_correct_headers() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "download-ds").await;
    let sql = "SELECT 1";
    let (_query_id, hash) = create_query(&client, &app.address, ds_id, "Download", sql).await;
    let result_id = seed_result(&app.db, org_id, ds_id, &hash, sql, sample_data()).await;

    for (ext, expected_ct) in [
        ("csv", "text/csv"),
        ("tsv", "text/tab-separated-values"),
        ("json", "application/json"),
    ] {
        let resp = client
            .get(format!(
                "{}/api/query_results/{result_id}.{ext}",
                app.address
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "download .{ext} must be 200");

        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            ct.starts_with(expected_ct),
            "download .{ext} Content-Type should be {expected_ct}; got: {ct}"
        );

        let cd = resp
            .headers()
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            cd.contains("attachment") && cd.contains(&format!("query_result_{result_id}.{ext}")),
            "download .{ext} Content-Disposition should be an attachment with the filename; got: {cd}"
        );

        let body = resp.text().await.unwrap_or_default();
        if ext == "csv" {
            assert!(
                body.contains("ID,Label"),
                "csv body missing header; got: {body}"
            );
        } else if ext == "tsv" {
            assert!(
                body.contains("ID\tLabel"),
                "tsv body missing header; got: {body}"
            );
        }
    }
}

/// GET /api/query_results/:id.xlsx → 200 with the xlsx Content-Type and zip magic bytes.
#[tokio::test]
async fn download_xlsx_has_correct_headers() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "xlsx-ds").await;
    let sql = "SELECT 1";
    let (_query_id, hash) = create_query(&client, &app.address, ds_id, "Xlsx", sql).await;
    let result_id = seed_result(&app.db, org_id, ds_id, &hash, sql, sample_data()).await;

    let resp = client
        .get(format!(
            "{}/api/query_results/{result_id}.xlsx",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "download .xlsx must be 200");
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.starts_with("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        "xlsx Content-Type wrong; got: {ct}"
    );
    let bytes = resp.bytes().await.unwrap();
    assert!(
        bytes.len() > 4 && &bytes[0..2] == b"PK",
        "xlsx must start with PK zip magic"
    );
}

/// A string cell starting with a formula character is escaped with a leading `'` in CSV/TSV
/// (rendered as text, not evaluated) but left alone in XLSX.
#[tokio::test]
async fn download_escapes_formula_injection_in_csv_tsv_but_not_xlsx() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "formula-ds").await;
    let sql = "SELECT 1";
    let (_query_id, hash) = create_query(&client, &app.address, ds_id, "Formula", sql).await;
    let data = serde_json::json!({
        "columns": [{ "name": "cell", "friendly_name": "Cell" }],
        "rows": [{ "cell": "=cmd|'/c calc'!A1" }]
    });
    let result_id = seed_result(&app.db, org_id, ds_id, &hash, sql, data).await;

    let csv = client
        .get(format!("{}/api/query_results/{result_id}.csv", app.address))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        csv.contains("'=cmd|'/c calc'!A1") || csv.contains("\"'=cmd"),
        "csv must escape the leading formula character with a leading quote; got: {csv}"
    );
    assert!(
        !csv.contains("\n=cmd"),
        "the raw unescaped formula must not appear: {csv}"
    );

    let xlsx = client
        .get(format!(
            "{}/api/query_results/{result_id}.xlsx",
            app.address
        ))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(
        xlsx.len() > 4 && &xlsx[0..2] == b"PK",
        "xlsx must still be a valid zip regardless of cell content"
    );
}

/// GET /api/query_results/:id.bogus → 400 unsupported format; non-numeric id → 400.
#[tokio::test]
async fn download_bad_format_and_bad_id_are_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "badfmt-ds").await;
    let sql = "SELECT 1";
    let (_query_id, hash) = create_query(&client, &app.address, ds_id, "BadFmt", sql).await;
    let result_id = seed_result(&app.db, org_id, ds_id, &hash, sql, sample_data()).await;

    let bogus = client
        .get(format!(
            "{}/api/query_results/{result_id}.bogus",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        bogus.status().as_u16(),
        400,
        "unsupported format must be 400"
    );

    let bad_id_fmt = client
        .get(format!("{}/api/query_results/abc.csv", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        bad_id_fmt.status().as_u16(),
        400,
        "non-numeric id (with format) must be 400"
    );

    let bad_id_plain = client
        .get(format!("{}/api/query_results/notanumber", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        bad_id_plain.status().as_u16(),
        400,
        "non-numeric id (plain) must be 400"
    );
}

/// GET /api/queries/:id/parameter_values?name=... for a query-based dropdown:
/// returns `{values:[...]}` when the source query has a cached result.
#[tokio::test]
async fn parameter_values_returns_values_when_source_cached() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "paramvals-ds").await;

    let source_sql = "SELECT name, value FROM opts";
    let (source_id, source_hash) =
        create_query(&client, &app.address, ds_id, "Options Source", source_sql).await;
    seed_result(
        &app.db,
        org_id,
        ds_id,
        &source_hash,
        source_sql,
        serde_json::json!({
            "columns": [{ "name": "name" }, { "name": "value" }],
            "rows": [
                { "name": "Alpha", "value": "a" },
                { "name": "Beta",  "value": "b" }
            ]
        }),
    )
    .await;

    // Note: the wire field is `query_id` (no serde rename), not `queryId`.
    let consumer = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Consumer",
                "query": "SELECT * FROM t WHERE k = {{ pick }}",
                "is_draft": false,
                "options": {
                    "parameters": [
                        {
                            "name": "pick",
                            "title": "Pick",
                            "type": "query",
                            "query_id": source_id
                        }
                    ]
                }
            }))
            .send()
            .await
            .unwrap(),
        "create consumer query",
    )
    .await;
    let consumer_id = consumer["id"].as_i64().unwrap();

    let body = ok_json(
        client
            .get(format!(
                "{}/api/queries/{consumer_id}/parameter_values?name=pick",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "parameter_values (cached source)",
    )
    .await;
    let values = body["values"]
        .as_array()
        .unwrap_or_else(|| panic!("expected values array; got: {body}"));
    assert_eq!(values.len(), 2, "expected 2 options; got: {body}");
    assert_eq!(values[0]["name"], "Alpha");
    assert_eq!(values[0]["value"], "a");
}

/// GET /api/queries/:id/parameter_values?name=... returns `{job:...}` when the source query has
/// never been run (no cached result yet).
#[tokio::test]
async fn parameter_values_returns_job_when_source_not_run() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address, "paramvals-job-ds").await;

    let (source_id, _hash) = create_query(
        &client,
        &app.address,
        ds_id,
        "Unrun Source",
        "SELECT name, value FROM opts",
    )
    .await;

    let consumer = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Consumer No Cache",
                "query": "SELECT * FROM t WHERE k = {{ pick }}",
                "is_draft": false,
                "options": {
                    "parameters": [
                        {
                            "name": "pick",
                            "title": "Pick",
                            "type": "query",
                            "query_id": source_id
                        }
                    ]
                }
            }))
            .send()
            .await
            .unwrap(),
        "create consumer query (no cache)",
    )
    .await;
    let consumer_id = consumer["id"].as_i64().unwrap();

    let body = ok_json(
        client
            .get(format!(
                "{}/api/queries/{consumer_id}/parameter_values?name=pick",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "parameter_values (unrun source)",
    )
    .await;
    assert!(
        body.get("job").is_some(),
        "unrun source must enqueue a job; got: {body}"
    );
    assert!(
        !body["job"]["id"].as_str().unwrap_or("").is_empty(),
        "enqueued job must have an id; got: {body}"
    );
}

/// Re-running a published query whose first publish had no result, via the ad-hoc/promoted path
/// (`POST /api/query_results` with `query_id`), must repoint the PUBLISHED pointer too; otherwise
/// `GET .../results?published_only=true` (dashboard widgets) 404s forever. Exercises the cache-hit
/// branch (`sync_cached_hit_state`).
#[tokio::test]
async fn adhoc_execute_after_publish_repoints_published_pointer() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let ds_id = create_ds(&client, &app.address, "publish-repoint-ds").await;
    let sql = "SELECT 1";
    let (query_id, hash) = create_query(&client, &app.address, ds_id, "Publish Repoint", sql).await;

    let q = query::Entity::find_by_id(query_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let publish = client
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": q.working_version }))
        .send()
        .await
        .unwrap();
    assert_eq!(publish.status().as_u16(), 200, "publish must succeed");

    let pre = client
        .get(format!(
            "{}/api/queries/{query_id}/results?published_only=true",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(pre.status().as_u16(), 404, "no published result yet");

    // Re-run via the ad-hoc/promoted path with content identical to the published draft.
    let result_id = seed_result(&app.db, org_id, ds_id, &hash, sql, sample_data()).await;
    let exec = ok_json(
        client
            .post(format!("{}/api/query_results", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "query": sql,
                "parameters": {},
                "max_age": 86400,
                "query_id": query_id
            }))
            .send()
            .await
            .unwrap(),
        "ad-hoc execute against published query",
    )
    .await;
    assert_eq!(
        exec["query_result"]["id"].as_i64(),
        Some(result_id),
        "must be a cache hit against the seeded row; got: {exec}"
    );

    let post = ok_json(
        client
            .get(format!(
                "{}/api/queries/{query_id}/results?published_only=true",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "published_only result after ad-hoc re-execute",
    )
    .await;
    assert_eq!(
        post["query_result"]["id"].as_i64(),
        Some(result_id),
        "the ad-hoc/promoted run must repoint published_latest_query_data_id; got: {post}"
    );
}

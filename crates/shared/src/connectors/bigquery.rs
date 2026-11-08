//! Google `BigQuery` connector: service-account JSON key to `OAuth2` token via a signed JWT, then
//! REST calls to `jobs.query`/`jobs.getQueryResults`, all over `reqwest`.
//!
//! The vendor hosts and `OAuth2` token endpoint are fixed constants (never read from the key file).
//! The user-supplied `api_endpoint` override is SSRF-guarded in `resolve_token`, the choke point
//! all trait methods route through. With `api_endpoint` set (emulator mode) the SA-JWT exchange is
//! skipped.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::http_connectors::{guard_url, pinned_http_client, validate_identifier};
use super::{
    ColumnMeta, ColumnSchema, NetworkTarget, QueryExecutionResult, QueryRunner, TableSchema,
};

const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
/// Fixed vendor host, never attacker-controllable: needs a connect timeout but no DNS pinning (see
/// [`resolve_token`]).
const BIGQUERY_API_HOST: &str = "bigquery.googleapis.com";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const BQ_SCOPE: &str = "https://www.googleapis.com/auth/bigquery";
const DEFAULT_BASE_URL: &str = "https://bigquery.googleapis.com";
/// Poll interval while a `jobs.query` call hasn't completed within its `timeoutMs`.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BigQueryOptions {
    pub project_id: String,
    /// The full service-account JSON key contents (not a path). Required unless `api_endpoint` is
    /// set.
    #[serde(default)]
    pub service_account_json: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    /// Default dataset for schema browsing; if unset, `get_schema` enumerates every dataset in the
    /// project.
    #[serde(default)]
    pub dataset: Option<String>,
    /// Base URL override for a local `BigQuery` emulator. Requests go to this host and the SA-JWT
    /// exchange is skipped. Never set this for real `BigQuery`.
    #[serde(default)]
    pub api_endpoint: Option<String>,
    /// Per-query billing cap; `BigQuery` fails a query that would bill more, before running it.
    #[serde(default)]
    pub maximum_bytes_billed: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
struct ServiceAccountKey {
    client_email: String,
    private_key: String,
}

fn parse_options(opts: &Value) -> Result<BigQueryOptions> {
    super::parse_opts(opts, "invalid BigQuery connection options")
}

fn parse_service_account(opts: &BigQueryOptions) -> Result<ServiceAccountKey> {
    let json = opts.service_account_json.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "BigQuery service_account_json is required unless api_endpoint (emulator mode) is set"
        )
    })?;
    serde_json::from_str(json).context(
        "invalid BigQuery service_account_json (expected the full service-account key JSON \
         document, not a file path)",
    )
}

/// Base `BigQuery` REST host: `opts.api_endpoint` if set (emulator), else the real API.
fn base_url(opts: &BigQueryOptions) -> &str {
    opts.api_endpoint.as_deref().unwrap_or(DEFAULT_BASE_URL)
}

/// Resolve the bearer token, or `None` in emulator mode. Also returns the HTTP client for all
/// requests against `opts`: pinned to the approved addresses for a user-supplied `api_endpoint`,
/// with a connect timeout either way. The single choke point for all trait methods, so the SSRF
/// egress guard (`guard_host`) on a user-supplied `api_endpoint` lives here; the fixed vendor host
/// needs none.
async fn resolve_token(opts: &BigQueryOptions) -> Result<(Option<String>, reqwest::Client)> {
    if let Some(endpoint) = &opts.api_endpoint {
        let (host, addrs) = guard_url(endpoint).await?;
        return Ok((None, pinned_http_client(&host, &addrs)?));
    }
    let key = parse_service_account(opts)?;
    let token = get_access_token(&key)
        .await
        .context("BigQuery authentication failed")?;
    Ok((Some(token), pinned_http_client(BIGQUERY_API_HOST, &[])?))
}

#[derive(Serialize)]
struct GoogleClaims {
    iss: String,
    scope: String,
    aud: String,
    iat: u64,
    exp: u64,
}

/// Build and sign the RS256 JWT bearer assertion for `key`. Pure/offline.
fn build_assertion(key: &ServiceAccountKey) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let claims = GoogleClaims {
        iss: key.client_email.clone(),
        scope: BQ_SCOPE.to_string(),
        aud: TOKEN_URL.to_string(),
        iat: now,
        exp: now + 3600,
    };
    let encoding_key = EncodingKey::from_rsa_pem(key.private_key.as_bytes()).context(
        "invalid BigQuery service account private_key (expected a PEM-encoded RSA private key)",
    )?;
    encode(&Header::new(Algorithm::RS256), &claims, &encoding_key)
        .context("failed to sign BigQuery service-account JWT")
}

/// Pull a human-readable failure reason out of a token-exchange response, never the whole body (it
/// may carry internal fields that must not reach the UI).
fn token_error_detail(resp: &Value) -> &str {
    resp.pointer("/error_description")
        .or_else(|| resp.pointer("/error"))
        .and_then(Value::as_str)
        .unwrap_or("no access_token in the response")
}

/// Exchange a signed JWT for an `OAuth2` access token, always at the fixed vendor `TOKEN_URL`.
async fn exchange_token(assertion: &str) -> Result<String> {
    let req = pinned_http_client("oauth2.googleapis.com", &[])?
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion),
        ]);
    let resp = send_json(
        req,
        None,
        "BigQuery OAuth2 token exchange request failed",
        "BigQuery OAuth2 token exchange returned invalid JSON",
    )
    .await?;

    resp["access_token"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| {
            let detail = token_error_detail(&resp);
            anyhow::anyhow!("BigQuery OAuth2 token exchange failed: {detail}")
        })
}

async fn get_access_token(key: &ServiceAccountKey) -> Result<String> {
    // RS256 signing is CPU-bound; run it off the async runtime.
    let key_for_signing = key.clone();
    let assertion = tokio::task::spawn_blocking(move || build_assertion(&key_for_signing))
        .await
        .context("BigQuery JWT signing thread panicked")??;
    exchange_token(&assertion).await
}

/// Sends `req` (bearer-authed when `token` is set) under [`HTTP_TIMEOUT`] and decodes the JSON
/// body; `send_ctx`/`json_ctx` label the two failure points.
async fn send_json(
    req: reqwest::RequestBuilder,
    token: Option<&str>,
    send_ctx: &'static str,
    json_ctx: &'static str,
) -> Result<Value> {
    let req = match token {
        Some(t) => req.bearer_auth(t),
        None => req,
    };
    req.timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(super::http_connectors::classify_reqwest_err)
        .context(send_ctx)?
        .json()
        .await
        .context(json_ctx)
}

/// Map `BigQuery` REST `field.type` strings to `ColumnMeta::friendly_type` tags.
fn friendly_type(bq_type: &str) -> &'static str {
    match bq_type {
        "INTEGER" | "INT64" => "integer",
        "FLOAT" | "FLOAT64" | "NUMERIC" | "BIGNUMERIC" => "float",
        "BOOLEAN" | "BOOL" => "boolean",
        "TIMESTAMP" | "DATETIME" => "datetime",
        "DATE" => "date",
        _ => "string",
    }
}

/// Decode one `BigQuery` REST cell (a string, or omitted for `NULL`) to JSON per its field type.
fn cell_to_json(bq_type: &str, raw: Option<&str>) -> Value {
    let Some(raw) = raw else {
        return Value::Null;
    };
    match bq_type {
        "INTEGER" | "INT64" => super::parse_or_raw::<i64>(raw),
        "FLOAT" | "FLOAT64" => super::parse_or_raw::<f64>(raw),
        "BOOLEAN" | "BOOL" => super::parse_or_raw::<bool>(raw),
        // NUMERIC/BIGNUMERIC/TIMESTAMP/DATE etc. stay strings: lossless for decimals and sortable.
        _ => json!(raw),
    }
}

/// Parse one `jobs.query`/`jobs.getQueryResults` response into columns and that page's rows. Pure.
fn parse_query_response(resp: &Value) -> Result<(Vec<ColumnMeta>, Vec<Value>)> {
    if let Some(err) = resp.pointer("/error/message").and_then(Value::as_str) {
        anyhow::bail!("BigQuery error: {err}");
    }

    let field_defs: Vec<(String, String)> = resp
        .pointer("/schema/fields")
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .map(|f| {
                    (
                        f["name"].as_str().unwrap_or("").to_string(),
                        f["type"].as_str().unwrap_or("STRING").to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    let columns = field_defs
        .iter()
        .map(|(name, ty)| ColumnMeta {
            name: name.clone(),
            friendly_type: friendly_type(ty).to_string(),
        })
        .collect();

    let rows = resp["rows"]
        .as_array()
        .map(|raw_rows| {
            raw_rows
                .iter()
                .map(|row| {
                    let cells = field_defs.iter().enumerate().map(|(i, (name, ty))| {
                        (name.clone(), cell_to_json(ty, row["f"][i]["v"].as_str()))
                    });
                    Value::Object(cells.collect())
                })
                .collect()
        })
        .unwrap_or_default();

    Ok((columns, rows))
}

/// Extract an error message from a response that failed before `jobComplete` (auth/permission/quota
/// errors), so the real cause is reported, not a generic missing-`jobReference`.
fn bigquery_error_message(resp: &Value) -> Option<String> {
    resp.pointer("/error/message")
        .or_else(|| resp.pointer("/error/errors/0/message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Page-count cap on `getQueryResults` pagination, so a misbehaving server can't hold the loop
/// forever.
const BIGQUERY_MAX_PAGES: usize = 2000;

/// Row-count cap independent of [`BIGQUERY_MAX_PAGES`], bounding memory like the other connectors'
/// row caps.
const BIGQUERY_MAX_ROWS: usize = 200_000;

/// Checked after every page; a pure chokepoint for [`BIGQUERY_MAX_ROWS`].
fn check_row_cap(accumulated_rows: usize) -> Result<()> {
    if accumulated_rows > BIGQUERY_MAX_ROWS {
        anyhow::bail!(
            "BigQuery result exceeds this connector's row cap ({BIGQUERY_MAX_ROWS} rows); \
             narrow the query or add an explicit LIMIT"
        );
    }
    Ok(())
}

/// `jobs.query` request body; `maximumBytesBilled` is an int64 and travels as a string.
fn query_body(opts: &BigQueryOptions, sql: &str) -> Value {
    let mut body = json!({
        "query": sql,
        "useLegacySql": false,
        "timeoutMs": 30_000,
    });
    if let Some(location) = &opts.location {
        body["location"] = json!(location);
    }
    if let Some(max) = opts.maximum_bytes_billed {
        body["maximumBytesBilled"] = json!(max.to_string());
    }
    body
}

fn views_sql(project_id: &str, dataset: &str) -> String {
    format!(
        "SELECT table_name FROM `{project_id}`.{dataset}.INFORMATION_SCHEMA.TABLES \
         WHERE table_type IN ('VIEW', 'MATERIALIZED VIEW') ORDER BY table_name"
    )
}

/// `jobs.getQueryResults` for `job_id`, optionally at `page_token`. `location` is REQUIRED for
/// single-region jobs (e.g. asia-northeast1 otherwise 404s).
async fn get_query_results(
    client: &reqwest::Client,
    opts: &BigQueryOptions,
    token: Option<&str>,
    job_id: &str,
    page_token: Option<&str>,
    send_ctx: &'static str,
    json_ctx: &'static str,
) -> Result<Value> {
    let url = format!(
        "{}/bigquery/v2/projects/{}/queries/{job_id}",
        base_url(opts),
        opts.project_id
    );
    let mut req = client.get(&url);
    if let Some(tok) = page_token {
        req = req.query(&[("pageToken", tok)]);
    }
    if let Some(loc) = &opts.location {
        req = req.query(&[("location", loc.as_str())]);
    }
    send_json(req, token, send_ctx, json_ctx).await
}

/// Run one SQL statement via `jobs.query`, polling `jobs.getQueryResults` if needed and paginating
/// through all pages.
async fn run_bigquery_sql(
    client: &reqwest::Client,
    opts: &BigQueryOptions,
    token: Option<&str>,
    sql: &str,
) -> Result<(Vec<ColumnMeta>, Vec<Value>)> {
    let query_url = format!(
        "{}/bigquery/v2/projects/{}/queries",
        base_url(opts),
        opts.project_id
    );
    let req = client.post(&query_url).json(&query_body(opts, sql));
    let mut resp = send_json(
        req,
        token,
        "BigQuery jobs.query request failed",
        "BigQuery jobs.query returned invalid JSON",
    )
    .await?;

    if let Some(msg) = bigquery_error_message(&resp) {
        anyhow::bail!("BigQuery error: {msg}");
    }

    let job_id = resp
        .pointer("/jobReference/jobId")
        .and_then(Value::as_str)
        .map(str::to_string);

    let poll_deadline = Instant::now() + Duration::from_secs(super::query_timeout_secs());
    while resp["jobComplete"].as_bool() != Some(true) {
        if let Some(msg) = bigquery_error_message(&resp) {
            anyhow::bail!("BigQuery error: {msg}");
        }
        let job_id = job_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("BigQuery jobs.query did not return a jobReference"))?;
        if Instant::now() >= poll_deadline {
            anyhow::bail!(
                "BigQuery job {job_id} did not complete within {}s",
                super::query_timeout_secs()
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
        resp = get_query_results(
            client,
            opts,
            token,
            job_id,
            None,
            "BigQuery getQueryResults poll failed",
            "BigQuery getQueryResults returned invalid JSON",
        )
        .await?;
    }

    let (columns, mut rows) = parse_query_response(&resp)?;
    check_row_cap(rows.len())?;

    let mut page_token = resp["pageToken"].as_str().map(str::to_string);
    let mut pages_fetched = 0;
    while let (Some(tok), Some(job_id)) = (page_token, job_id.as_deref()) {
        pages_fetched += 1;
        if pages_fetched > BIGQUERY_MAX_PAGES {
            anyhow::bail!(
                "BigQuery result has more pages than this connector will follow ({BIGQUERY_MAX_PAGES})"
            );
        }
        let page = get_query_results(
            client,
            opts,
            token,
            job_id,
            Some(&tok),
            "BigQuery getQueryResults pagination failed",
            "BigQuery getQueryResults page returned invalid JSON",
        )
        .await?;
        rows.extend(parse_query_response(&page)?.1);
        check_row_cap(rows.len())?;
        page_token = page["pageToken"].as_str().map(str::to_string);
    }

    Ok((columns, rows))
}

/// Enumerate every dataset in `opts.project_id` via `datasets.list` (paginated).
async fn list_datasets(
    client: &reqwest::Client,
    opts: &BigQueryOptions,
    token: Option<&str>,
) -> Result<Vec<String>> {
    let url = format!(
        "{}/bigquery/v2/projects/{}/datasets",
        base_url(opts),
        opts.project_id
    );
    let mut ids = Vec::new();
    let mut page_token: Option<String> = None;
    let mut pages_fetched = 0;
    let deadline = Instant::now() + Duration::from_secs(super::query_timeout_secs());
    loop {
        if pages_fetched > BIGQUERY_MAX_PAGES {
            anyhow::bail!(
                "BigQuery dataset list has more pages than this connector will follow \
                 ({BIGQUERY_MAX_PAGES})"
            );
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "BigQuery datasets.list did not finish paginating within {}s",
                super::query_timeout_secs()
            );
        }
        let mut req = client.get(&url);
        if let Some(t) = &page_token {
            req = req.query(&[("pageToken", t.as_str())]);
        }
        let resp = send_json(
            req,
            token,
            "BigQuery datasets.list request failed",
            "BigQuery datasets.list returned invalid JSON",
        )
        .await?;
        if let Some(err) = resp.pointer("/error/message").and_then(Value::as_str) {
            anyhow::bail!("BigQuery error: {err}");
        }
        ids.extend(
            resp["datasets"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|ds| ds.pointer("/datasetReference/datasetId")?.as_str())
                .map(str::to_string),
        );
        pages_fetched += 1;
        let next_token = resp["nextPageToken"].as_str().map(str::to_string);
        if next_token.is_none() {
            break;
        }
        // A misbehaving endpoint echoing the same page token would loop until the page cap; catch
        // it on the next page.
        if next_token == page_token {
            anyhow::bail!("BigQuery datasets.list returned the same nextPageToken twice in a row");
        }
        page_token = next_token;
    }
    Ok(ids)
}

/// The datasets to introspect: the configured `dataset` alone, else every dataset in the project.
/// Validates both identifiers, since they are interpolated into `INFORMATION_SCHEMA` SQL.
async fn introspection_datasets(
    client: &reqwest::Client,
    opts: &BigQueryOptions,
    token: Option<&str>,
) -> Result<Vec<String>> {
    let datasets = if let Some(ds) = &opts.dataset {
        validate_identifier(ds, "dataset")?;
        vec![ds.clone()]
    } else {
        list_datasets(client, opts, token).await?
    };
    validate_identifier(&opts.project_id, "project_id")?;
    Ok(datasets)
}

pub struct BigQueryConnector;

#[async_trait]
impl QueryRunner for BigQueryConnector {
    fn type_name(&self) -> &'static str {
        "bigquery"
    }

    fn configuration_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "title": "Project ID" },
                "service_account_json": {
                    "type": "string",
                    "title": "Service Account JSON Key",
                    "x-widget": "textarea"
                },
                "location": { "type": "string", "title": "Location (optional, e.g. US, EU)" },
                "dataset": {
                    "type": "string",
                    "title": "Default Dataset (optional — limits schema browsing to one dataset)"
                },
                "api_endpoint": {
                    "type": "string",
                    "title": "API Endpoint override (advanced — local emulator testing only)"
                }
            },
            "required": ["project_id", "service_account_json"],
            "secret": ["service_account_json"]
        })
    }

    fn network_targets(&self, opts: &Value) -> Vec<NetworkTarget> {
        parse_options(opts)
            .ok()
            .and_then(|opts| opts.api_endpoint)
            .map(|endpoint| vec![NetworkTarget::Url(endpoint)])
            .unwrap_or_default()
    }

    async fn test_connection(&self, opts: &Value) -> Result<()> {
        let opts = parse_options(opts)?;
        let (token, client) = resolve_token(&opts).await?;
        run_bigquery_sql(&client, &opts, token.as_deref(), "SELECT 1")
            .await
            .map(|_| ())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let opts = parse_options(opts)?;
        let (token, client) = resolve_token(&opts).await?;
        let datasets = introspection_datasets(&client, &opts, token.as_deref()).await?;

        let mut tables: Vec<TableSchema> = Vec::new();
        for dataset in datasets {
            // Project quoted alone (handles hyphenated project IDs); the `project.dataset` form is
            // the one the goccy emulator's analyzer supports.
            let sql = format!(
                "SELECT table_name, column_name, data_type FROM `{}`.{}.INFORMATION_SCHEMA.COLUMNS \
                 ORDER BY table_name, ordinal_position",
                opts.project_id, dataset
            );
            let (_, rows) = run_bigquery_sql(&client, &opts, token.as_deref(), &sql)
                .await
                .with_context(|| format!("failed to introspect BigQuery dataset {dataset}"))?;
            for row in rows {
                let field = |key: &str| row[key].as_str().unwrap_or_default().to_string();
                let column = ColumnSchema::typed_or_blank(field("column_name"), field("data_type"));
                let full_name = format!("{dataset}.{}", field("table_name"));
                super::push_schema_column(&mut tables, full_name, column);
            }
        }
        Ok(tables)
    }

    async fn get_view_names(&self, opts: &Value) -> Result<Vec<String>> {
        let opts = parse_options(opts)?;
        let (token, client) = resolve_token(&opts).await?;
        let datasets = introspection_datasets(&client, &opts, token.as_deref()).await?;

        let mut names = Vec::new();
        for dataset in datasets {
            let sql = views_sql(&opts.project_id, &dataset);
            let (_, rows) = run_bigquery_sql(&client, &opts, token.as_deref(), &sql)
                .await
                .with_context(|| format!("failed to introspect BigQuery views in {dataset}"))?;
            names.extend(
                rows.iter()
                    .filter_map(|r| r["table_name"].as_str())
                    .map(|t| format!("{dataset}.{t}")),
            );
        }
        Ok(names)
    }

    async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
        let opts = parse_options(opts)?;
        let start = Instant::now();
        let (token, client) = resolve_token(&opts).await?;
        let (columns, rows) = run_bigquery_sql(&client, &opts, token.as_deref(), query).await?;
        Ok(QueryExecutionResult {
            columns,
            rows,
            runtime_seconds: start.elapsed().as_secs_f64(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare_opts(max: Option<i64>) -> BigQueryOptions {
        serde_json::from_value(json!({"project_id": "p", "maximum_bytes_billed": max})).unwrap()
    }

    #[test]
    fn query_body_sets_bytes_cap_only_when_configured() {
        let capped = query_body(&bare_opts(Some(104_857_600)), "SELECT 1");
        assert_eq!(capped["maximumBytesBilled"], json!("104857600"));
        let plain = query_body(&bare_opts(None), "SELECT 1");
        assert!(plain.get("maximumBytesBilled").is_none());
    }

    #[test]
    fn views_sql_scopes_to_dataset_and_view_types() {
        let sql = views_sql("my-proj", "ds");
        assert!(sql.contains("`my-proj`.ds.INFORMATION_SCHEMA.TABLES"));
        assert!(sql.contains("'VIEW', 'MATERIALIZED VIEW'"));
    }
    use jsonwebtoken::{decode, Algorithm as JwtAlgorithm, DecodingKey, Validation};
    use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
    use rsa::{RsaPrivateKey, RsaPublicKey};

    fn test_service_account() -> (ServiceAccountKey, RsaPublicKey) {
        let mut rng = rand::thread_rng();
        let private_key = RsaPrivateKey::new(&mut rng, 2048).expect("generate test RSA key");
        let public_key = RsaPublicKey::from(&private_key);
        let pem = private_key
            .to_pkcs8_pem(LineEnding::LF)
            .expect("encode test private key");
        let key = ServiceAccountKey {
            client_email: "svc@test-project.iam.gserviceaccount.com".to_string(),
            private_key: pem.to_string(),
        };
        (key, public_key)
    }

    #[derive(Debug, Deserialize)]
    struct DecodedClaims {
        iss: String,
        scope: String,
        aud: String,
    }

    /// Verifies `jwt` against `public_key` with `aud` pinned to [`TOKEN_URL`].
    fn decode_claims(
        jwt: &str,
        public_key: &RsaPublicKey,
    ) -> jsonwebtoken::errors::Result<DecodedClaims> {
        let public_pem = public_key
            .to_public_key_pem(LineEnding::LF)
            .expect("encode test public key");
        let decoding_key =
            DecodingKey::from_rsa_pem(public_pem.as_bytes()).expect("decode public key");
        let mut validation = Validation::new(JwtAlgorithm::RS256);
        validation.set_audience(&[TOKEN_URL]);
        validation.validate_exp = true;
        decode::<DecodedClaims>(jwt, &decoding_key, &validation).map(|d| d.claims)
    }

    #[test]
    fn build_assertion_signs_a_verifiable_rs256_jwt_with_expected_claims() {
        let (key, public_key) = test_service_account();
        let jwt = build_assertion(&key).expect("build_assertion failed");

        let claims = decode_claims(&jwt, &public_key)
            .expect("signature must verify against the derived public key");
        assert_eq!(claims.iss, key.client_email);
        assert_eq!(claims.scope, BQ_SCOPE);
        assert_eq!(claims.aud, TOKEN_URL);
    }

    /// Guards the `token_uri` SSRF: `ServiceAccountKey` has no `token_uri` field, so a key file's
    /// attacker-controlled value can't influence the JWT `aud` or the exchange target.
    #[test]
    fn build_assertion_ignores_a_malicious_token_uri_in_the_key_file() {
        let (key, public_key) = test_service_account();
        let raw = serde_json::json!({
            "client_email": key.client_email,
            "private_key": key.private_key,
            "token_uri": "https://attacker.example/steal-tokens",
        });
        let parsed: ServiceAccountKey =
            serde_json::from_value(raw).expect("token_uri must be an ignored unknown field");
        let jwt = build_assertion(&parsed).expect("build_assertion failed");

        let claims = decode_claims(&jwt, &public_key)
            .expect("aud must be the fixed vendor TOKEN_URL, not the key file's token_uri");
        assert_eq!(claims.aud, TOKEN_URL);
    }

    #[test]
    fn token_error_detail_never_falls_back_to_the_whole_response_body() {
        let resp = serde_json::json!({
            "secret_field": "internal-data",
            "other": "context",
        });
        let detail = token_error_detail(&resp);
        assert!(!detail.contains("secret_field"));
        assert!(!detail.contains("internal-data"));
    }

    #[test]
    fn token_error_detail_prefers_error_description_then_error() {
        assert_eq!(
            token_error_detail(&serde_json::json!({
                "error_description": "invalid_grant: bad JWT",
                "error": "invalid_grant"
            })),
            "invalid_grant: bad JWT"
        );
        assert_eq!(
            token_error_detail(&serde_json::json!({ "error": "invalid_grant" })),
            "invalid_grant"
        );
        assert_eq!(
            token_error_detail(&serde_json::json!({})),
            "no access_token in the response"
        );
    }

    #[test]
    fn friendly_type_mapping() {
        assert_eq!(friendly_type("INT64"), "integer");
        assert_eq!(friendly_type("INTEGER"), "integer");
        assert_eq!(friendly_type("FLOAT64"), "float");
        assert_eq!(friendly_type("NUMERIC"), "float");
        assert_eq!(friendly_type("BOOL"), "boolean");
        assert_eq!(friendly_type("TIMESTAMP"), "datetime");
        assert_eq!(friendly_type("DATE"), "date");
        assert_eq!(friendly_type("STRING"), "string");
        assert_eq!(friendly_type("BYTES"), "string");
    }

    #[test]
    fn cell_to_json_parses_by_declared_type() {
        assert_eq!(cell_to_json("INT64", Some("42")), json!(42));
        assert_eq!(cell_to_json("FLOAT64", Some("3.5")), json!(3.5));
        assert_eq!(cell_to_json("BOOL", Some("true")), json!(true));
        assert_eq!(cell_to_json("STRING", Some("hi")), json!("hi"));
        assert_eq!(cell_to_json("NUMERIC", Some("12.3400")), json!("12.3400"));
        assert_eq!(cell_to_json("INT64", None), Value::Null);
        assert_eq!(
            cell_to_json("INT64", Some("not-a-number")),
            json!("not-a-number")
        );
    }

    #[test]
    fn parse_query_response_reads_columns_and_rows() {
        let resp = json!({
            "jobComplete": true,
            "schema": {
                "fields": [
                    { "name": "id", "type": "INTEGER" },
                    { "name": "name", "type": "STRING" },
                    { "name": "active", "type": "BOOLEAN" }
                ]
            },
            "rows": [
                { "f": [ { "v": "1" }, { "v": "a" }, { "v": "true" } ] },
                { "f": [ { "v": "2" }, { "v": "b" }, { "v": "false" } ] }
            ]
        });
        let (columns, rows) = parse_query_response(&resp).expect("parse failed");
        assert_eq!(columns.len(), 3);
        assert_eq!(columns[0].friendly_type, "integer");
        assert_eq!(columns[2].friendly_type, "boolean");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["id"], json!(1));
        assert_eq!(rows[0]["name"], json!("a"));
        assert_eq!(rows[1]["active"], json!(false));
    }

    #[test]
    fn parse_query_response_surfaces_api_errors() {
        let resp = json!({ "error": { "message": "Access Denied: ..." } });
        let err = parse_query_response(&resp).unwrap_err();
        assert!(err.to_string().contains("Access Denied"));
    }

    #[test]
    fn bigquery_error_message_reads_error_message() {
        let resp = json!({ "error": { "message": "Access Denied: quota exceeded" } });
        assert_eq!(
            bigquery_error_message(&resp).as_deref(),
            Some("Access Denied: quota exceeded")
        );
    }

    #[test]
    fn bigquery_error_message_falls_back_to_errors_array() {
        let resp = json!({ "error": { "errors": [{ "message": "Not found: Job" }] } });
        assert_eq!(
            bigquery_error_message(&resp).as_deref(),
            Some("Not found: Job")
        );
    }

    #[test]
    fn bigquery_error_message_is_none_for_an_in_progress_job() {
        let resp = json!({ "jobComplete": false, "jobReference": { "jobId": "job1" } });
        assert_eq!(bigquery_error_message(&resp), None);
    }

    /// Emulator mode must skip the SA-JWT exchange. A public IP literal needs no DNS lookup, so
    /// this runs without `ARCODASH_ALLOW_PRIVATE_CONNECTORS`.
    fn emulator_opts(api_endpoint: &str) -> BigQueryOptions {
        serde_json::from_value(json!({"project_id": "test", "api_endpoint": api_endpoint})).unwrap()
    }

    #[tokio::test]
    async fn resolve_token_in_emulator_mode_returns_no_bearer_token() {
        let opts = emulator_opts("http://8.8.8.8:9050");
        let (token, _client) = resolve_token(&opts)
            .await
            .expect("emulator mode must not require a service account");
        assert!(
            token.is_none(),
            "emulator mode must not perform the SA-JWT exchange"
        );
    }

    #[test]
    fn check_row_cap_rejects_past_the_limit_and_accepts_at_it() {
        assert!(check_row_cap(BIGQUERY_MAX_ROWS).is_ok());
        let err = check_row_cap(BIGQUERY_MAX_ROWS + 1).unwrap_err();
        assert!(err.to_string().contains(&BIGQUERY_MAX_ROWS.to_string()));
    }

    #[test]
    fn configuration_schema_shape() {
        assert_eq!(BigQueryConnector.type_name(), "bigquery");
        let schema = BigQueryConnector.configuration_schema();
        assert_eq!(
            schema["required"],
            json!(["project_id", "service_account_json"])
        );
        assert_eq!(schema["secret"], json!(["service_account_json"]));
        assert!(schema["properties"]["dataset"].is_object());
    }

    /// SSRF guard: a loopback/internal `api_endpoint` must be rejected before any request unless
    /// the opt-out env is set.
    #[tokio::test]
    async fn api_endpoint_at_loopback_is_rejected_by_ssrf_guard() {
        let _env = crate::test_support::hold_private_net_env();
        if crate::config::parse_bool_env(crate::net_guard::ALLOW_PRIVATE_ENV, false)
            .unwrap_or(false)
        {
            return;
        }
        let opts = emulator_opts("http://127.0.0.1:9050");
        let err = resolve_token(&opts).await.unwrap_err();
        assert!(
            err.to_string().contains("blocked"),
            "expected SSRF-guard rejection, got: {err}"
        );
    }
}

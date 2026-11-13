//! Snowflake connector via the SQL REST API v2 (`/api/v2/statements`) with key-pair JWT auth. Plain
//! username/password is not supported by that API, so `password` is accepted but `private_key` is
//! required.
//!
//! The host is built from `account` (guarded via `network_targets`; `run_statement` validates its
//! character set before interpolating).

use std::fmt::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use rsa::pkcs8::EncodePublicKey;
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{
    ColumnMeta, ColumnSchema, NetworkTarget, QueryExecutionResult, QueryRunner, TableSchema,
};

const HTTP_TIMEOUT: Duration = Duration::from_secs(65);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Snowflake caps key-pair JWT lifetime at 1 hour; stay comfortably under it.
const JWT_LIFETIME_SECS: u64 = 55 * 60;
/// Hard ceiling on rows accumulated across a multi-partition result, so a huge result set isn't
/// fetched into memory in full.
const SNOWFLAKE_MAX_ROWS: usize = 50_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnowflakeOptions {
    /// Account identifier, e.g. `xy12345` or `xy12345.us-east-1`; used verbatim as the `{account}`
    /// URL segment.
    pub account: String,
    pub user: String,
    /// PEM-encoded RSA private key (PKCS#1 or PKCS#8) whose public key is registered on the
    /// Snowflake user. Required.
    #[serde(default)]
    pub private_key: Option<String>,
    /// Accepted for config-schema parity; not used to authenticate (a password-only config yields a
    /// clear error).
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub warehouse: Option<String>,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
}

fn parse_options(opts: &Value) -> Result<SnowflakeOptions> {
    super::parse_opts(opts, "invalid Snowflake connection options")
}

fn clean_pem_string(pem: &str) -> String {
    pem.replace("\\n", "\n").trim().to_string()
}

fn parse_rsa_private_key(pem: &str) -> Result<RsaPrivateKey> {
    use rsa::pkcs1::DecodeRsaPrivateKey;
    use rsa::pkcs8::DecodePrivateKey;

    RsaPrivateKey::from_pkcs8_pem(pem)
        .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem))
        .context(
            "invalid Snowflake private_key (expected a PEM-encoded PKCS#8 or PKCS#1 RSA \
             private key)",
        )
}

/// SHA256 fingerprint of the DER-encoded (SPKI) public key matching `private_key_pem`, in the
/// `SHA256:<base64>` form Snowflake uses in the JWT `iss` claim.
fn public_key_fingerprint(private_key: &RsaPrivateKey) -> Result<String> {
    let public_key = RsaPublicKey::from(private_key);
    let der = public_key
        .to_public_key_der()
        .context("failed to DER-encode Snowflake public key for fingerprinting")?;
    let hash = Sha256::digest(der.as_bytes());
    Ok(format!("SHA256:{}", BASE64_STANDARD.encode(hash)))
}

#[derive(Serialize)]
struct SnowflakeClaims {
    iss: String,
    sub: String,
    iat: u64,
    exp: u64,
}

/// Build the key-pair auth JWT for `account`/`user`, signed with `private_key_pem`. Pure/offline.
fn build_jwt(account: &str, user: &str, private_key_pem: &str) -> Result<String> {
    let clean_pem = clean_pem_string(private_key_pem);
    let private_key = parse_rsa_private_key(&clean_pem)?;
    let fingerprint = public_key_fingerprint(&private_key)?;

    // `iss`/`sub` use the account LOCATOR only (before the first `.`), uppercased; a
    // region-qualified form yields a 401. The request hostname keeps the full account.
    let locator = account.split('.').next().unwrap_or(account).to_uppercase();
    let qualified_user = format!("{}.{}", locator, user.to_uppercase());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let claims = SnowflakeClaims {
        iss: format!("{qualified_user}.{fingerprint}"),
        sub: qualified_user,
        iat: now,
        exp: now + JWT_LIFETIME_SECS,
    };
    let encoding_key = EncodingKey::from_rsa_pem(clean_pem.as_bytes())
        .context("failed to load Snowflake private_key for signing")?;
    encode(&Header::new(Algorithm::RS256), &claims, &encoding_key)
        .context("failed to sign Snowflake key-pair JWT")
}

/// Friendly type mapping: Snowflake statements-API `rowType[].type` (+ `scale` for `fixed`) →
/// this crate's coarse `ColumnMeta::friendly_type` tags.
fn friendly_type(sf_type: &str, scale: i64) -> &'static str {
    match sf_type {
        "fixed" if scale > 0 => "float",
        "fixed" => "integer",
        "real" => "float",
        "boolean" => "boolean",
        "timestamp_ntz" | "timestamp_ltz" | "timestamp_tz" => "datetime",
        "date" => "date",
        _ => "string",
    }
}

/// Decode one statements-API cell value to JSON per its declared column type.
///
/// NOTE: DATE/TIMESTAMP wire formats (epoch-days / epoch-seconds-with-fraction) follow the
/// documented `jsonv2` format; every branch falls back to the raw string on parse failure.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "epoch seconds and the nanosecond fraction fit; out-of-range values fail from_timestamp and fall back to raw"
)]
fn cell_to_json(sf_type: &str, scale: i64, raw: Option<&str>) -> Value {
    let Some(raw) = raw else {
        return Value::Null;
    };
    match sf_type {
        "fixed" if scale == 0 => super::parse_or_raw::<i64>(raw),
        "real" => super::parse_or_raw::<f64>(raw),
        "boolean" => match raw.to_ascii_lowercase().as_str() {
            "true" | "1" => json!(true),
            "false" | "0" => json!(false),
            _ => json!(raw),
        },
        "timestamp_ntz" | "timestamp_ltz" | "timestamp_tz" => raw
            .parse::<f64>()
            .ok()
            .and_then(|epoch_secs| {
                chrono::DateTime::from_timestamp(
                    epoch_secs.trunc() as i64,
                    ((epoch_secs.fract()) * 1e9).round() as u32,
                )
            })
            .map_or_else(|| json!(raw), |dt| json!(dt.to_rfc3339())),
        "date" => raw
            .parse::<i64>()
            .ok()
            .and_then(|days| chrono::DateTime::from_timestamp(days * 86_400, 0))
            .map_or_else(|| json!(raw), |dt| json!(dt.date_naive().to_string())),
        // Non-zero-scale fixed (and anything unrecognized) kept as a string; for fixed this is
        // lossless precision, like the MySQL/MSSQL DECIMAL handling.
        _ => json!(raw),
    }
}

/// One `rowType` entry: (column name, Snowflake type, scale).
type FieldDef = (String, String, i64);

/// One `rowType` entry (name, type, scale) from the statements-API `resultSetMetaData`.
fn field_defs_from_meta(meta: &Value) -> Vec<FieldDef> {
    meta["rowType"]
        .as_array()
        .map(|fields| {
            fields
                .iter()
                .map(|f| {
                    (
                        f["name"].as_str().unwrap_or("").to_string(),
                        f["type"].as_str().unwrap_or("text").to_string(),
                        f["scale"].as_i64().unwrap_or(0),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn columns_from_field_defs(field_defs: &[FieldDef]) -> Vec<ColumnMeta> {
    field_defs
        .iter()
        .map(|(name, ty, scale)| ColumnMeta {
            name: name.clone(),
            friendly_type: friendly_type(ty, *scale).to_string(),
        })
        .collect()
}

/// Pure chokepoint for [`SNOWFLAKE_MAX_ROWS`], checked after each page/partition is folded in.
fn check_row_cap(accumulated_rows: usize) -> Result<()> {
    if accumulated_rows > SNOWFLAKE_MAX_ROWS {
        anyhow::bail!(
            "Snowflake result exceeds this connector's row cap ({SNOWFLAKE_MAX_ROWS} rows); \
             narrow the query or add an explicit LIMIT"
        );
    }
    Ok(())
}

/// Decode a statements-API `data` array using `field_defs` (the page-0 defs, since later
/// `?partition=N` pages don't repeat `resultSetMetaData`).
fn rows_from_data(data: &Value, field_defs: &[FieldDef]) -> Vec<Value> {
    data.as_array()
        .map(|raw_rows| {
            raw_rows
                .iter()
                .map(|row| {
                    let cells = field_defs.iter().enumerate().map(|(i, (name, ty, scale))| {
                        (name.clone(), cell_to_json(ty, *scale, row[i].as_str()))
                    });
                    Value::Object(cells.collect())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the first page of a statements-API response into field defs, columns and rows (the defs
/// are reused for later `?partition=N` pages).
fn parse_statement_response(resp: &Value) -> Result<(Vec<FieldDef>, Vec<ColumnMeta>, Vec<Value>)> {
    if let Some(msg) = resp["message"].as_str() {
        if resp["data"].is_null() && resp["resultSetMetaData"].is_null() {
            anyhow::bail!("Snowflake error: {msg}");
        }
    }

    let field_defs = field_defs_from_meta(&resp["resultSetMetaData"]);
    let columns = columns_from_field_defs(&field_defs);
    let rows = rows_from_data(&resp["data"], &field_defs);
    Ok((field_defs, columns, rows))
}

/// Run one SQL statement to completion against the statements API: submit, poll while
/// Snowflake is still executing (HTTP 202), then page through every result partition.
async fn run_statement(
    opts: &SnowflakeOptions,
    sql: &str,
) -> Result<(Vec<ColumnMeta>, Vec<Value>)> {
    let private_key_pem = opts.private_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "Snowflake connector requires key-pair authentication (private_key) — the SQL REST \
             API v2 doesn't accept a plain username/password. Generate an RSA key pair and \
             register the public key with ALTER USER ... SET RSA_PUBLIC_KEY = '...'."
        )
    })?;
    // `account` becomes a hostname segment below — reject anything that could break out of it.
    super::http_connectors::validate_identifier(&opts.account, "Snowflake account")?;
    // RS256 signing is CPU-bound; run it off the async runtime.
    let account = opts.account.clone();
    let user = opts.user.clone();
    let private_key_pem = private_key_pem.to_string();
    let jwt = tokio::task::spawn_blocking(move || build_jwt(&account, &user, &private_key_pem))
        .await
        .context("Snowflake JWT signing thread panicked")??;

    let base = format!(
        "https://{}.snowflakecomputing.com/api/v2/statements",
        opts.account
    );
    // No pinning (unlike BigQuery): the vendor host is covered by `network_targets`.
    // `pinned_http_client` with no addresses still adds the connect timeout and the User-Agent
    // header Snowflake requires.
    let client = super::http_connectors::pinned_http_client(
        &format!("{}.snowflakecomputing.com", opts.account),
        &[],
    )?;

    let mut body = json!({
        "statement": sql,
        "timeout": 60,
    });
    for (key, value) in [
        ("warehouse", &opts.warehouse),
        ("database", &opts.database),
        ("schema", &opts.schema),
        ("role", &opts.role),
    ] {
        if let Some(v) = value {
            body[key] = json!(v);
        }
    }
    let authed = |req: reqwest::RequestBuilder| {
        req.bearer_auth(&jwt)
            .header("X-Snowflake-Authorization-Token-Type", "KEYPAIR_JWT")
            .header("Accept", "application/json")
            .timeout(HTTP_TIMEOUT)
    };

    let submit = authed(client.post(&base).json(&body))
        .send()
        .await
        .map_err(super::http_connectors::classify_reqwest_err)
        .context("Snowflake statement submission failed")?;

    let mut status = submit.status();
    let mut resp: Value = submit
        .json()
        .await
        .context("Snowflake statement response was not valid JSON")?;

    let handle = resp["statementHandle"].as_str().map(str::to_string);

    // Each poll has its own `HTTP_TIMEOUT`; this bounds the poll count, since the synchronous
    // `test_connection`/`get_schema` path has no outer timeout.
    let poll_deadline = Instant::now() + Duration::from_secs(super::query_timeout_secs());
    while status.as_u16() == 202 {
        let handle = handle.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "Snowflake accepted the statement asynchronously but returned no statementHandle"
            )
        })?;
        if Instant::now() >= poll_deadline {
            anyhow::bail!(
                "Snowflake statement {handle} did not complete within {}s; \
                 cancel it Snowflake-side if it's still running",
                super::query_timeout_secs()
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
        let poll = authed(client.get(format!("{base}/{handle}")))
            .send()
            .await
            .map_err(super::http_connectors::classify_reqwest_err)
            .context("Snowflake statement status poll failed")?;
        status = poll.status();
        resp = poll
            .json()
            .await
            .context("Snowflake statement status response was not valid JSON")?;
    }

    if !status.is_success() {
        let msg = resp["message"].as_str().unwrap_or("unknown error");
        let sql_state = resp["sqlState"].as_str().unwrap_or("");
        anyhow::bail!("Snowflake query failed ({sql_state}): {msg}");
    }

    let (field_defs, columns, mut rows) = parse_statement_response(&resp)?;
    check_row_cap(rows.len())?;

    let partition_count = resp
        .pointer("/resultSetMetaData/partitionInfo")
        .and_then(Value::as_array)
        .map_or(1, Vec::len);
    if let Some(handle) = handle.as_deref() {
        for partition in 1..partition_count {
            let page: Value = authed(client.get(format!("{base}/{handle}")))
                .query(&[("partition", partition.to_string())])
                .send()
                .await
                .map_err(super::http_connectors::classify_reqwest_err)
                .context("Snowflake result partition fetch failed")?
                .json()
                .await
                .context("Snowflake result partition response was not valid JSON")?;
            rows.extend(rows_from_data(&page["data"], &field_defs));
            check_row_cap(rows.len())?;
        }
    }

    Ok((columns, rows))
}

/// SQL for `get_schema`: base tables and views only, across non-INFORMATION_SCHEMA schemas or
/// scoped to `schema`. `db`/`schema` must already be `validate_identifier`-checked since the
/// statements API has no parameterized queries.
fn build_schema_sql(db: &str, schema: Option<&str>) -> String {
    let mut sql = format!(
        "SELECT c.TABLE_SCHEMA, c.TABLE_NAME, c.COLUMN_NAME, c.DATA_TYPE \
         FROM {db}.INFORMATION_SCHEMA.COLUMNS c \
         JOIN {db}.INFORMATION_SCHEMA.TABLES t \
         ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME \
         WHERE UPPER(c.TABLE_SCHEMA) != 'INFORMATION_SCHEMA' \
         AND t.TABLE_TYPE IN ('BASE TABLE', 'VIEW')"
    );
    if let Some(schema) = schema {
        // Snowflake folds unquoted identifiers to uppercase; compare case-insensitively.
        let _ = write!(sql, " AND UPPER(c.TABLE_SCHEMA) = UPPER('{schema}')");
    }
    sql.push_str(" ORDER BY c.TABLE_SCHEMA, c.TABLE_NAME, c.ORDINAL_POSITION");
    sql
}

/// SQL for `get_view_names`: views only, same schema scope as `build_schema_sql`.
fn build_view_names_sql(db: &str, schema: Option<&str>) -> String {
    let mut sql = format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME FROM {db}.INFORMATION_SCHEMA.TABLES \
         WHERE UPPER(TABLE_SCHEMA) != 'INFORMATION_SCHEMA' AND TABLE_TYPE = 'VIEW'"
    );
    if let Some(schema) = schema {
        let _ = write!(sql, " AND UPPER(TABLE_SCHEMA) = UPPER('{schema}')");
    }
    sql.push_str(" ORDER BY TABLE_SCHEMA, TABLE_NAME");
    sql
}

/// Table name as `get_schema` spells it: bare inside the default schema, else `schema.table`.
fn display_name(schema: &str, table: &str, default_schema: &str) -> String {
    if schema.eq_ignore_ascii_case(default_schema) {
        table.to_string()
    } else {
        format!("{schema}.{table}")
    }
}

fn process_schema_rows(rows: Vec<Value>, default_schema: &str) -> Vec<TableSchema> {
    let mut tables: Vec<TableSchema> = Vec::new();
    let mut last_key: Option<(String, String)> = None;

    for row in rows {
        let schema_name = row["TABLE_SCHEMA"].as_str().unwrap_or_default().to_string();
        let table_name = row["TABLE_NAME"].as_str().unwrap_or_default().to_string();
        let column_name = row["COLUMN_NAME"].as_str().unwrap_or_default().to_string();
        let data_type = row["DATA_TYPE"].as_str().unwrap_or_default().to_string();

        let full_name = display_name(&schema_name, &table_name, default_schema);
        let column = ColumnSchema::typed_or_blank(column_name, data_type);

        // Key on the raw (schema, table) pair so schemas differing only by case never merge.
        let key = (schema_name, table_name);
        if last_key.as_ref() == Some(&key) {
            tables
                .last_mut()
                .expect("last_key implies a pushed table")
                .columns
                .push(column);
        } else {
            tables.push(TableSchema {
                name: full_name,
                columns: vec![column],
            });
            last_key = Some(key);
        }
    }
    tables
}

/// The configured database, validated (with any `schema`) for interpolation into introspection
/// SQL, since the statements API has no parameterized queries.
fn introspection_db(opts: &SnowflakeOptions) -> Result<&str> {
    let db = opts.database.as_deref().ok_or_else(|| {
        anyhow::anyhow!("Snowflake schema introspection requires a database. Please edit the connection and specify a \"Database\".")
    })?;
    super::http_connectors::validate_identifier(db, "database")?;
    if let Some(schema) = &opts.schema {
        super::http_connectors::validate_identifier(schema, "schema")?;
    }
    Ok(db)
}

pub struct SnowflakeConnector;

#[async_trait]
impl QueryRunner for SnowflakeConnector {
    fn type_name(&self) -> &'static str {
        "snowflake"
    }

    fn configuration_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "title": "Account (e.g. xy12345.us-east-1)" },
                "user": { "type": "string", "title": "User" },
                "private_key": {
                    "type": "string",
                    "title": "Private Key (PEM, key-pair auth — required)",
                    "x-widget": "textarea"
                },
                "password": {
                    "type": "string",
                    "title": "Password (not currently supported by this connector — use Private Key)"
                },
                "warehouse": { "type": "string", "title": "Warehouse" },
                "database": { "type": "string", "title": "Database" },
                "schema": { "type": "string", "title": "Schema", "default": "PUBLIC" },
                "role": { "type": "string", "title": "Role (optional)" }
            },
            "required": ["account", "user", "database"],
            "secret": ["private_key", "password"]
        })
    }

    fn network_targets(&self, opts: &Value) -> Vec<NetworkTarget> {
        parse_options(opts)
            .map(|opts| {
                vec![NetworkTarget::Host(format!(
                    "{}.snowflakecomputing.com",
                    opts.account
                ))]
            })
            .unwrap_or_default()
    }

    async fn test_connection(&self, opts: &Value) -> Result<()> {
        let opts = parse_options(opts)?;
        run_statement(&opts, "SELECT 1").await.map(|_| ())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let opts = parse_options(opts)?;
        let sql = build_schema_sql(introspection_db(&opts)?, opts.schema.as_deref());
        let (_, rows) = run_statement(&opts, &sql)
            .await
            .context("Snowflake schema introspection failed")?;

        let default_schema = opts.schema.as_deref().unwrap_or("PUBLIC");
        Ok(process_schema_rows(rows, default_schema))
    }

    async fn get_view_names(&self, opts: &Value) -> Result<Vec<String>> {
        let opts = parse_options(opts)?;
        let sql = build_view_names_sql(introspection_db(&opts)?, opts.schema.as_deref());
        let (_, rows) = run_statement(&opts, &sql)
            .await
            .context("Snowflake view introspection failed")?;
        let default_schema = opts.schema.as_deref().unwrap_or("PUBLIC");
        Ok(rows
            .iter()
            .map(|r| {
                display_name(
                    r["TABLE_SCHEMA"].as_str().unwrap_or_default(),
                    r["TABLE_NAME"].as_str().unwrap_or_default(),
                    default_schema,
                )
            })
            .collect())
    }

    async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
        let opts = parse_options(opts)?;
        let start = Instant::now();
        let (columns, rows) = run_statement(&opts, query).await?;
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
    use jsonwebtoken::{decode, Algorithm as JwtAlgorithm, DecodingKey, Validation};
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};

    fn test_key_pem() -> (String, RsaPublicKey) {
        let mut rng = rand::thread_rng();
        let private_key = RsaPrivateKey::new(&mut rng, 2048).expect("generate test RSA key");
        let public_key = RsaPublicKey::from(&private_key);
        let pem = private_key
            .to_pkcs8_pem(LineEnding::LF)
            .expect("encode test private key");
        (pem.to_string(), public_key)
    }

    #[derive(Debug, Deserialize)]
    struct DecodedClaims {
        iss: String,
        sub: String,
    }

    #[test]
    fn build_jwt_signs_a_verifiable_rs256_jwt_with_expected_claims() {
        let (pem, public_key) = test_key_pem();
        let jwt = build_jwt("xy12345.us-east-1", "alice", &pem).expect("build_jwt failed");

        let public_pem = public_key
            .to_public_key_pem(LineEnding::LF)
            .expect("encode test public key");
        let expected_fingerprint = public_key_fingerprint(&parse_rsa_private_key(&pem).unwrap())
            .expect("fingerprint failed");

        let decoding_key =
            DecodingKey::from_rsa_pem(public_pem.as_bytes()).expect("decode public key");
        let mut validation = Validation::new(JwtAlgorithm::RS256);
        validation.required_spec_claims.clear();
        validation.validate_exp = true;

        let decoded = decode::<DecodedClaims>(&jwt, &decoding_key, &validation)
            .expect("signature must verify against the derived public key");
        // Region suffix (`.us-east-1`) must be stripped from the JWT account locator.
        assert_eq!(decoded.claims.sub, "XY12345.ALICE");
        assert_eq!(
            decoded.claims.iss,
            format!("XY12345.ALICE.{expected_fingerprint}")
        );
        assert!(expected_fingerprint.starts_with("SHA256:"));
    }

    /// `account` becomes a hostname segment: anything that could break out of it must be rejected,
    /// while real account forms still pass.
    #[test]
    fn account_identifier_rejects_hostname_injection_and_accepts_real_forms() {
        for bad in [
            "attacker.example/x?",
            "internal.svc:8080/x?",
            "a b",
            "acct@evil",
        ] {
            assert!(
                super::super::http_connectors::validate_identifier(bad, "Snowflake account")
                    .is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        assert!(super::super::http_connectors::validate_identifier(
            "xy12345.us-east-1",
            "Snowflake account"
        )
        .is_ok());
    }

    #[test]
    fn process_schema_rows_correctly_groups_and_qualifies_tables() {
        let rows = vec![
            json!({ "TABLE_SCHEMA": "PUBLIC", "TABLE_NAME": "USERS", "COLUMN_NAME": "ID", "DATA_TYPE": "NUMBER" }),
            json!({ "TABLE_SCHEMA": "PUBLIC", "TABLE_NAME": "USERS", "COLUMN_NAME": "NAME", "DATA_TYPE": "TEXT" }),
            json!({ "TABLE_SCHEMA": "STAGING", "TABLE_NAME": "USERS", "COLUMN_NAME": "ID", "DATA_TYPE": "NUMBER" }),
            json!({ "TABLE_SCHEMA": "staging", "TABLE_NAME": "USERS", "COLUMN_NAME": "EMAIL", "DATA_TYPE": "TEXT" }),
            json!({ "TABLE_SCHEMA": "CUSTOM_SCH", "TABLE_NAME": "ORDERS", "COLUMN_NAME": "ORDER_ID", "DATA_TYPE": "NUMBER" }),
        ];

        // Case 1: Default schema is "PUBLIC"
        let schema = process_schema_rows(rows.clone(), "PUBLIC");
        assert_eq!(schema.len(), 4);

        assert_eq!(schema[0].name, "USERS");
        assert_eq!(
            schema[0].columns,
            vec![
                ColumnSchema::typed("ID", "NUMBER"),
                ColumnSchema::typed("NAME", "TEXT"),
            ]
        );

        assert_eq!(schema[1].name, "STAGING.USERS");
        assert_eq!(schema[1].columns, vec![ColumnSchema::typed("ID", "NUMBER")]);

        assert_eq!(schema[2].name, "staging.USERS");
        assert_eq!(
            schema[2].columns,
            vec![ColumnSchema::typed("EMAIL", "TEXT")]
        );

        assert_eq!(schema[3].name, "CUSTOM_SCH.ORDERS");
        assert_eq!(
            schema[3].columns,
            vec![ColumnSchema::typed("ORDER_ID", "NUMBER")]
        );

        // Case 2: Configured schema is "staging" (case-insensitive check)
        let schema2 = process_schema_rows(rows, "staging");
        assert_eq!(schema2.len(), 4);

        assert_eq!(schema2[0].name, "PUBLIC.USERS");
        assert_eq!(
            schema2[0].columns,
            vec![
                ColumnSchema::typed("ID", "NUMBER"),
                ColumnSchema::typed("NAME", "TEXT"),
            ]
        );

        // Matches the configured default case-insensitively for display, but stays distinct from
        // "staging.USERS": grouping is on the raw pair.
        assert_eq!(schema2[1].name, "USERS");
        assert_eq!(
            schema2[1].columns,
            vec![ColumnSchema::typed("ID", "NUMBER")]
        );

        assert_eq!(schema2[2].name, "USERS");
        assert_eq!(
            schema2[2].columns,
            vec![ColumnSchema::typed("EMAIL", "TEXT")]
        );

        assert_eq!(schema2[3].name, "CUSTOM_SCH.ORDERS");
        assert_eq!(
            schema2[3].columns,
            vec![ColumnSchema::typed("ORDER_ID", "NUMBER")]
        );
    }

    #[test]
    fn build_schema_sql_restricts_table_type_and_optionally_schema() {
        let sql = build_schema_sql("MYDB", None);
        assert!(sql.contains("MYDB.INFORMATION_SCHEMA.COLUMNS"));
        assert!(sql.contains("MYDB.INFORMATION_SCHEMA.TABLES"));
        assert!(sql.contains("TABLE_TYPE IN ('BASE TABLE', 'VIEW')"));
        assert!(!sql.contains("AND UPPER(c.TABLE_SCHEMA) = UPPER("));

        let scoped = build_schema_sql("MYDB", Some("staging"));
        assert!(scoped.contains("TABLE_TYPE IN ('BASE TABLE', 'VIEW')"));
        assert!(scoped.contains("AND UPPER(c.TABLE_SCHEMA) = UPPER('staging')"));
    }

    #[test]
    fn build_view_names_sql_selects_views_and_optionally_scopes_schema() {
        let sql = build_view_names_sql("MYDB", None);
        assert!(sql.contains("MYDB.INFORMATION_SCHEMA.TABLES"));
        assert!(sql.contains("TABLE_TYPE = 'VIEW'"));
        assert!(!sql.contains("= UPPER("));

        let scoped = build_view_names_sql("MYDB", Some("staging"));
        assert!(scoped.contains("AND UPPER(TABLE_SCHEMA) = UPPER('staging')"));
    }

    #[test]
    fn display_name_matches_schema_spelling() {
        assert_eq!(display_name("PUBLIC", "V", "public"), "V");
        assert_eq!(display_name("STAGING", "V", "PUBLIC"), "STAGING.V");
    }

    #[test]
    fn friendly_type_mapping() {
        assert_eq!(friendly_type("fixed", 0), "integer");
        assert_eq!(friendly_type("fixed", 2), "float");
        assert_eq!(friendly_type("real", 0), "float");
        assert_eq!(friendly_type("boolean", 0), "boolean");
        assert_eq!(friendly_type("timestamp_ntz", 0), "datetime");
        assert_eq!(friendly_type("timestamp_tz", 0), "datetime");
        assert_eq!(friendly_type("date", 0), "date");
        assert_eq!(friendly_type("text", 0), "string");
        assert_eq!(friendly_type("variant", 0), "string");
    }

    #[test]
    fn cell_to_json_parses_by_declared_type() {
        assert_eq!(cell_to_json("fixed", 0, Some("42")), json!(42));
        assert_eq!(cell_to_json("fixed", 2, Some("12.34")), json!("12.34"));
        assert_eq!(cell_to_json("real", 0, Some("3.5")), json!(3.5));
        assert_eq!(cell_to_json("boolean", 0, Some("true")), json!(true));
        assert_eq!(cell_to_json("boolean", 0, Some("0")), json!(false));
        assert_eq!(cell_to_json("text", 0, Some("hi")), json!("hi"));
        assert_eq!(cell_to_json("fixed", 0, None), Value::Null);
        assert_eq!(cell_to_json("fixed", 0, Some("nope")), json!("nope"));
    }

    #[test]
    fn cell_to_json_decodes_timestamp_and_date_epochs() {
        // 2024-01-15T12:00:00Z
        let ts = cell_to_json("timestamp_ntz", 9, Some("1705320000.000000000"));
        assert_eq!(ts, json!("2024-01-15T12:00:00+00:00"));
        // 19737 days since epoch == 2024-01-15
        let d = cell_to_json("date", 0, Some("19737"));
        assert_eq!(d, json!("2024-01-15"));
    }

    #[test]
    fn parse_statement_response_reads_columns_and_rows() {
        let resp = json!({
            "resultSetMetaData": {
                "numRows": 2,
                "partitionInfo": [{ "rowCount": 2 }],
                "rowType": [
                    { "name": "ID", "type": "fixed", "scale": 0 },
                    { "name": "NAME", "type": "text", "scale": 0 }
                ]
            },
            "data": [ ["1", "a"], ["2", "b"] ]
        });
        let (field_defs, columns, rows) = parse_statement_response(&resp).expect("parse failed");
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].friendly_type, "integer");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["ID"], json!(1));
        assert_eq!(rows[1]["NAME"], json!("b"));

        // A later `?partition=N` page repeats only `data`; confirms continuation pages still decode
        // INTEGER.
        let page_two = json!({ "data": [ ["3", "c"] ] });
        let more_rows = rows_from_data(&page_two["data"], &field_defs);
        assert_eq!(more_rows[0]["ID"], json!(3));
        assert_eq!(more_rows[0]["NAME"], json!("c"));
    }

    #[test]
    fn check_row_cap_rejects_past_the_limit_and_accepts_at_it() {
        assert!(check_row_cap(SNOWFLAKE_MAX_ROWS).is_ok());
        let err = check_row_cap(SNOWFLAKE_MAX_ROWS + 1).unwrap_err();
        assert!(err.to_string().contains(&SNOWFLAKE_MAX_ROWS.to_string()));
    }

    #[test]
    fn configuration_schema_shape() {
        assert_eq!(SnowflakeConnector.type_name(), "snowflake");
        let schema = SnowflakeConnector.configuration_schema();
        assert_eq!(schema["required"], json!(["account", "user", "database"]));
        assert_eq!(schema["secret"], json!(["private_key", "password"]));
    }
}

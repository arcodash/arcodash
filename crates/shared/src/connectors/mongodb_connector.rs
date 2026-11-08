//! MongoDB connector via the official mongodb crate.

use std::fmt::Write;
use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use mongodb::bson::{doc, Bson, Document};
use mongodb::options::{ClientOptions, FindOptions};
use mongodb::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::http_connectors::percent_encode;
use super::{
    ColumnMeta, ColumnSchema, NetworkTarget, QueryExecutionResult, QueryRunner, TableSchema,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MongoOptions {
    pub host: String,
    #[serde(default = "mongo_port")]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
    pub db: String,
    #[serde(default = "default_auth_source")]
    pub auth_source: String,
    #[serde(default)]
    pub replica_set: String,
}

const fn mongo_port() -> u16 {
    27017
}

fn default_auth_source() -> String {
    "admin".to_string()
}

pub struct MongoConnector;

fn parse_options(opts: &Value) -> Result<MongoOptions> {
    super::parse_opts(opts, "invalid MongoDB options")
}

/// Reject a host that could inject a fake userinfo/path/query segment into the URI `build_uri`
/// interpolates it into unencoded (e.g. `evil.com/x@attacker.example`). Independent of
/// `net_guard::guard_host`, which is a no-op under `ARCODASH_ALLOW_PRIVATE_CONNECTORS=1`: this is
/// URI syntax, so it always runs.
fn validate_host(host: &str) -> Result<()> {
    if host.is_empty()
        || host
            .chars()
            .any(|c| "/@?#\\".contains(c) || c.is_whitespace())
    {
        anyhow::bail!(
            "MongoDB host must not be empty or contain '/', '@', '?', '#', '\\', or whitespace"
        );
    }
    Ok(())
}

fn build_uri(opts: &MongoOptions) -> String {
    let (host, port, db) = (&opts.host, opts.port, percent_encode(&opts.db));
    let (mut uri, sep) = if opts.user.is_empty() {
        (format!("mongodb://{host}:{port}/{db}"), '?')
    } else {
        let user = percent_encode(&opts.user);
        let pass = percent_encode(&opts.password);
        let auth_source = percent_encode(&opts.auth_source);
        let uri = format!("mongodb://{user}:{pass}@{host}:{port}/{db}?authSource={auth_source}");
        (uri, '&')
    };
    if !opts.replica_set.is_empty() {
        let _ = write!(uri, "{sep}replicaSet={}", percent_encode(&opts.replica_set));
    }
    uri
}

/// `ClientOptions::parse` errors can embed the URI, and so the (percent-encoded) password. Strip
/// both raw and encoded forms before the error reaches `queries.last_execution_error`, Redis and
/// the UI.
fn redact_password_from_parse_err(err: &mongodb::error::Error, password: &str) -> anyhow::Error {
    let mut text = err.to_string();
    if !password.is_empty() {
        text = text
            .replace(password, "***")
            .replace(&percent_encode(password), "***");
    }
    anyhow::anyhow!("invalid MongoDB URI: {text}")
}

async fn mongo_client(opts: &MongoOptions) -> Result<Client> {
    validate_host(&opts.host)?;
    crate::net_guard::guard_host(&opts.host).await?;
    let uri = build_uri(opts);
    let mut client_options = ClientOptions::parse(&uri)
        .await
        .map_err(|e| redact_password_from_parse_err(&e, &opts.password))?;
    client_options.connect_timeout = Some(super::CONNECT_TIMEOUT);
    client_options.server_selection_timeout = Some(super::CONNECT_TIMEOUT);
    Client::with_options(client_options).context("failed to create MongoDB client")
}

fn bson_to_json(bson: &Bson) -> Value {
    match bson {
        Bson::Null => Value::Null,
        Bson::Boolean(b) => json!(b),
        Bson::Int32(i) => json!(i),
        Bson::Int64(i) => json!(i),
        Bson::Double(f) => json!(f),
        Bson::String(s) => json!(s),
        Bson::DateTime(dt) => json!(dt.timestamp_millis()),
        Bson::ObjectId(oid) => json!(oid.to_hex()),
        Bson::Array(arr) => Value::Array(arr.iter().map(bson_to_json).collect()),
        Bson::Document(doc) => Value::Object(
            doc.iter()
                .map(|(k, v)| (k.clone(), bson_to_json(v)))
                .collect(),
        ),
        other => json!(other.to_string()),
    }
}

fn doc_to_json(doc: Document) -> Value {
    Value::Object(
        doc.into_iter()
            .map(|(k, v)| (k, bson_to_json(&v)))
            .collect(),
    )
}

/// Whether `err` means the data source is unusable (no server reachable/selectable, or credentials
/// rejected) rather than one bad query. The driver connects lazily, so this is the real "did we
/// connect" signal; anything else stays `Statement`.
fn is_connect_failure(err: &mongodb::error::Error) -> bool {
    matches!(
        err.kind.as_ref(),
        mongodb::error::ErrorKind::ServerSelection { .. }
            | mongodb::error::ErrorKind::Io(_)
            | mongodb::error::ErrorKind::DnsResolve { .. }
            | mongodb::error::ErrorKind::Authentication { .. }
    )
}

/// Tag a failed Mongo operation with [`super::connect_err`] when it's actually a connectivity
/// failure (see [`is_connect_failure`]).
fn classify_mongo_err(err: mongodb::error::Error) -> anyhow::Error {
    if is_connect_failure(&err) {
        super::connect_err(err.into())
    } else {
        err.into()
    }
}

/// Row cap for both `run_query` paths, hardcoded to the worker's default since `max_query_rows`
/// can't be threaded in. Mongo queries are JSON, so `apply_auto_limit` never applies.
const MAX_ROWS: i64 = 10_000;

/// Cap on collections `get_schema` samples, bounding `$sample` round trips.
const MAX_SCHEMA_COLLECTIONS: usize = 500;

/// Clamp a user `find` `limit` into `1..=MAX_ROWS`; Mongo treats `0` and negatives as "no limit",
/// so they're remapped to the cap.
fn clamp_mongo_limit(requested: i64) -> i64 {
    if requested <= 0 {
        MAX_ROWS
    } else {
        requested.min(MAX_ROWS)
    }
}

/// Append the row-cap `$limit` stage so the server bounds an aggregate pipeline. `MAX_ROWS + 1`, so
/// with `drain_cursor`'s cap an overflow surfaces as an error instead of silently dropped rows.
fn append_row_cap_stage(mut pipeline: Vec<Document>) -> Vec<Document> {
    pipeline.push(doc! { "$limit": MAX_ROWS + 1 });
    pipeline
}

/// Drain a Cursor<Document> via the low-level advance/deserialize API, erroring (naming the cap)
/// rather than silently truncating once `cap` documents are collected.
async fn drain_cursor(mut cursor: mongodb::Cursor<Document>, cap: usize) -> Result<Vec<Document>> {
    let mut docs = Vec::new();
    while cursor.advance().await.context("cursor advance failed")? {
        if docs.len() >= cap {
            anyhow::bail!(
                "MongoDB result exceeds this connector's row cap ({cap} rows); narrow the query"
            );
        }
        docs.push(
            cursor
                .deserialize_current()
                .context("cursor deserialize failed")?,
        );
    }
    Ok(docs)
}

#[async_trait]
impl QueryRunner for MongoConnector {
    fn type_name(&self) -> &'static str {
        "mongodb"
    }

    fn configuration_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": { "type": "string", "title": "Host" },
                "port": { "type": "integer", "title": "Port", "default": 27017 },
                "user": { "type": "string", "title": "Username" },
                "password": { "type": "string", "title": "Password" },
                "db": { "type": "string", "title": "Database" },
                "auth_source": { "type": "string", "title": "Auth Source", "default": "admin" },
                "replica_set": { "type": "string", "title": "Replica Set (optional)" }
            },
            "required": ["host", "db"],
            "secret": ["password"]
        })
    }

    async fn test_connection(&self, opts: &Value) -> Result<()> {
        let opts = parse_options(opts)?;
        let cl = mongo_client(&opts).await?;
        cl.database(&opts.db)
            .run_command(doc! { "ping": 1 })
            .await
            .context("MongoDB ping failed")?;
        Ok(())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let opts = parse_options(opts)?;
        let cl = mongo_client(&opts).await?;
        let db = cl.database(&opts.db);
        let mut names = db
            .list_collection_names()
            .await
            .context("list collections failed")?;

        if names.len() > MAX_SCHEMA_COLLECTIONS {
            tracing::warn!(
                total = names.len(),
                shown = MAX_SCHEMA_COLLECTIONS,
                "MongoDB database has more collections than schema introspection will list"
            );
            names.truncate(MAX_SCHEMA_COLLECTIONS);
        }

        let max_time = std::time::Duration::from_secs(super::db_statement_timeout_secs());
        let mut tables = Vec::new();
        for name in names {
            let coll = db.collection::<Document>(&name);
            let cursor = coll
                .aggregate(vec![doc! { "$sample": { "size": 1 } }])
                .max_time(max_time)
                .await
                .context("schema sample failed")?;
            let sample = drain_cursor(cursor, 1).await?;
            let columns = sample
                .first()
                .map(|doc| {
                    doc.keys()
                        .map(|k| ColumnSchema::untyped(k.as_str()))
                        .collect()
                })
                .unwrap_or_default();
            tables.push(TableSchema { name, columns });
        }
        Ok(tables)
    }

    fn network_targets(&self, opts: &Value) -> Vec<NetworkTarget> {
        parse_options(opts)
            .map(|opts| vec![NetworkTarget::Host(opts.host)])
            .unwrap_or_default()
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "MAX_ROWS and the clamped limit are in 1..=10_001"
    )]
    async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
        let opts = parse_options(opts)?;
        let cl = mongo_client(&opts).await?;
        let db = cl.database(&opts.db);

        let q: Value = serde_json::from_str(query.trim())
            .context("MongoDB query must be JSON: { \"collection\": \"name\", \"filter\": {} }")?;

        let coll_name = q["collection"]
            .as_str()
            .context("missing 'collection' field")?;
        let coll = db.collection::<Document>(coll_name);

        let max_time = std::time::Duration::from_secs(super::db_statement_timeout_secs());

        let start = Instant::now();
        let docs = if let Some(pipeline) = q.get("aggregate").and_then(Value::as_array) {
            let bson_pipeline: Vec<Document> = pipeline
                .iter()
                .enumerate()
                .map(|(i, stage)| {
                    mongodb::bson::to_document(stage).with_context(|| {
                        format!("aggregate pipeline stage {i} is not a valid BSON document")
                    })
                })
                .collect::<Result<_>>()?;
            let bson_pipeline = append_row_cap_stage(bson_pipeline);
            let cursor = coll
                .aggregate(bson_pipeline)
                .max_time(max_time)
                .await
                .map_err(classify_mongo_err)?;
            drain_cursor(cursor, MAX_ROWS as usize).await?
        } else {
            // Absent filter matches everything; a present-but-invalid one must error, not fall back
            // to "match everything".
            let filter = match q.get("filter") {
                None => Document::new(),
                Some(f) => mongodb::bson::to_document(f)
                    .context("MongoDB query 'filter' is not a valid BSON document")?,
            };
            let requested_limit = q.get("limit").and_then(Value::as_i64).unwrap_or(100);
            let limit = clamp_mongo_limit(requested_limit);
            // Request one more than `limit` so a result that exactly fills the server-side
            // `.limit()` is distinguishable from a complete one.
            let find_opts = FindOptions::builder()
                .limit(limit + 1)
                .max_time(max_time)
                .build();
            let cursor = coll
                .find(filter)
                .with_options(find_opts)
                .await
                .map_err(classify_mongo_err)?;
            let mut docs = drain_cursor(cursor, (limit + 1) as usize).await?;
            // `limit` is usually the user's own cap, so overflowing by one is an ordinary "there's
            // more", not an error, unlike the aggregate arm's `MAX_ROWS` overflow.
            if docs.len() > limit as usize {
                docs.truncate(limit as usize);
                tracing::warn!(
                    collection = coll_name,
                    limit,
                    "MongoDB find result truncated at the requested limit"
                );
            }
            docs
        };
        let runtime_seconds = start.elapsed().as_secs_f64();

        let mut seen = std::collections::HashSet::new();
        let col_names: Vec<String> = docs
            .iter()
            .flat_map(Document::keys)
            .filter(|k| seen.insert(*k))
            .cloned()
            .collect();

        Ok(QueryExecutionResult {
            columns: ColumnMeta::all_string(&col_names),
            rows: docs.into_iter().map(doc_to_json).collect(),
            runtime_seconds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::error::ErrorKind;

    #[test]
    fn clamp_mongo_limit_treats_zero_as_the_row_cap_not_unlimited() {
        assert_eq!(clamp_mongo_limit(0), MAX_ROWS);
    }

    #[test]
    fn clamp_mongo_limit_treats_negative_as_the_row_cap() {
        assert_eq!(clamp_mongo_limit(-5), MAX_ROWS);
    }

    #[test]
    fn clamp_mongo_limit_caps_an_over_large_request() {
        assert_eq!(clamp_mongo_limit(MAX_ROWS + 500), MAX_ROWS);
    }

    #[test]
    fn clamp_mongo_limit_leaves_a_small_request_untouched() {
        assert_eq!(clamp_mongo_limit(5), 5);
    }

    #[test]
    fn append_row_cap_stage_appends_a_limit_one_past_the_row_cap() {
        let pipeline = append_row_cap_stage(vec![doc! { "$match": {} }]);
        assert_eq!(pipeline.last(), Some(&doc! { "$limit": MAX_ROWS + 1 }));
    }

    #[test]
    fn validate_host_rejects_uri_breaking_characters() {
        for bad in [
            "evil.com/x@attacker.example",
            "a@b",
            "a?b",
            "a#b",
            "a\\b",
            "a b",
            "",
        ] {
            assert!(
                validate_host(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn validate_host_accepts_ordinary_hostnames() {
        assert!(validate_host("mongo.internal.example.com").is_ok());
        assert!(validate_host("10.0.0.5").is_ok());
    }

    fn opts(password: &str, db: &str, auth_source: &str, replica_set: &str) -> MongoOptions {
        MongoOptions {
            host: "host".to_string(),
            port: 27017,
            user: "user".to_string(),
            password: password.to_string(),
            db: db.to_string(),
            auth_source: auth_source.to_string(),
            replica_set: replica_set.to_string(),
        }
    }

    #[test]
    fn build_uri_percent_encodes_db_auth_source_and_replica_set() {
        let opts = opts("pw", "db name/with?special&chars", "admin&x=1", "rs/0");
        let uri = build_uri(&opts);
        assert!(!uri.contains("db name/with?special&chars"));
        assert!(!uri.contains("authSource=admin&x=1"));
        assert!(uri.contains("authSource=admin%26x%3D1"));
        assert!(!uri.contains("replicaSet=rs/0"));
        assert!(uri.contains("replicaSet=rs%2F0"));
    }

    /// A non-ASCII password must percent-encode its UTF-8 bytes, not the code point, or the server
    /// can't decode it and authentication fails.
    #[test]
    fn build_uri_percent_encodes_non_ascii_password_to_a_decodable_form() {
        let opts = opts("pásswörd中", "db", "admin", "");
        let uri = build_uri(&opts);
        assert!(uri.contains(&percent_encode("pásswörd中")));

        let encoded_pass = uri
            .split("://")
            .nth(1)
            .unwrap()
            .split(':')
            .nth(1)
            .unwrap()
            .split('@')
            .next()
            .unwrap();
        let bytes = encoded_pass.as_bytes();
        let mut decoded = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                decoded.push(u8::from_str_radix(hex, 16).unwrap());
                i += 3;
            } else {
                decoded.push(bytes[i]);
                i += 1;
            }
        }
        assert_eq!(String::from_utf8(decoded).unwrap(), "pásswörd中");
    }

    /// The redaction is a defensive strip regardless of whether the driver echoes the connection
    /// string. `ErrorKind::Io` is the only variant constructible outside the driver crate, so it
    /// stands in for an error whose text contains the password.
    #[test]
    fn redact_password_from_parse_err_strips_the_password() {
        let io_err = std::io::Error::other("connection failed near secret-pw-1 in the URI");
        let err: mongodb::error::Error = ErrorKind::Io(std::sync::Arc::new(io_err)).into();
        let redacted = redact_password_from_parse_err(&err, "secret-pw-1");
        let text = redacted.to_string();
        assert!(
            !text.contains("secret-pw-1"),
            "password leaked into error text: {text}"
        );
        assert!(text.contains("***"));
    }

    /// Unreachable-host failures can't be exercised live, so mock them. `mongo_client()` does no
    /// I/O, so `is_connect_failure` is the real "did we connect" signal.
    /// `ServerSelection`/`DnsResolve`/`Authentication` are `#[non_exhaustive]` and can't be
    /// constructed here, so only `Io` and `SessionsNotSupported` are tested; `Authentication` is
    /// covered by `auth_failure_classifies_as_connect` below.
    #[test]
    fn is_connect_failure_flags_io_errors_but_not_everything_else() {
        let io: mongodb::error::Error = ErrorKind::Io(std::sync::Arc::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        )))
        .into();
        assert!(is_connect_failure(&io));

        let not_connectivity: mongodb::error::Error = ErrorKind::SessionsNotSupported.into();
        assert!(!is_connect_failure(&not_connectivity));
    }

    /// A rejected password is a `Connect` failure like an unreachable host, so the health hook
    /// flags it. Needs a real Mongo (`TEST_MONGO_URL`) since `Authentication` errors can't be
    /// constructed.
    #[tokio::test]
    async fn auth_failure_classifies_as_connect() {
        let Ok(url) = std::env::var("TEST_MONGO_URL") else {
            println!("[SKIP] TEST_MONGO_URL not set");
            return;
        };
        let rest = url.strip_prefix("mongodb://").expect("mongodb:// prefix");
        let (_, hostpath) = rest.split_once('@').expect("userinfo@host");
        let (hostport, dbname) = hostpath.split_once('/').expect("host/db");
        let (host, port_str) = hostport.split_once(':').unwrap_or((hostport, "27017"));

        let opts = MongoOptions {
            host: host.to_string(),
            port: port_str.parse().unwrap_or(27017),
            user: "root".to_string(),
            password: "wrong-password".to_string(),
            db: dbname.to_string(),
            auth_source: default_auth_source(),
            replica_set: String::new(),
        };

        let err = MongoConnector
            .run_query(
                &serde_json::to_value(&opts).unwrap(),
                r#"{"collection":"conn_test","filter":{}}"#,
            )
            .await
            .expect_err("wrong password must fail");

        assert_eq!(
            super::super::classify_run_query_error(&err),
            super::super::ConnectorErrorKind::Connect
        );
    }
}

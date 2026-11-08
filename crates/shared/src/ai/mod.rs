//! Provider abstraction for "Ask with AI" text-to-SQL and key testing: one trait, one shared error
//! type, one shared success type.
//!
//! Callers translate `AiError` into their own wire responses, since the same error reads
//! differently per endpoint.

pub mod anthropic;
pub mod budget;
pub mod context;
pub mod describe;
pub mod fallback;
pub mod gemini;
pub mod monitor_suggestions;
pub mod narrative;
pub mod openai;
pub mod pricing;
pub mod settings;
pub mod usage;
pub mod viz;

use std::collections::HashSet;

use crate::connectors::TableSchema;
use crate::entities::data_source;
use crate::models::DataSourceSchema;
use serde::Deserialize;

use describe::SchemaDescriptions;
use monitor_suggestions::MonitorSuggestionGeneration;
use viz::VizGeneration;

/// Which AI provider a key/model belongs to. `as_str`/`FromStr` are the JSON keys used in org
/// settings and API bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AiProviderKind {
    Gemini,
    OpenAi,
    Anthropic,
}

impl AiProviderKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

impl std::str::FromStr for AiProviderKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "gemini" => Ok(Self::Gemini),
            "openai" => Ok(Self::OpenAi),
            "anthropic" => Ok(Self::Anthropic),
            _ => Err(()),
        }
    }
}

/// Provider-agnostic failure modes with no message text, since the same variant reads differently
/// per endpoint.
#[derive(Debug)]
pub enum AiError {
    InvalidKey,
    /// `free_tier`: the provider reported the exhausted quota as a free-tier one.
    QuotaExceeded {
        free_tier: bool,
    },
    /// Per-minute provider rate limit; transient.
    RateLimited {
        retry_after_secs: u64,
        free_tier: bool,
    },
    Network(String),
    GenerationFailed(String),
    /// Provider overloaded or erroring (HTTP 5xx / Anthropic 529); transient.
    Unavailable(String),
    /// The org's monthly spend cap is reached; never retried against another provider.
    BudgetExceeded {
        cap: f64,
        spent: f64,
        resets_at: chrono::DateTime<chrono::Utc>,
    },
}

/// User-facing text for [`AiError::BudgetExceeded`].
#[must_use]
pub fn budget_message(cap: f64, resets_at: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "Monthly AI budget of ${cap:.2} reached; resets {}.",
        resets_at.format("%Y-%m-%d")
    )
}

/// User-facing text for [`AiError::Unavailable`] on generation endpoints.
pub const AI_BUSY_MESSAGE: &str =
    "The AI provider is busy right now (high demand). Try again in a moment.";

/// A `reqwest::Error`'s `Display` embeds the request URL, and Gemini carries its API key in the
/// query string; strip it before the text reaches a log, metric or `AiError`. Applied uniformly to
/// every provider.
#[must_use]
pub fn redact(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

/// A provider error body cut to a loggable length.
pub(crate) fn log_snippet(body: &str) -> String {
    body.chars().take(2000).collect()
}

/// Provider bodies are untrusted (`OpenAI`'s host is org-configurable): read at most this much.
const MAX_PROVIDER_BODY_BYTES: usize = 1 << 20; // 1 MiB

/// Reads a provider response body with a hard cap, instead of `resp.text()`'s unbounded buffer:
/// rejects an over-cap `Content-Length` up front, then streams so a missing or lying header can't
/// exceed it.
///
/// Non-UTF-8 bytes are replaced.
pub async fn read_capped(resp: reqwest::Response) -> Result<String, AiError> {
    use tokio_stream::StreamExt;

    if let Some(len) = resp.content_length() {
        if len > MAX_PROVIDER_BODY_BYTES as u64 {
            return Err(AiError::GenerationFailed(format!(
                "response body of {len} bytes exceeds the {MAX_PROVIDER_BODY_BYTES}-byte cap"
            )));
        }
    }

    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| AiError::Network(redact(e)))?;
        if buf.len() + chunk.len() > MAX_PROVIDER_BODY_BYTES {
            return Err(AiError::GenerationFailed(format!(
                "response body exceeded the {MAX_PROVIDER_BODY_BYTES}-byte cap"
            )));
        }
        buf.extend_from_slice(&chunk);
    }

    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Parsed `{sql, explanation}` result of a successful generation call, plus token counts from the
/// provider's response envelope (`None` if not reported).
#[derive(Debug, Deserialize)]
pub struct SqlGeneration {
    pub sql: String,
    #[serde(default)]
    pub explanation: Option<String>,
    #[serde(default, skip_deserializing)]
    pub tokens_in: Option<i64>,
    #[serde(default, skip_deserializing)]
    pub tokens_out: Option<i64>,
}

/// Parsed output of [`AiProvider::generate_json`] plus token counts.
#[derive(Debug, Clone)]
pub struct JsonGeneration {
    pub value: serde_json::Value,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
}

/// One provider's HTTP mapping, stateless beyond the shared `reqwest::Client`. `Send + Sync` so
/// `Box<dyn AiProvider>` stays `Send` across an `.await` (axum's `Handler` bound).
#[async_trait::async_trait]
pub trait AiProvider: Send + Sync {
    fn kind(&self) -> AiProviderKind;
    fn default_model(&self) -> &'static str;
    fn curated_models(&self) -> &'static [&'static str];

    /// Key-validation call, cheap where the provider has a per-model metadata endpoint; checks
    /// `model` specifically so a key that can't reach the configured model fails here.
    async fn test_key(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
    ) -> Result<(), AiError>;

    /// Structured text-to-SQL. Returns the parsed `{sql, explanation}`.
    async fn generate_sql(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<SqlGeneration, AiError>;

    /// Structured visualization suggestion ("Auto Visualize"), projected into a [`VizGeneration`].
    async fn suggest_visualization(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<VizGeneration, AiError>;

    /// Structured data-quality check suggestions ("Suggest checks with AI") plus token counts.
    async fn suggest_monitors(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<MonitorSuggestionGeneration, AiError>;

    /// Structured per-column descriptions for a whole schema (one call per data source).
    async fn describe_schema(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<SchemaDescriptions, AiError>;

    /// Schema-constrained JSON generation. `schema` is one OpenAI-strict JSON schema; each
    /// provider adapts it to its own structured-output mechanism.
    async fn generate_json(
        &self,
        _http: &reqwest::Client,
        _api_key: &str,
        _model: &str,
        _prompt: &str,
        _schema: &serde_json::Value,
    ) -> Result<JsonGeneration, AiError> {
        Err(AiError::GenerationFailed(
            "structured JSON generation is not supported".into(),
        ))
    }
}

/// Dispatch registry turning an [`AiProviderKind`] into a live [`AiProvider`]. `base_url` is the
/// org-configurable endpoint override, only used for `OpenAI` (Anthropic has a single fixed host).
#[must_use]
pub fn provider_for(kind: AiProviderKind, base_url: Option<String>) -> Box<dyn AiProvider> {
    match kind {
        AiProviderKind::Gemini => Box::new(gemini::GeminiProvider),
        AiProviderKind::OpenAi => Box::new(openai::OpenAiProvider::new(base_url)),
        AiProviderKind::Anthropic => Box::new(anthropic::AnthropicProvider),
    }
}

#[must_use]
pub fn into_tables(schema: DataSourceSchema) -> Vec<TableSchema> {
    schema
        .tables
        .into_iter()
        .map(|t| TableSchema {
            name: t.name,
            columns: t.columns,
        })
        .collect()
}

#[must_use]
pub fn dialect_name(type_name: &str) -> &'static str {
    match type_name {
        "pg" => "PostgreSQL",
        "mysql" => "MySQL",
        "mariadb" => "MariaDB",
        "clickhouse" => "ClickHouse",
        "mongodb" => "MongoDB",
        "sqlite" => "SQLite",
        _ => "standard SQL",
    }
}

/// Cached `data_sources.schema` as tables, or `None` if absent, unreadable or empty (the caller
/// then goes live). `query_results` never has one: its schema differs per caller.
pub fn cached_schema(ds: &data_source::Model) -> Option<Vec<TableSchema>> {
    if ds.r#type == "query_results" {
        return None;
    }
    let schema = match serde_json::from_value::<DataSourceSchema>(ds.schema.clone()?) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(data_source_id = ds.id, error = ?e, "cached schema unreadable, using live lookup");
            return None;
        }
    };
    if schema.tables.is_empty() {
        return None;
    }
    Some(into_tables(schema))
}

/// One table per line, `table(col type, col, ...)`; untyped columns render bare.
#[must_use]
pub fn format_schema_for_prompt(schema: &[TableSchema]) -> String {
    schema
        .iter()
        .map(|t| {
            let cols = t
                .columns
                .iter()
                .map(|c| {
                    c.type_name
                        .as_ref()
                        .map_or_else(|| c.name.clone(), |ty| format!("{} {}", c.name, ty))
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}({})", t.name, cols)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Word-boundary tokenizer for matching schema names against raw SQL: splits on
/// non-alphanumeric/`_`, lowercased.
#[must_use]
pub fn tokenize(text: &str) -> HashSet<String> {
    words(text).collect()
}

/// [`tokenize`]'s words in order, duplicates kept.
pub(crate) fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
}

/// Strips leading whitespace and SQL comments (`--…`/`/*…*/`) so a commented-out preamble can't
/// hide the real first keyword from `is_read_only_sql`'s prefix check.
fn strip_leading_trivia(sql: &str) -> &str {
    let mut s = sql;
    loop {
        let t = s.trim_start();
        if let Some(rest) = t.strip_prefix("--") {
            match rest.find('\n') {
                Some(idx) => s = &rest[idx + 1..],
                None => return "",
            }
        } else if let Some(rest) = t.strip_prefix("/*") {
            match rest.find("*/") {
                Some(idx) => s = &rest[idx + 2..],
                None => return "",
            }
        } else {
            return t;
        }
    }
}

/// Cheap guard against a hallucinated or prompt-injected write statement reaching the connector
/// (the suggestion card is actionable without reading the SQL).
///
/// Requires a single `select`/`with` statement with no DML/DDL keyword as a whole word.
/// Deliberately conservative, not a substitute for the connector's own permissions.
#[must_use]
pub fn is_read_only_sql(sql: &str) -> bool {
    const FORBIDDEN: &[&str] = &[
        "insert", "update", "delete", "drop", "alter", "truncate", "grant", "revoke", "create",
        "merge", "call", "exec", "execute", "copy", "vacuum", "reindex", "into", "replace",
    ];
    let trimmed = sql.trim();
    let first = strip_leading_trivia(trimmed).to_lowercase();
    if !(first.starts_with("select") || first.starts_with("with")) {
        return false;
    }

    // Allow exactly one optional trailing semicolon; any other one means a second statement.
    let statement = trimmed.strip_suffix(';').unwrap_or(trimmed);
    if statement.contains(';') {
        return false;
    }

    let tokens = tokenize(&statement.to_lowercase());
    !FORBIDDEN.iter().any(|kw| tokens.contains(*kw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::ColumnSchema;

    fn ds_with(r#type: &str, schema: Option<serde_json::Value>) -> data_source::Model {
        let now = chrono::Utc::now().into();
        data_source::Model {
            id: 7,
            org_id: 1,
            name: "t".to_string(),
            r#type: r#type.to_string(),
            encrypted_options: None,
            scheduled_queue_name: None,
            schema,
            schema_refreshed_at: None,
            paused_at: None,
            pause_reason: None,
            health_state: "unknown".to_string(),
            health_error: None,
            health_checked_at: None,
            ai_profile: None,
            ai_profile_built_at: None,
            ai_profile_last_error: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn cached_schema_serves_a_usable_cache() {
        let ds = ds_with(
            "pg",
            Some(serde_json::json!({ "tables": [{
                "name": "orders",
                "columns": [{ "name": "id", "type_name": "integer" }]
            }]})),
        );
        let tables = cached_schema(&ds).unwrap();
        assert_eq!(format_schema_for_prompt(&tables), "orders(id integer)");
    }

    /// `suggest_monitors` re-checks live only when this is `Some`, so every unusable cache (absent,
    /// empty, unreadable, any `query_results` source) must be `None`.
    #[test]
    fn cached_schema_is_none_for_every_unusable_cache() {
        assert!(cached_schema(&ds_with("pg", None)).is_none());
        assert!(cached_schema(&ds_with("pg", Some(serde_json::json!({ "tables": [] })))).is_none());
        assert!(cached_schema(&ds_with(
            "pg",
            Some(serde_json::json!({ "tables": "nope" }))
        ))
        .is_none());
        let usable = serde_json::json!({ "tables": [{ "name": "t", "columns": [] }] });
        assert!(cached_schema(&ds_with("query_results", Some(usable))).is_none());
    }

    #[test]
    fn read_only_sql_accepts_select_and_with() {
        assert!(is_read_only_sql("SELECT COUNT(*) AS row_count FROM orders"));
        assert!(is_read_only_sql(
            "with recent as (select 1) select * from recent"
        ));
        assert!(
            is_read_only_sql("  select 1;  "),
            "trailing semicolon is fine"
        );
        assert!(
            is_read_only_sql("-- a comment\nSELECT 1"),
            "leading comment before the real statement is fine"
        );
    }

    #[test]
    fn read_only_sql_rejects_non_select_prefix() {
        assert!(!is_read_only_sql("DROP TABLE orders"));
        assert!(!is_read_only_sql("DELETE FROM orders"));
        assert!(!is_read_only_sql("UPDATE orders SET status = 'x'"));
        assert!(!is_read_only_sql("INSERT INTO orders (id) VALUES (1)"));
    }

    #[test]
    fn read_only_sql_rejects_multiple_statements() {
        assert!(!is_read_only_sql("SELECT 1; DROP TABLE orders"));
        assert!(!is_read_only_sql("SELECT 1;;"));
    }

    #[test]
    fn read_only_sql_rejects_forbidden_keyword_even_with_a_leading_select() {
        assert!(!is_read_only_sql(
            "SELECT 1 FROM orders WHERE 1=1 OR delete"
        ));
    }

    #[test]
    fn read_only_sql_does_not_false_positive_on_substrings() {
        // "created_at" contains "create" as a substring but must not trip the whole-word guard.
        assert!(is_read_only_sql(
            "SELECT COUNT(*) AS row_count FROM orders WHERE created_at IS NOT NULL"
        ));
    }

    #[test]
    fn format_schema_renders_typed_and_untyped_columns() {
        let schema = vec![
            TableSchema {
                name: "orders".into(),
                columns: vec![
                    ColumnSchema::typed("id", "integer"),
                    ColumnSchema::typed("created_at", "timestamp with time zone"),
                    ColumnSchema::untyped("note"),
                ],
            },
            TableSchema {
                name: "empty".into(),
                columns: vec![],
            },
        ];
        assert_eq!(
            format_schema_for_prompt(&schema),
            "orders(id integer, created_at timestamp with time zone, note)\nempty()"
        );
    }

    #[test]
    fn format_schema_empty_is_empty_string() {
        assert_eq!(format_schema_for_prompt(&[]), "");
    }

    #[test]
    fn provider_for_dispatches_to_matching_kind() {
        assert_eq!(
            provider_for(AiProviderKind::Gemini, None).kind(),
            AiProviderKind::Gemini
        );
        assert_eq!(
            provider_for(
                AiProviderKind::OpenAi,
                Some("https://api.mistral.ai".to_string())
            )
            .kind(),
            AiProviderKind::OpenAi
        );
        assert_eq!(
            provider_for(AiProviderKind::Anthropic, None).kind(),
            AiProviderKind::Anthropic
        );
    }

    #[test]
    fn provider_kind_round_trips_through_json_keys() {
        for kind in [
            AiProviderKind::Gemini,
            AiProviderKind::OpenAi,
            AiProviderKind::Anthropic,
        ] {
            assert_eq!(kind.as_str().parse::<AiProviderKind>().unwrap(), kind);
        }
    }

    #[test]
    fn provider_kind_rejects_unknown_string() {
        assert!("bogus".parse::<AiProviderKind>().is_err());
    }

    /// A real `reqwest::Error` carries the request URL in its `Display`; redaction must remove the
    /// host and any `key=` query param (Gemini's key shape).
    #[tokio::test]
    async fn redact_strips_the_url_from_a_reqwest_error() {
        let client = reqwest::Client::new();
        let err = client
            .get("http://127.0.0.1:1/v1beta/models/gemini-3.1-flash-lite?key=super-secret-key")
            .send()
            .await
            .expect_err("port 1 must refuse the connection");
        let redacted = redact(err);
        assert!(
            !redacted.contains("key="),
            "redacted error still carries the query string: {redacted}"
        );
        assert!(
            !redacted.contains("127.0.0.1"),
            "redacted error still carries the host: {redacted}"
        );
    }

    /// One-shot local server answering 200 with `body` and its true `Content-Length`.
    async fn respond_with(body: Vec<u8>) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        });
        reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn read_capped_returns_a_small_body_unchanged() {
        let resp = respond_with(b"{\"ok\":true}".to_vec()).await;
        assert_eq!(read_capped(resp).await.unwrap(), "{\"ok\":true}");
    }

    #[tokio::test]
    async fn read_capped_rejects_a_body_over_the_cap() {
        let resp = respond_with(vec![b'a'; MAX_PROVIDER_BODY_BYTES + 1024]).await;
        let result = read_capped(resp).await;
        assert!(
            matches!(result, Err(AiError::GenerationFailed(_))),
            "expected a cap rejection, got {result:?}"
        );
    }
}

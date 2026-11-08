//! `AnthropicProvider` — the `AiProvider` mapping for Anthropic's `/v1/messages` API.
//!
//! Headers are `x-api-key` + `anthropic-version: 2023-06-01` (no bearer auth, unlike `OpenAI`).
//! Anthropic has no free list-models-by-key endpoint, so `test_key` spends a minimal real
//! generation call (`max_tokens: 1`) rather than a cheap list call like Gemini/OpenAI use.
//! Structured output via forced tool-use: a single tool (e.g. `emit_sql`) with `tool_choice` pinned
//! to it guarantees the response carries a `tool_use` block whose `input` is the structured result
//! — Anthropic's equivalent of Gemini's `responseSchema` / `OpenAI`'s `json_schema` response
//! format.

use std::time::Duration;

use serde::Deserialize;

use super::describe::{parse_descriptions_json, response_schema, SchemaDescriptions};
use super::monitor_suggestions::{
    parse_suggestions_json, suggest_schema, MonitorSuggestionGeneration,
};
use super::viz::{project_flat_viz, viz_schema, VizGeneration};
use super::{
    log_snippet, read_capped, redact, AiError, AiProvider, AiProviderKind, JsonGeneration,
    SqlGeneration,
};

const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const SQL_TIMEOUT: Duration = Duration::from_secs(30);
const VIZ_TIMEOUT: Duration = Duration::from_secs(45);
const SUGGEST_TIMEOUT: Duration = Duration::from_secs(90);
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(90);

pub struct AnthropicProvider;

#[async_trait::async_trait]
impl AiProvider for AnthropicProvider {
    fn kind(&self) -> AiProviderKind {
        AiProviderKind::Anthropic
    }

    fn default_model(&self) -> &'static str {
        "claude-sonnet-5-5"
    }

    fn curated_models(&self) -> &'static [&'static str] {
        &[
            "claude-sonnet-5-5",
            "claude-sonnet-5",
            "claude-opus-5-5",
            "claude-haiku-4-5",
        ]
    }

    async fn test_key(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
    ) -> Result<(), AiError> {
        let body = serde_json::json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let resp = messages_request(http, ANTHROPIC_MESSAGES_URL, api_key, &body)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| AiError::Network(redact(e)))?;

        map_test_key_status(resp.status().as_u16())
    }

    async fn generate_sql(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<SqlGeneration, AiError> {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "sql": { "type": "string" },
                "explanation": { "type": "string" }
            },
            "required": ["sql"]
        });
        let body = tool_body(
            model,
            prompt,
            2048,
            "emit_sql",
            "Emit the generated SQL query and an explanation of what it does.",
            &schema,
        );
        parse_anthropic_body(&call(http, api_key, &body, SQL_TIMEOUT).await?)
    }

    async fn suggest_visualization(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<VizGeneration, AiError> {
        let body = tool_body(
            model,
            prompt,
            2048,
            "emit_visualization",
            "Emit the chosen visualization type, its config, and a short title.",
            &viz_schema(false),
        );
        let text = call(http, api_key, &body, VIZ_TIMEOUT).await?;
        let (flat, tokens_in, tokens_out) = tool_input(&text, "emit_visualization")?;
        let (title, suggestion) = project_flat_viz(&flat);
        Ok(VizGeneration {
            title,
            suggestion,
            tokens_in,
            tokens_out,
        })
    }

    async fn suggest_monitors(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<MonitorSuggestionGeneration, AiError> {
        let body = tool_body(
            model,
            prompt,
            4096,
            "emit_monitor_suggestions",
            "Emit the proposed data-quality check suggestions.",
            &suggest_schema(false),
        );
        parse_anthropic_suggest_body(&call(http, api_key, &body, SUGGEST_TIMEOUT).await?)
    }

    async fn describe_schema(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<SchemaDescriptions, AiError> {
        let body = describe_body(model, prompt);
        parse_anthropic_describe_body(&call(http, api_key, &body, DESCRIBE_TIMEOUT).await?)
    }

    async fn generate_json(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
        schema: &serde_json::Value,
    ) -> Result<JsonGeneration, AiError> {
        call_anthropic_json(http, ANTHROPIC_MESSAGES_URL, model, api_key, prompt, schema).await
    }
}

/// 5.5-generation models reject a pinned `tool_choice` (400); `auto` still calls the only tool.
/// Detected by model-name heuristic; switch to a 400-retry if more model families need it.
fn tool_choice(model: &str, name: &str) -> serde_json::Value {
    if model.contains("-5-5") {
        serde_json::json!({ "type": "auto" })
    } else {
        serde_json::json!({ "type": "tool", "name": name })
    }
}

/// A `messages` body offering the single tool `tool` and steering the model to answer through it.
fn tool_body(
    model: &str,
    prompt: &str,
    max_tokens: u32,
    tool: &str,
    description: &str,
    input_schema: &serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": [{ "role": "user", "content": prompt }],
        "tools": [{
            "name": tool,
            "description": description,
            "input_schema": input_schema
        }],
        "tool_choice": tool_choice(model, tool)
    })
}

fn describe_body(model: &str, prompt: &str) -> serde_json::Value {
    tool_body(
        model,
        prompt,
        8192,
        "emit_schema_descriptions",
        "Emit a short description for every column.",
        &response_schema(false),
    )
}

fn messages_request(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
) -> reqwest::RequestBuilder {
    http.post(url)
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json")
        .json(body)
}

/// POSTs a tool `body`; returns the response if it is a 200, else the mapped status error.
async fn post_tool(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
    timeout: Duration,
) -> Result<reqwest::Response, AiError> {
    let tool = &body["tools"][0]["name"];
    tracing::debug!(model = %body["model"], %tool, "calling anthropic messages");
    let resp = messages_request(http, url, api_key, body)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| {
            let redacted = redact(e);
            tracing::warn!(error = %redacted, %tool, "anthropic messages request failed");
            AiError::Network(redacted)
        })?;
    let status = resp.status().as_u16();
    if status == 200 {
        return Ok(resp);
    }
    let text = log_snippet(&read_capped(resp).await.unwrap_or_default());
    tracing::warn!(%status, body = %text, %tool, "anthropic messages returned unexpected status");
    Err(map_generate_status_error(status))
}

/// [`post_tool`] against the real endpoint, returning the 200 body.
async fn call(
    http: &reqwest::Client,
    api_key: &str,
    body: &serde_json::Value,
    timeout: Duration,
) -> Result<String, AiError> {
    let resp = post_tool(http, ANTHROPIC_MESSAGES_URL, api_key, body, timeout).await?;
    read_capped(resp).await.inspect_err(|e| {
        tracing::warn!(error = ?e, "failed to read anthropic response body");
    })
}

async fn call_anthropic_json(
    http: &reqwest::Client,
    url: &str,
    model: &str,
    api_key: &str,
    prompt: &str,
    schema: &serde_json::Value,
) -> Result<JsonGeneration, AiError> {
    let body = tool_body(
        model,
        prompt,
        4096,
        "emit_json",
        "Emit the result as a JSON object matching the schema.",
        schema,
    );
    let resp = post_tool(http, url, api_key, &body, SUGGEST_TIMEOUT).await?;
    parse_anthropic_json_body(&read_capped(resp).await.unwrap_or_default())
}

/// Logs stop reason and block types only, never model text.
fn warn_missing_tool_block(envelope: &serde_json::Value, tool: &str) {
    let types: Vec<&str> = envelope["content"]
        .as_array()
        .map(|b| b.iter().filter_map(|x| x["type"].as_str()).collect())
        .unwrap_or_default();
    tracing::warn!(tool, stop_reason = ?envelope["stop_reason"].as_str(), block_types = ?types, "anthropic returned no tool_use block");
}

/// Scans for the named `tool_use` block rather than indexing `content[0]`: a text block can
/// precede it even with `tool_choice` pinned.
fn parse_anthropic_json_body(text_body: &str) -> Result<JsonGeneration, AiError> {
    let envelope: serde_json::Value =
        serde_json::from_str(text_body).map_err(|e| AiError::GenerationFailed(e.to_string()))?;
    let value = envelope["content"]
        .as_array()
        .and_then(|blocks| {
            blocks.iter().find(|b| {
                b["type"] == "tool_use" && b["name"] == "emit_json" && b.get("input").is_some()
            })
        })
        .map(|b| b["input"].clone())
        .ok_or_else(|| {
            warn_missing_tool_block(&envelope, "emit_json");
            AiError::GenerationFailed("no emit_json tool_use block".to_string())
        })?;
    Ok(JsonGeneration {
        value,
        tokens_in: envelope["usage"]["input_tokens"].as_i64(),
        tokens_out: envelope["usage"]["output_tokens"].as_i64(),
    })
}

fn parse_anthropic_describe_body(text_body: &str) -> Result<SchemaDescriptions, AiError> {
    let envelope: serde_json::Value =
        serde_json::from_str(text_body).map_err(|e| AiError::GenerationFailed(e.to_string()))?;
    let root = envelope["content"]
        .as_array()
        .and_then(|blocks| {
            blocks
                .iter()
                .find(|b| b["type"] == "tool_use" && b["name"] == "emit_schema_descriptions")
        })
        .and_then(|b| b.get("input"))
        .ok_or_else(|| {
            warn_missing_tool_block(&envelope, "emit_schema_descriptions");
            AiError::GenerationFailed("no emit_schema_descriptions tool_use block".to_string())
        })?;
    Ok(SchemaDescriptions {
        tables: parse_descriptions_json(root),
        tokens_in: envelope["usage"]["input_tokens"].as_i64(),
        tokens_out: envelope["usage"]["output_tokens"].as_i64(),
    })
}

/// Pure HTTP-status → `AiError` mapping for non-200 generation responses. Anthropic returns
/// 401/403 for a bad key and 400 for a malformed request — on the generate path (unlike
/// `test_key`) a 400 means a bad request, not a bad key, so it maps to `GenerationFailed`.
fn map_generate_status_error(status: u16) -> AiError {
    match status {
        401 | 403 => AiError::InvalidKey,
        429 => AiError::QuotaExceeded { free_tier: false },
        500..=599 => AiError::Unavailable(format!("status {status}")),
        other => AiError::GenerationFailed(format!("unexpected status {other}")),
    }
}

/// The `input` of the first `tool_use` block named `tool`, plus token counts. Scans for it rather
/// than indexing `content[0]`: a text block can precede it even with `tool_choice` pinned. Pure, so
/// the defensive-parsing paths are unit-testable without a live or mocked request.
fn tool_input(
    text_body: &str,
    tool: &str,
) -> Result<(serde_json::Value, Option<i64>, Option<i64>), AiError> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        content: Vec<ContentBlock>,
        usage: Option<Usage>,
    }
    #[derive(Deserialize)]
    struct ContentBlock {
        #[serde(rename = "type")]
        block_type: String,
        name: Option<String>,
        input: Option<serde_json::Value>,
    }
    #[derive(Deserialize)]
    struct Usage {
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
    }

    let parsed: Envelope = serde_json::from_str(text_body).map_err(|e| {
        tracing::warn!(error = %e, raw_len = text_body.len(), tool, "anthropic response envelope did not match expected shape");
        AiError::GenerationFailed(e.to_string())
    })?;
    let content_empty = parsed.content.is_empty();
    let Some(input) = parsed
        .content
        .into_iter()
        .find(|b| b.block_type == "tool_use" && b.name.as_deref() == Some(tool))
        .and_then(|b| b.input)
    else {
        if content_empty {
            tracing::warn!(tool, "anthropic returned no content blocks");
        } else {
            tracing::warn!(tool, "anthropic returned no matching tool_use block");
        }
        return Err(AiError::GenerationFailed(format!(
            "no {tool} tool_use block"
        )));
    };
    let (tokens_in, tokens_out) = parsed
        .usage
        .map_or((None, None), |u| (u.input_tokens, u.output_tokens));
    Ok((input, tokens_in, tokens_out))
}

fn parse_anthropic_body(text_body: &str) -> Result<SqlGeneration, AiError> {
    let (input, tokens_in, tokens_out) = tool_input(text_body, "emit_sql")?;
    let gen = serde_json::from_value::<SqlGeneration>(input).map_err(|e| {
        tracing::warn!(error = %e, "anthropic's tool_use input did not match the expected {{sql, explanation}} shape");
        AiError::GenerationFailed(e.to_string())
    })?;
    Ok(SqlGeneration {
        tokens_in,
        tokens_out,
        ..gen
    })
}

fn parse_anthropic_suggest_body(text_body: &str) -> Result<MonitorSuggestionGeneration, AiError> {
    let (root, tokens_in, tokens_out) = tool_input(text_body, "emit_monitor_suggestions")?;
    Ok(MonitorSuggestionGeneration {
        suggestions: parse_suggestions_json(&root),
        tokens_in,
        tokens_out,
    })
}

/// Pure HTTP-status -> `AiError` mapping for the `test_key` probe.
/// Unlike generation, a 400 here means a bad key.
fn map_test_key_status(status: u16) -> Result<(), AiError> {
    match status {
        200 => Ok(()),
        400 => Err(AiError::InvalidKey),
        _ => Err(map_generate_status_error(status)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_timeouts_are_sized_per_call() {
        assert_eq!(SQL_TIMEOUT, Duration::from_secs(30));
        assert_eq!(VIZ_TIMEOUT, Duration::from_secs(45));
        assert_eq!(SUGGEST_TIMEOUT, Duration::from_secs(90));
        assert_eq!(DESCRIBE_TIMEOUT, Duration::from_secs(90));
    }

    #[test]
    fn tool_choice_is_auto_for_5_5_models_and_pinned_otherwise() {
        assert_eq!(
            tool_choice("claude-sonnet-5-5", "t"),
            serde_json::json!({"type":"auto"})
        );
        assert_eq!(
            tool_choice("claude-opus-5-5", "t"),
            serde_json::json!({"type":"auto"})
        );
        assert_eq!(
            tool_choice("claude-sonnet-5", "t"),
            serde_json::json!({"type":"tool","name":"t"})
        );
        assert_eq!(
            describe_body("claude-sonnet-5-5", "p")["tool_choice"],
            serde_json::json!({"type":"auto"})
        );
    }

    #[test]
    fn describe_body_shape() {
        let b = describe_body("m", "the prompt");
        assert_eq!(b["max_tokens"], 8192);
        assert_eq!(
            b["tool_choice"],
            serde_json::json!({"type":"tool","name":"emit_schema_descriptions"})
        );
        assert_eq!(b["tools"][0]["input_schema"], response_schema(false));
        assert_eq!(b["messages"][0]["content"], "the prompt");
    }

    #[test]
    fn describe_body_parses_tool_use_with_usage() {
        let body = serde_json::json!({
            "content": [
                {"type":"text","text":"x"},
                {"type":"tool_use","name":"emit_schema_descriptions","input":{"tables":[
                    {"table":"orders","columns":[{"column":"status","description":"state"}]}
                ]}}
            ],
            "usage": {"input_tokens": 11, "output_tokens": 7}
        })
        .to_string();
        let d = parse_anthropic_describe_body(&body).unwrap();
        assert_eq!(d.tables.len(), 1);
        assert_eq!(d.tables[0].columns[0].description, "state");
        assert_eq!((d.tokens_in, d.tokens_out), (Some(11), Some(7)));
    }

    #[test]
    fn describe_body_without_tool_block_fails() {
        let body = serde_json::json!({"content":[{"type":"text","text":"hi"}]}).to_string();
        assert!(matches!(
            parse_anthropic_describe_body(&body),
            Err(AiError::GenerationFailed(_))
        ));
    }

    #[test]
    fn generate_status_error_mapping() {
        assert!(matches!(
            map_generate_status_error(401),
            AiError::InvalidKey
        ));
        assert!(matches!(
            map_generate_status_error(403),
            AiError::InvalidKey
        ));
        assert!(matches!(
            map_generate_status_error(429),
            AiError::QuotaExceeded { .. }
        ));
        // Unlike the test_key path, a 400 on generate means a bad request, not a bad key.
        assert!(matches!(
            map_generate_status_error(400),
            AiError::GenerationFailed(_)
        ));
        assert!(matches!(
            map_generate_status_error(503),
            AiError::Unavailable(_)
        ));
        assert!(matches!(
            map_generate_status_error(529),
            AiError::Unavailable(_)
        ));
        assert!(matches!(
            map_generate_status_error(404),
            AiError::GenerationFailed(_)
        ));
    }

    #[test]
    fn parse_body_success() {
        let body = serde_json::json!({
            "content": [
                { "type": "text", "text": "Here you go" },
                {
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "emit_sql",
                    "input": { "sql": "SELECT 1", "explanation": "trivial" }
                }
            ]
        })
        .to_string();
        let gen = parse_anthropic_body(&body).unwrap();
        assert_eq!(gen.sql, "SELECT 1");
        assert_eq!(gen.explanation, Some("trivial".to_string()));
    }

    #[test]
    fn parse_body_success_without_explanation() {
        let body = serde_json::json!({
            "content": [
                {
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "emit_sql",
                    "input": { "sql": "SELECT 1" }
                }
            ]
        })
        .to_string();
        let gen = parse_anthropic_body(&body).unwrap();
        assert_eq!(gen.sql, "SELECT 1");
        assert_eq!(gen.explanation, None);
    }

    #[test]
    fn parse_body_empty_content_fails() {
        let body = serde_json::json!({ "content": [] }).to_string();
        assert!(matches!(
            parse_anthropic_body(&body),
            Err(AiError::GenerationFailed(_))
        ));
    }

    #[test]
    fn parse_body_missing_tool_use_fails() {
        let body = serde_json::json!({
            "content": [{ "type": "text", "text": "I refuse to use the tool" }]
        })
        .to_string();
        assert!(matches!(
            parse_anthropic_body(&body),
            Err(AiError::GenerationFailed(_))
        ));
    }

    #[test]
    fn parse_body_wrong_tool_name_fails() {
        let body = serde_json::json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "some_other_tool",
                "input": { "sql": "SELECT 1" }
            }]
        })
        .to_string();
        assert!(matches!(
            parse_anthropic_body(&body),
            Err(AiError::GenerationFailed(_))
        ));
    }

    #[test]
    fn parse_body_malformed_input_fails() {
        let body = serde_json::json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "emit_sql",
                "input": { "not_sql": "oops" }
            }]
        })
        .to_string();
        assert!(matches!(
            parse_anthropic_body(&body),
            Err(AiError::GenerationFailed(_))
        ));
    }

    #[test]
    fn parse_body_malformed_envelope_fails() {
        assert!(matches!(
            parse_anthropic_body("not json"),
            Err(AiError::GenerationFailed(_))
        ));
    }

    #[test]
    fn parse_suggest_body_success() {
        let body = serde_json::json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "emit_monitor_suggestions",
                "input": {
                    "suggestions": [{
                        "table": "orders", "kind": "row_count", "target_column": "",
                        "title": "Orders table goes empty", "rationale": "r",
                        "sql": "SELECT COUNT(*) AS row_count FROM orders",
                        "column": "row_count", "op": "<", "value": 1.0
                    }]
                }
            }]
        })
        .to_string();
        let gen = parse_anthropic_suggest_body(&body).unwrap();
        assert_eq!(gen.suggestions.len(), 1);
        assert_eq!(gen.suggestions[0].table, "orders");
    }

    #[test]
    fn parse_suggest_body_wrong_tool_name_fails() {
        let body = serde_json::json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "some_other_tool",
                "input": { "suggestions": [] }
            }]
        })
        .to_string();
        assert!(matches!(
            parse_anthropic_suggest_body(&body),
            Err(AiError::GenerationFailed(_))
        ));
    }

    #[test]
    fn test_key_5xx_is_unavailable() {
        assert!(matches!(
            map_test_key_status(503),
            Err(AiError::Unavailable(_))
        ));
        assert!(matches!(map_test_key_status(401), Err(AiError::InvalidKey)));
        assert!(matches!(
            map_test_key_status(429),
            Err(AiError::QuotaExceeded { .. })
        ));
        assert!(matches!(
            map_test_key_status(404),
            Err(AiError::GenerationFailed(_))
        ));
    }

    #[tokio::test]
    async fn generate_json_returns_object_and_tokens() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"content":[{"type":"text","text":"ok"},
                {"type":"tool_use","name":"emit_json","input":{"a":1}}],
                "usage":{"input_tokens":7,"output_tokens":3}}"#,
            ))
            .mount(&server)
            .await;
        let g = call_anthropic_json(
            &reqwest::Client::new(),
            &server.uri(),
            "m",
            "k",
            "p",
            &serde_json::json!({"type":"object"}),
        )
        .await
        .unwrap();
        assert_eq!(g.value, serde_json::json!({"a":1}));
        assert_eq!((g.tokens_in, g.tokens_out), (Some(7), Some(3)));
    }

    #[tokio::test]
    async fn generate_json_maps_401_to_invalid_key() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let r = call_anthropic_json(
            &reqwest::Client::new(),
            &server.uri(),
            "m",
            "k",
            "p",
            &serde_json::json!({}),
        )
        .await;
        assert!(matches!(r, Err(AiError::InvalidKey)));
    }
}

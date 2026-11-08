//! `OpenAiProvider` — the `AiProvider` mapping for `OpenAI`'s `/v1/chat/completions` API.
//!
//! The same HTTP shape (bearer auth, `POST {base_url}/v1/...`) is what any OpenAI-compatible API
//! (Mistral, xAI, `OpenRouter`, …) speaks too, which is why `base_url` is an org-configurable
//! override rather than a hardcoded constant — pointing it elsewhere makes this provider double as
//! the client for those services. Structured output via `response_format: json_schema` (`strict:
//! true`) guarantees parseable JSON in `choices[0].message.content` — same "no
//! markdown-fence-stripping" property Gemini's `responseSchema` gives us, just `OpenAI`'s own
//! mechanism for it.

use std::time::Duration;

use serde::de::DeserializeOwned;
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

const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com";
const SQL_TIMEOUT: Duration = Duration::from_secs(30);
const VIZ_TIMEOUT: Duration = Duration::from_secs(45);
const SUGGEST_TIMEOUT: Duration = Duration::from_secs(90);
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(90);

pub struct OpenAiProvider {
    base_url: Option<String>,
}

impl OpenAiProvider {
    #[must_use]
    pub const fn new(base_url: Option<String>) -> Self {
        Self { base_url }
    }

    fn base_url(&self) -> &str {
        self.base_url.as_deref().unwrap_or(OPENAI_DEFAULT_BASE_URL)
    }

    /// The endpoint override is org-supplied, so each call re-checks it and pins the connection to
    /// the addresses just approved; the default `OpenAI` host needs neither and reuses the shared
    /// client.
    async fn client_for(&self, shared: &reqwest::Client) -> Result<reqwest::Client, AiError> {
        let Some(url) = self.base_url.as_deref() else {
            return Ok(shared.clone());
        };
        let (host, addrs) = crate::connectors::http_connectors::guard_url(url)
            .await
            .map_err(|e| AiError::Network(e.to_string()))?;
        crate::connectors::http_connectors::pinned_http_client(&host, &addrs)
            .map_err(|e| AiError::Network(e.to_string()))
    }

    /// One strict structured-output call on the (re-checked) endpoint; returns the 200 body.
    #[allow(clippy::too_many_arguments)]
    async fn chat(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
        name: &str,
        schema: &serde_json::Value,
        timeout: Duration,
    ) -> Result<String, AiError> {
        let http = self.client_for(http).await?;
        let resp = post_chat(
            &http,
            self.base_url(),
            api_key,
            model,
            prompt,
            name,
            schema,
            timeout,
        )
        .await?;
        read_capped(resp).await.inspect_err(|e| {
            tracing::warn!(error = ?e, schema = name, "failed to read openai response body");
        })
    }
}

#[async_trait::async_trait]
impl AiProvider for OpenAiProvider {
    fn kind(&self) -> AiProviderKind {
        AiProviderKind::OpenAi
    }

    fn default_model(&self) -> &'static str {
        "gpt-5.6-terra"
    }

    fn curated_models(&self) -> &'static [&'static str] {
        &["gpt-5.6-luna", "gpt-5.6-terra", "gpt-5.6-sol"]
    }

    async fn test_key(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
    ) -> Result<(), AiError> {
        let http = self.client_for(http).await?;
        // Per-model retrieve, not the bare list — validates the key AND that it can reach the
        // org's actually-configured model, at the same free (no generation tokens) cost.
        let url = format!("{}/v1/models/{model}", self.base_url());
        let resp = http
            .get(&url)
            .timeout(Duration::from_secs(10))
            .bearer_auth(api_key)
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
                "explanation": { "type": ["string", "null"] }
            },
            "required": ["sql", "explanation"],
            "additionalProperties": false
        });
        let body = self
            .chat(
                http,
                api_key,
                model,
                prompt,
                "sql_generation",
                &schema,
                SQL_TIMEOUT,
            )
            .await?;
        parse_openai_body(&body)
    }

    async fn suggest_visualization(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<VizGeneration, AiError> {
        let body = self
            .chat(
                http,
                api_key,
                model,
                prompt,
                "auto_visualize",
                &viz_schema(true),
                VIZ_TIMEOUT,
            )
            .await?;
        parse_openai_viz_body(&body)
    }

    async fn suggest_monitors(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<MonitorSuggestionGeneration, AiError> {
        let schema = suggest_schema(true);
        let body = self
            .chat(
                http,
                api_key,
                model,
                prompt,
                "monitor_suggestions",
                &schema,
                SUGGEST_TIMEOUT,
            )
            .await?;
        parse_openai_suggest_body(&body)
    }

    async fn describe_schema(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<SchemaDescriptions, AiError> {
        let schema = response_schema(true);
        let body = self
            .chat(
                http,
                api_key,
                model,
                prompt,
                "schema_descriptions",
                &schema,
                DESCRIBE_TIMEOUT,
            )
            .await?;
        let g = parse_openai_json_body(&body)?;
        Ok(SchemaDescriptions {
            tables: parse_descriptions_json(&g.value),
            tokens_in: g.tokens_in,
            tokens_out: g.tokens_out,
        })
    }

    async fn generate_json(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
        schema: &serde_json::Value,
    ) -> Result<JsonGeneration, AiError> {
        let http = self.client_for(http).await?;
        call_openai_json(&http, self.base_url(), model, api_key, prompt, schema).await
    }
}

/// POSTs a `chat/completions` request with a strict `json_schema` response format named `name`.
/// Returns the response if it is a 200, else the mapped status error.
#[allow(clippy::too_many_arguments)]
async fn post_chat(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    prompt: &str,
    name: &str,
    schema: &serde_json::Value,
    timeout: Duration,
) -> Result<reqwest::Response, AiError> {
    tracing::debug!(%model, prompt_len = prompt.len(), schema = name, "calling openai chat/completions");
    let body = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "response_format": {
            "type": "json_schema",
            "json_schema": { "name": name, "strict": true, "schema": schema }
        }
    });
    let resp = http
        .post(format!("{base_url}/v1/chat/completions"))
        .timeout(timeout)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            let redacted = redact(e);
            tracing::warn!(error = %redacted, schema = name, "openai chat/completions request failed");
            AiError::Network(redacted)
        })?;
    let status = resp.status().as_u16();
    if status == 200 {
        return Ok(resp);
    }
    let body = log_snippet(&read_capped(resp).await.unwrap_or_default());
    tracing::warn!(%status, %body, schema = name, "openai chat/completions returned unexpected status");
    Err(map_generate_status_error(status))
}

async fn call_openai_json(
    http: &reqwest::Client,
    base_url: &str,
    model: &str,
    api_key: &str,
    prompt: &str,
    schema: &serde_json::Value,
) -> Result<JsonGeneration, AiError> {
    let resp = post_chat(
        http,
        base_url,
        api_key,
        model,
        prompt,
        "result",
        schema,
        SUGGEST_TIMEOUT,
    )
    .await?;
    parse_openai_json_body(&read_capped(resp).await.unwrap_or_default())
}

fn parse_openai_json_body(text_body: &str) -> Result<JsonGeneration, AiError> {
    let envelope: serde_json::Value =
        serde_json::from_str(text_body).map_err(|e| AiError::GenerationFailed(e.to_string()))?;
    let content = envelope["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| AiError::GenerationFailed("no choices".to_string()))?;
    Ok(JsonGeneration {
        value: parse_inner(content, "json")?,
        tokens_in: envelope["usage"]["prompt_tokens"].as_i64(),
        tokens_out: envelope["usage"]["completion_tokens"].as_i64(),
    })
}

/// Pure HTTP-status → outcome mapping for `test_key`'s `GET /v1/models/{model}` call: 200 valid,
/// otherwise the same mapping as generation (a 404 for a model the key can't reach is an
/// unexpected status).
fn map_test_key_status(status: u16) -> Result<(), AiError> {
    match status {
        200 => Ok(()),
        _ => Err(map_generate_status_error(status)),
    }
}

/// Pure HTTP-status → `AiError` mapping for non-200 responses.
fn map_generate_status_error(status: u16) -> AiError {
    match status {
        400 | 401 | 403 => AiError::InvalidKey,
        429 => AiError::QuotaExceeded { free_tier: false },
        500..=599 => AiError::Unavailable(format!("status {status}")),
        other => AiError::GenerationFailed(format!("unexpected status {other}")),
    }
}

/// The first choice's message content and token counts from a `chat/completions` envelope. Pure,
/// so the defensive-parsing paths are unit-testable without a live or mocked request.
fn parse_content(text_body: &str) -> Result<(String, Option<i64>, Option<i64>), AiError> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        choices: Vec<Choice>,
        usage: Option<Usage>,
    }
    #[derive(Deserialize)]
    struct Choice {
        message: Message,
    }
    #[derive(Deserialize)]
    struct Message {
        content: Option<String>,
    }
    #[derive(Deserialize)]
    struct Usage {
        prompt_tokens: Option<i64>,
        completion_tokens: Option<i64>,
    }

    let parsed: Envelope = serde_json::from_str(text_body).map_err(|e| {
        tracing::warn!(error = %e, raw_len = text_body.len(), "openai response envelope did not match expected shape");
        AiError::GenerationFailed(e.to_string())
    })?;
    let choices_empty = parsed.choices.is_empty();
    let Some(content) = parsed
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
    else {
        if choices_empty {
            tracing::warn!("openai returned no choices");
        } else {
            tracing::warn!("openai returned a choice with no message content");
        }
        return Err(AiError::GenerationFailed("no choices".to_string()));
    };
    let (tokens_in, tokens_out) = parsed
        .usage
        .map_or((None, None), |u| (u.prompt_tokens, u.completion_tokens));
    Ok((content, tokens_in, tokens_out))
}

fn parse_inner<T: DeserializeOwned>(content: &str, what: &str) -> Result<T, AiError> {
    serde_json::from_str(content).map_err(|e| {
        tracing::warn!(error = %e, raw_len = content.len(), "openai's inner JSON ({what}) did not match the expected shape");
        AiError::GenerationFailed(e.to_string())
    })
}

fn parse_openai_body(text_body: &str) -> Result<SqlGeneration, AiError> {
    let (content, tokens_in, tokens_out) = parse_content(text_body)?;
    Ok(SqlGeneration {
        tokens_in,
        tokens_out,
        ..parse_inner(&content, "sql")?
    })
}

fn parse_openai_viz_body(text_body: &str) -> Result<VizGeneration, AiError> {
    let (content, tokens_in, tokens_out) = parse_content(text_body)?;
    let (title, suggestion) = project_flat_viz(&parse_inner(&content, "viz")?);
    Ok(VizGeneration {
        title,
        suggestion,
        tokens_in,
        tokens_out,
    })
}

fn parse_openai_suggest_body(text_body: &str) -> Result<MonitorSuggestionGeneration, AiError> {
    let (content, tokens_in, tokens_out) = parse_content(text_body)?;
    Ok(MonitorSuggestionGeneration {
        suggestions: parse_suggestions_json(&parse_inner(&content, "monitor suggestions")?),
        tokens_in,
        tokens_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::monitor_suggestions::{SuggestionFieldKind, SUGGESTION_FIELDS};
    use crate::ai::viz::{VIZ_FIELDS, VIZ_TYPE_ENUM};

    #[test]
    fn request_timeouts_are_sized_per_call() {
        assert_eq!(SQL_TIMEOUT, Duration::from_secs(30));
        assert_eq!(VIZ_TIMEOUT, Duration::from_secs(45));
        assert_eq!(SUGGEST_TIMEOUT, Duration::from_secs(90));
        assert_eq!(DESCRIBE_TIMEOUT, Duration::from_secs(90));
    }

    #[test]
    fn base_url_defaults_when_unset() {
        assert_eq!(
            OpenAiProvider::new(None).base_url(),
            OPENAI_DEFAULT_BASE_URL
        );
    }

    #[test]
    fn base_url_uses_override_when_set() {
        let provider = OpenAiProvider::new(Some("https://api.mistral.ai".to_string()));
        assert_eq!(provider.base_url(), "https://api.mistral.ai");
    }

    #[test]
    fn test_key_status_mapping() {
        assert!(map_test_key_status(200).is_ok());
        assert!(matches!(map_test_key_status(400), Err(AiError::InvalidKey)));
        assert!(matches!(map_test_key_status(401), Err(AiError::InvalidKey)));
        assert!(matches!(map_test_key_status(403), Err(AiError::InvalidKey)));
        assert!(matches!(
            map_test_key_status(429),
            Err(AiError::QuotaExceeded { .. })
        ));
        assert!(matches!(
            map_test_key_status(500),
            Err(AiError::Unavailable(_))
        ));
        assert!(matches!(
            map_test_key_status(404),
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
            map_generate_status_error(429),
            AiError::QuotaExceeded { .. }
        ));
        assert!(matches!(
            map_generate_status_error(503),
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
            "choices": [{
                "message": { "content": "{\"sql\": \"SELECT 1\", \"explanation\": \"trivial\"}" }
            }]
        })
        .to_string();
        let gen = parse_openai_body(&body).unwrap();
        assert_eq!(gen.sql, "SELECT 1");
        assert_eq!(gen.explanation, Some("trivial".to_string()));
    }

    #[test]
    fn parse_body_success_without_explanation() {
        let body = serde_json::json!({
            "choices": [{
                "message": { "content": "{\"sql\": \"SELECT 1\"}" }
            }]
        })
        .to_string();
        let gen = parse_openai_body(&body).unwrap();
        assert_eq!(gen.sql, "SELECT 1");
        assert_eq!(gen.explanation, None);
    }

    #[test]
    fn parse_body_empty_choices_fails() {
        let body = serde_json::json!({ "choices": [] }).to_string();
        assert!(matches!(
            parse_openai_body(&body),
            Err(AiError::GenerationFailed(_))
        ));
    }

    #[test]
    fn parse_body_missing_content_fails() {
        let body = serde_json::json!({ "choices": [{ "message": {} }] }).to_string();
        assert!(matches!(
            parse_openai_body(&body),
            Err(AiError::GenerationFailed(_))
        ));
    }

    #[test]
    fn parse_body_malformed_envelope_fails() {
        assert!(matches!(
            parse_openai_body("not json"),
            Err(AiError::GenerationFailed(_))
        ));
    }

    #[test]
    fn parse_body_malformed_inner_json_fails() {
        let body = serde_json::json!({
            "choices": [{ "message": { "content": "not valid json" } }]
        })
        .to_string();
        assert!(matches!(
            parse_openai_body(&body),
            Err(AiError::GenerationFailed(_))
        ));
    }

    /// OpenAI strict mode rejects a schema where any `properties` key is missing from
    /// `required`, or where an "optional" field is anything other than a nullable type union —
    /// both invisible to `cargo check` and only caught live, so pin them down here instead.
    #[test]
    fn openai_viz_schema_strict_mode_invariants() {
        let schema = viz_schema(true);
        let properties = schema["properties"].as_object().unwrap();
        let required: std::collections::HashSet<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let prop_keys: std::collections::HashSet<&str> =
            properties.keys().map(String::as_str).collect();

        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            prop_keys, required,
            "every property must also be required under OpenAI strict mode"
        );
        assert_eq!(
            properties.len(),
            VIZ_FIELDS.len() + 2,
            "property count must be every shared viz field plus viz_type and title"
        );

        let viz_type = &properties["viz_type"];
        assert_eq!(viz_type["type"], "string");
        let viz_type_enum: Vec<&str> = viz_type["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(viz_type_enum, VIZ_TYPE_ENUM);

        for (key, value) in properties {
            if key == "viz_type" {
                continue;
            }
            let ty = value["type"]
                .as_array()
                .unwrap_or_else(|| panic!("{key} must use a nullable type union"));
            assert_eq!(ty.len(), 2, "{key} type union must have exactly 2 members");
            assert!(
                ty.contains(&serde_json::json!("null")),
                "{key} type union must include null"
            );
            if key == "yColumns" || key == "tableHiddenColumns" {
                assert!(
                    ty.contains(&serde_json::json!("array")),
                    "{key} must be an array|null union"
                );
                assert_eq!(value["items"]["type"], "string");
            }
        }
    }

    /// Unlike the viz schema, every suggestion field is unconditionally required with a plain
    /// (non-nullable) type — "absent" is expressed as `""`, never a missing key or `null`.
    /// Both levels (`suggestions` item schema and its root wrapper) need `additionalProperties:
    /// false` under strict mode.
    #[test]
    fn openai_suggest_schema_strict_mode_invariants() {
        let schema = suggest_schema(true);
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], serde_json::json!(["suggestions"]));

        let item_schema = &schema["properties"]["suggestions"]["items"];
        assert_eq!(item_schema["additionalProperties"], false);
        let properties = item_schema["properties"].as_object().unwrap();
        let required: std::collections::HashSet<&str> = item_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let prop_keys: std::collections::HashSet<&str> =
            properties.keys().map(String::as_str).collect();
        assert_eq!(
            prop_keys, required,
            "every suggestion property must also be required under strict mode"
        );
        assert_eq!(properties.len(), SUGGESTION_FIELDS.len());
        for (name, kind) in SUGGESTION_FIELDS {
            let ty = properties[*name]["type"].as_str().unwrap();
            match kind {
                SuggestionFieldKind::Str => assert_eq!(ty, "string"),
                SuggestionFieldKind::Num => assert_eq!(ty, "number"),
            }
        }
    }

    #[test]
    fn parse_suggest_body_success() {
        let body = serde_json::json!({
            "choices": [{
                "message": { "content": serde_json::json!({
                    "suggestions": [{
                        "table": "orders", "kind": "row_count", "target_column": "",
                        "title": "Orders table goes empty", "rationale": "r",
                        "sql": "SELECT COUNT(*) AS row_count FROM orders",
                        "column": "row_count", "op": "<", "value": 1.0
                    }]
                }).to_string() }
            }]
        })
        .to_string();
        let gen = parse_openai_suggest_body(&body).unwrap();
        assert_eq!(gen.suggestions.len(), 1);
        assert_eq!(gen.suggestions[0].table, "orders");
    }

    #[test]
    fn parse_suggest_body_empty_choices_fails() {
        let body = serde_json::json!({ "choices": [] }).to_string();
        assert!(matches!(
            parse_openai_suggest_body(&body),
            Err(AiError::GenerationFailed(_))
        ));
    }

    #[tokio::test]
    async fn generate_json_returns_object_and_tokens() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"choices":[{"message":{"content":"{\"a\":1}"}}],
                "usage":{"prompt_tokens":7,"completion_tokens":3}}"#,
            ))
            .mount(&server)
            .await;
        let g = call_openai_json(
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
        let r = call_openai_json(
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

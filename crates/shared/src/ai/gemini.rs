//! `GeminiProvider`: the `AiProvider` mapping for Gemini.
//!
//! `generateContent` with `responseSchema` guarantees parseable JSON in the candidate's text parts.
//! Key testing uses the free `GET /v1beta/models/{model}` endpoint, which validates the key and
//! that it can reach the configured model.

use std::time::{Duration, Instant};

use super::describe::{parse_descriptions_json, response_schema, SchemaDescriptions};
use super::monitor_suggestions::{
    parse_suggestions_json, suggest_schema, MonitorSuggestionGeneration,
};
use super::viz::{project_flat_viz, viz_schema, VizGeneration};
use super::{
    log_snippet, read_capped, redact, AiError, AiProvider, AiProviderKind, JsonGeneration,
    SqlGeneration,
};

const GEMINI_GENERATE_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";

const SQL_TIMEOUT: Duration = Duration::from_secs(60);
const VIZ_TIMEOUT: Duration = Duration::from_secs(45);
// Whole-schema outputs are large; latency varies 2s-25s+ on the same prompt.
const SUGGEST_TIMEOUT: Duration = Duration::from_secs(90);
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(90);
// Per-attempt caps: one hung request must not consume the whole call budget.
const SQL_ATTEMPT_CAP: Duration = Duration::from_secs(20);
const VIZ_ATTEMPT_CAP: Duration = Duration::from_secs(20);
const LONG_ATTEMPT_CAP: Duration = Duration::from_secs(40);
/// An attempt is only started with at least this much budget left.
const MIN_ATTEMPT: Duration = Duration::from_secs(10);
const RETRY_BACKOFFS: [Duration; 2] = [Duration::from_millis(1000), Duration::from_millis(2500)];
/// Overload fallback order, newest first.
const FALLBACK_LADDER: [&str; 4] = [
    "gemini-3.8-flash",
    "gemini-3.7-flash",
    "gemini-3.6-flash",
    "gemini-3.5-flash",
];
/// Extra models tried per call once the configured one stays overloaded.
const MAX_FALLBACK_MODELS: usize = 2;
const RETRY_AFTER_CAP: Duration = Duration::from_secs(5);
/// Longest per-minute rate-limit wait worth sitting out on the same model.
const RATE_LIMIT_WAIT_CAP: Duration = Duration::from_secs(20);
const DEFAULT_RATE_LIMIT_SECS: u64 = 60;

/// Time limits for one logical call: `budget` bounds all attempts and models, `cap` a single
/// attempt, `min_attempt` the least budget worth starting one with.
#[derive(Clone, Copy)]
struct Limits {
    budget: Duration,
    cap: Duration,
    min_attempt: Duration,
}

impl Limits {
    const fn new(budget: Duration, cap: Duration) -> Self {
        Self {
            budget,
            cap,
            min_attempt: MIN_ATTEMPT,
        }
    }

    /// Timeout for the next attempt: what is left of the budget, capped per attempt, and only if at
    /// least `min_attempt` remains.
    fn attempt_timeout(self, elapsed: Duration) -> Option<Duration> {
        let left = self.budget.saturating_sub(elapsed);
        (left >= self.min_attempt).then(|| left.min(self.cap))
    }
}

pub struct GeminiProvider;

#[async_trait::async_trait]
impl AiProvider for GeminiProvider {
    fn kind(&self) -> AiProviderKind {
        AiProviderKind::Gemini
    }

    fn default_model(&self) -> &'static str {
        FALLBACK_LADDER[0]
    }

    fn curated_models(&self) -> &'static [&'static str] {
        &[
            "gemini-3.8-flash",
            "gemini-3.7-flash",
            "gemini-3.6-flash",
            "gemini-3.5-flash",
            "gemini-3.1-flash-lite",
            "gemini-3.1-pro-preview",
        ]
    }

    async fn test_key(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
    ) -> Result<(), AiError> {
        let url = format!("{GEMINI_GENERATE_BASE_URL}/{model}?key={api_key}");
        let resp = http
            .get(&url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| AiError::Network(redact(e)))?;

        let status = resp.status().as_u16();
        let retry_after = retry_after(&resp);
        let body = if status == 429 {
            resp.text().await.unwrap_or_default()
        } else {
            String::new()
        };
        map_test_key_status(status, &body, retry_after.as_deref())
    }

    async fn generate_sql(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<SqlGeneration, AiError> {
        let schema = serde_json::json!({
            "type": "OBJECT",
            "properties": {
                "sql": { "type": "STRING" },
                "explanation": { "type": "STRING" }
            },
            "required": ["sql"]
        });
        let limits = Limits::new(SQL_TIMEOUT, SQL_ATTEMPT_CAP);
        let (text, tokens_in, tokens_out) =
            generate_structured(http, model, api_key, prompt, schema, limits).await?;
        let gen: SqlGeneration = serde_json::from_str(&text).map_err(|e| {
            tracing::warn!(error = %e, raw_len = text.len(), "gemini's inner JSON did not match the expected {{sql, explanation}} shape");
            AiError::GenerationFailed(e.to_string())
        })?;
        Ok(SqlGeneration {
            tokens_in,
            tokens_out,
            ..gen
        })
    }

    async fn suggest_visualization(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<VizGeneration, AiError> {
        let schema = to_gemini_schema(&viz_schema(true));
        let limits = Limits::new(VIZ_TIMEOUT, VIZ_ATTEMPT_CAP);
        let (text, tokens_in, tokens_out) =
            generate_structured(http, model, api_key, prompt, schema, limits).await?;
        let (title, suggestion) = project_flat_viz(&parse_inner_json(&text, "viz")?);
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
        let schema = to_gemini_schema(&suggest_schema(true));
        let limits = Limits::new(SUGGEST_TIMEOUT, LONG_ATTEMPT_CAP);
        let (text, tokens_in, tokens_out) =
            generate_structured(http, model, api_key, prompt, schema, limits).await?;
        Ok(MonitorSuggestionGeneration {
            suggestions: parse_suggestions_json(&parse_inner_json(&text, "monitor suggestions")?),
            tokens_in,
            tokens_out,
        })
    }

    async fn describe_schema(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<SchemaDescriptions, AiError> {
        let schema = to_gemini_schema(&response_schema(true));
        let limits = Limits::new(DESCRIBE_TIMEOUT, LONG_ATTEMPT_CAP);
        let (text, tokens_in, tokens_out) =
            generate_structured(http, model, api_key, prompt, schema, limits).await?;
        Ok(SchemaDescriptions {
            tables: parse_descriptions_json(&parse_inner_json(&text, "describe schema")?),
            tokens_in,
            tokens_out,
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
        generate_json_at(
            http,
            GEMINI_GENERATE_BASE_URL,
            &RETRY_BACKOFFS,
            model,
            api_key,
            prompt,
            schema,
        )
        .await
    }
}

async fn generate_json_at(
    http: &reqwest::Client,
    base_url: &str,
    backoffs: &[Duration],
    model: &str,
    api_key: &str,
    prompt: &str,
    schema: &serde_json::Value,
) -> Result<JsonGeneration, AiError> {
    let (text, tokens_in, tokens_out) = generate_structured_at(
        http,
        base_url,
        backoffs,
        model,
        api_key,
        prompt,
        to_gemini_schema(schema),
        Limits::new(SUGGEST_TIMEOUT, LONG_ATTEMPT_CAP),
    )
    .await?;
    Ok(JsonGeneration {
        value: parse_inner_json(&text, "generate_json")?,
        tokens_in,
        tokens_out,
    })
}

/// Adapts an OpenAI-strict schema to Gemini's `responseSchema` dialect: drops
/// `additionalProperties`, uppercases types, turns `["t","null"]` into `t` + `nullable`.
fn to_gemini_schema(schema: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match schema {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                match (k.as_str(), v) {
                    ("additionalProperties", _) => {}
                    ("type", Value::String(t)) => {
                        out.insert(k.clone(), Value::String(t.to_uppercase()));
                    }
                    ("type", Value::Array(ts)) => {
                        let mut names = ts.iter().filter_map(Value::as_str);
                        let nullable = ts.iter().any(|t| t.as_str() == Some("null"));
                        if let Some(t) = names.find(|t| *t != "null") {
                            out.insert(k.clone(), Value::String(t.to_uppercase()));
                        }
                        if nullable {
                            out.insert("nullable".into(), Value::Bool(true));
                        }
                    }
                    _ => {
                        out.insert(k.clone(), to_gemini_schema(v));
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(to_gemini_schema).collect()),
        other => other.clone(),
    }
}

/// Delay before a retry: the capped `Retry-After` if sent, else the backoff plus up to 25% jitter.
fn retry_delay(base: Duration, retry_after: Option<&str>) -> Duration {
    if let Some(secs) = retry_after.and_then(|v| v.trim().parse::<u64>().ok()) {
        return Duration::from_secs(secs).min(RETRY_AFTER_CAP);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    base + base * (nanos % 250) / 1000
}

fn retry_after(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Splits a Gemini 429 into a daily/plan quota (`QuotaExceeded`, retrying is pointless) and a
/// per-minute rate limit (`RateLimited`, with the server's suggested wait).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "retry delay is filtered to finite and non-negative; saturating cast is intended"
)]
fn classify_429(body: &str, retry_after: Option<&str>) -> AiError {
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let details = json["error"]["details"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let violations = details
        .iter()
        .filter(|d| {
            d["@type"]
                .as_str()
                .is_some_and(|t| t.ends_with("google.rpc.QuotaFailure"))
        })
        .flat_map(|d| d["violations"].as_array().into_iter().flatten())
        .collect::<Vec<_>>();
    let any_contains = |needle: &str| {
        violations.iter().any(|v| {
            ["quotaId", "quotaMetric"]
                .iter()
                .any(|k| v[k].as_str().is_some_and(|s| s.contains(needle)))
        })
    };
    let free_tier = any_contains("FreeTier") || any_contains("free_tier");
    if any_contains("PerDay") {
        return AiError::QuotaExceeded { free_tier };
    }
    let from_body = details
        .iter()
        .filter(|d| {
            d["@type"]
                .as_str()
                .is_some_and(|t| t.ends_with("google.rpc.RetryInfo"))
        })
        .find_map(|d| d["retryDelay"].as_str())
        .and_then(|s| s.trim().strip_suffix('s')?.parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| s.ceil() as u64);
    let from_header = || retry_after.and_then(|v| v.trim().parse::<u64>().ok());
    AiError::RateLimited {
        free_tier,
        retry_after_secs: from_body
            .or_else(from_header)
            .unwrap_or(DEFAULT_RATE_LIMIT_SECS),
    }
}

/// POSTs a `generateContent` body and maps non-200 statuses. Retries transient failures (connect
/// errors, attempt timeouts, 5xx) after each delay in `backoffs` while budget remains; a short
/// per-minute 429 waits once, other 429s/4xx aren't retried. Persistent 5xx or timeouts end in
/// [`AiError::Unavailable`]. Returns the response and the attempt number that produced it.
#[allow(
    clippy::cast_possible_truncation,
    reason = "elapsed milliseconds for a log field fit u64"
)]
async fn post_generate(
    http: &reqwest::Client,
    model: &str,
    url: &str,
    body: &serde_json::Value,
    limits: Limits,
    backoffs: &[Duration],
) -> Result<(reqwest::Response, u32), AiError> {
    let start = Instant::now();
    let mut attempt = 0;
    let mut saw_transient = false;
    let mut rate_limit_retried = false;
    let Some(mut timeout) = limits.attempt_timeout(Duration::ZERO) else {
        return Err(AiError::Unavailable("no time budget left".into()));
    };
    // Next (timeout, delay) if attempt `attempt` may be retried.
    let plan_retry = |attempt: u32, retry_after: Option<&str>| {
        let base = *backoffs.get(attempt as usize - 1)?;
        let delay = retry_delay(base, retry_after);
        limits
            .attempt_timeout(start.elapsed() + delay)
            .map(|t| (t, delay))
    };
    loop {
        attempt += 1;
        let resp = match http.post(url).timeout(timeout).json(body).send().await {
            Ok(resp) => resp,
            Err(e) => {
                let timed_out = e.is_timeout();
                saw_transient |= timed_out;
                let next = plan_retry(attempt, None);
                let redacted = redact(e);
                tracing::warn!(
                    error = %redacted, %model, attempt, timed_out,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "gemini generateContent request failed"
                );
                if let Some((t, delay)) = next {
                    timeout = t;
                    tokio::time::sleep(delay).await;
                    continue;
                }
                if saw_transient {
                    return Err(AiError::Unavailable(redacted));
                }
                return Err(AiError::Network(redacted));
            }
        };
        let status = resp.status();
        return match status.as_u16() {
            200 => Ok((resp, attempt)),
            401 | 403 => Err(AiError::InvalidKey),
            400 => {
                let body = read_capped(resp).await.unwrap_or_default();
                if body.contains("API_KEY_INVALID") || body.contains("API key not valid") {
                    return Err(AiError::InvalidKey);
                }
                let truncated = log_snippet(&body);
                tracing::warn!(%truncated, %model, attempt, "gemini generateContent rejected the request");
                Err(AiError::GenerationFailed(format!(
                    "invalid request: {truncated}"
                )))
            }
            429 => {
                let retry_after = retry_after(&resp);
                let body = read_capped(resp).await.unwrap_or_default();
                tracing::warn!(
                    body = %log_snippet(&body), retry_after = ?retry_after, %model, attempt,
                    "gemini generateContent rate limited"
                );
                let err = classify_429(&body, retry_after.as_deref());
                if let AiError::RateLimited {
                    retry_after_secs, ..
                } = err
                {
                    let delay = Duration::from_secs(retry_after_secs);
                    if !rate_limit_retried && delay <= RATE_LIMIT_WAIT_CAP {
                        if let Some(t) = limits.attempt_timeout(start.elapsed() + delay) {
                            rate_limit_retried = true;
                            timeout = t;
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                    }
                }
                Err(err)
            }
            _ => {
                let retry_after = retry_after(&resp);
                let body = log_snippet(&read_capped(resp).await.unwrap_or_default());
                tracing::warn!(
                    %status, %body, %model, attempt, timed_out = false,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "gemini generateContent returned unexpected status"
                );
                if status.is_server_error() {
                    saw_transient = true;
                    if let Some((t, delay)) = plan_retry(attempt, retry_after.as_deref()) {
                        timeout = t;
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    Err(AiError::Unavailable(format!("status {status}")))
                } else {
                    Err(AiError::GenerationFailed(format!(
                        "unexpected status {status}"
                    )))
                }
            }
        };
    }
}

/// [`post_generate`] on `model`; while it stays overloaded or limited (quotas are per model), walks
/// [`FALLBACK_LADDER`] (skipping `model`, at most [`MAX_FALLBACK_MODELS`]) within the remaining
/// budget. Returns the response, attempt number and serving model.
#[allow(
    clippy::cast_possible_truncation,
    reason = "elapsed milliseconds for a log field fit u64"
)]
async fn post_with_model_fallback(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    body: &serde_json::Value,
    limits: Limits,
    backoffs: &[Duration],
) -> Result<(reqwest::Response, u32, String), AiError> {
    let start = Instant::now();
    let url = |m: &str| format!("{base_url}/{m}:generateContent?key={api_key}");
    let err = match post_generate(http, model, &url(model), body, limits, backoffs).await {
        Ok((resp, attempt)) => return Ok((resp, attempt, model.to_owned())),
        Err(
            e @ (AiError::Unavailable(_)
            | AiError::RateLimited { .. }
            | AiError::QuotaExceeded { .. }),
        ) => e,
        Err(e) => return Err(e),
    };
    let rungs = FALLBACK_LADDER
        .iter()
        .copied()
        .filter(|m| *m != model)
        .take(MAX_FALLBACK_MODELS);
    for backup in rungs {
        let Some(remaining) = limits.attempt_timeout(start.elapsed()) else {
            break;
        };
        tracing::warn!(
            primary_model = %model, fallback_model = %backup,
            elapsed_ms = start.elapsed().as_millis() as u64,
            "gemini model overloaded; trying fallback model"
        );
        if let Ok((resp, attempt)) = post_generate(
            http,
            backup,
            &url(backup),
            body,
            Limits {
                budget: remaining,
                ..limits
            },
            backoffs,
        )
        .await
        {
            return Ok((resp, attempt, backup.to_owned()));
        }
    }
    Err(err)
}

/// Text, prompt tokens, output tokens and finish reason.
type Envelope = (String, Option<i64>, Option<i64>, Option<String>);

/// Extracts the concatenated non-thought text parts, token counts and finish reason from a
/// `generateContent` envelope.
fn parse_envelope(text_body: &str) -> Result<Envelope, AiError> {
    let envelope: serde_json::Value = serde_json::from_str(text_body).map_err(|e| {
        tracing::warn!(error = %e, raw_len = text_body.len(), "gemini response envelope did not match expected shape");
        AiError::GenerationFailed(e.to_string())
    })?;
    let candidate = envelope.pointer("/candidates/0");
    let text: String = candidate
        .and_then(|c| c.pointer("/content/parts"))
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p.get("thought").and_then(sea_orm::JsonValue::as_bool) != Some(true))
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let finish = candidate.and_then(|c| c.get("finishReason"));
    if text.is_empty() {
        tracing::warn!(
            finish_reason = ?finish,
            prompt_feedback = ?envelope.get("promptFeedback"),
            "gemini returned no text (blocked prompt, empty parts, or token budget spent)"
        );
        return Err(AiError::GenerationFailed(format!(
            "no text (finishReason {finish:?})"
        )));
    }
    let count = |k: &str| {
        envelope
            .pointer(&format!("/usageMetadata/{k}"))
            .and_then(sea_orm::JsonValue::as_i64)
    };
    Ok((
        text,
        count("promptTokenCount"),
        count("candidatesTokenCount"),
        finish.and_then(|f| f.as_str()).map(str::to_owned),
    ))
}

fn parse_inner_json(text: &str, what: &str) -> Result<serde_json::Value, AiError> {
    serde_json::from_str(text).map_err(|e| {
        tracing::warn!(error = %e, raw_len = text.len(), "gemini's inner JSON ({what}) was not valid JSON");
        AiError::GenerationFailed(e.to_string())
    })
}

/// One structured-output call: builds the body, sends (with retry and model fallback) and
/// returns the envelope text plus token counts.
async fn generate_structured(
    http: &reqwest::Client,
    model: &str,
    api_key: &str,
    prompt: &str,
    schema: serde_json::Value,
    limits: Limits,
) -> Result<(String, Option<i64>, Option<i64>), AiError> {
    generate_structured_at(
        http,
        GEMINI_GENERATE_BASE_URL,
        &RETRY_BACKOFFS,
        model,
        api_key,
        prompt,
        schema,
        limits,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "elapsed milliseconds for a log field fit u64"
)]
async fn generate_structured_at(
    http: &reqwest::Client,
    base_url: &str,
    backoffs: &[Duration],
    model: &str,
    api_key: &str,
    prompt: &str,
    schema: serde_json::Value,
    limits: Limits,
) -> Result<(String, Option<i64>, Option<i64>), AiError> {
    tracing::debug!(%model, prompt_len = prompt.len(), "calling gemini generateContent");
    let mut body = serde_json::json!({
        "contents": [{ "parts": [{ "text": prompt }] }],
        "generationConfig": {
            "responseMimeType": "application/json",
            "responseSchema": schema
        }
    });
    // Low thinking cuts latency with equivalent output; only the 3.x family accepts the field.
    if model.starts_with("gemini-3") {
        body["generationConfig"]["thinkingConfig"] = serde_json::json!({ "thinkingLevel": "low" });
    }
    let start = Instant::now();
    let (resp, attempt, model) =
        post_with_model_fallback(http, base_url, api_key, model, &body, limits, backoffs).await?;
    let (text, tin, tout, finish) = parse_envelope(&read_capped(resp).await?)?;
    tracing::info!(
        %model, attempt, tokens_in = ?tin, tokens_out = ?tout, finish_reason = ?finish,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "gemini generateContent succeeded"
    );
    Ok((text, tin, tout))
}

/// Pure HTTP-status -> `AiError` mapping for the `test_key` probe.
fn map_test_key_status(status: u16, body: &str, retry_after: Option<&str>) -> Result<(), AiError> {
    match status {
        200 => Ok(()),
        400 | 401 | 403 => Err(AiError::InvalidKey),
        429 => Err(classify_429(body, retry_after)),
        500..=599 => Err(AiError::Unavailable(format!("status {status}"))),
        other => Err(AiError::GenerationFailed(format!(
            "unexpected status {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_skips_thought_and_textless_parts() {
        let body = r#"{"candidates":[{"content":{"parts":[
            {"thought":true,"text":"reasoning"},
            {"functionCall":{"name":"x","args":{}}},
            {"text":"{\"a\":"},{"text":"1}"}]}}],
            "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}}"#;
        let (text, tin, tout, finish) = parse_envelope(body).unwrap();
        assert_eq!(finish, None);
        assert_eq!(text, r#"{"a":1}"#);
        assert_eq!((tin, tout), (Some(7), Some(3)));
    }

    #[test]
    fn envelope_without_text_is_generation_failed() {
        let body = r#"{"candidates":[{"finishReason":"MAX_TOKENS","content":{"parts":[{"thought":true}]}}]}"#;
        assert!(matches!(
            parse_envelope(body),
            Err(AiError::GenerationFailed(m)) if m.contains("MAX_TOKENS")
        ));
    }

    #[test]
    fn envelope_reports_finish_reason() {
        let body =
            r#"{"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"{}"}]}}]}"#;
        assert_eq!(parse_envelope(body).unwrap().3.as_deref(), Some("STOP"));
    }

    #[test]
    fn attempt_timeout_caps_and_requires_min_remaining() {
        let s = Duration::from_secs;
        let limits = Limits {
            budget: s(60),
            cap: s(20),
            min_attempt: s(10),
        };
        let at = |e| limits.attempt_timeout(s(e));
        assert_eq!(at(0), Some(s(20)));
        assert_eq!(at(45), Some(s(15)));
        assert_eq!(at(52), None);
        assert_eq!(at(100), None);
    }

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const FAST: [Duration; 2] = [Duration::from_millis(5), Duration::from_millis(5)];
    const OK_BODY: &str = r#"{"candidates":[{"content":{"parts":[{"text":"{}"}]}}]}"#;

    async fn run(server: &MockServer, model: &str, budget: Duration) -> Result<String, AiError> {
        run_with(
            &reqwest::Client::new(),
            server,
            model,
            Limits {
                budget,
                cap: budget,
                min_attempt: budget / 2,
            },
        )
        .await
    }

    async fn run_with(
        http: &reqwest::Client,
        server: &MockServer,
        model: &str,
        limits: Limits,
    ) -> Result<String, AiError> {
        generate_structured_at(
            http,
            &server.uri(),
            &FAST,
            model,
            "k",
            "p",
            serde_json::json!({}),
            limits,
        )
        .await
        .map(|r| r.0)
    }

    async fn mount(server: &MockServer, p: &str, template: ResponseTemplate, times: u64) {
        Mock::given(method("POST"))
            .and(path(p.to_string()))
            .respond_with(template)
            .up_to_n_times(times)
            .mount(server)
            .await;
    }

    fn ok() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_string(OK_BODY)
    }

    const PRIMARY: &str = "/gemini-3.6-flash:generateContent";
    const P38: &str = "/gemini-3.8-flash:generateContent";
    const P37: &str = "/gemini-3.7-flash:generateContent";

    #[tokio::test]
    async fn retries_503_then_succeeds_on_third_attempt() {
        let s = MockServer::start().await;
        mount(&s, PRIMARY, ResponseTemplate::new(503), 2).await;
        mount(&s, PRIMARY, ok(), 1).await;
        assert_eq!(
            run(&s, "gemini-3.6-flash", Duration::from_secs(30))
                .await
                .unwrap(),
            "{}"
        );
        assert_eq!(s.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn ladder_tries_newest_first_after_primary_exhausted() {
        let s = MockServer::start().await;
        mount(&s, PRIMARY, ResponseTemplate::new(503), 10).await;
        mount(&s, P38, ok(), 1).await;
        assert_eq!(
            run(&s, "gemini-3.6-flash", Duration::from_secs(30))
                .await
                .unwrap(),
            "{}"
        );
        assert_eq!(s.received_requests().await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn ladder_moves_to_next_rung_when_first_fails() {
        let s = MockServer::start().await;
        mount(&s, PRIMARY, ResponseTemplate::new(503), 10).await;
        mount(&s, P38, ResponseTemplate::new(503), 10).await;
        mount(&s, P37, ok(), 1).await;
        assert_eq!(
            run(&s, "gemini-3.6-flash", Duration::from_secs(30))
                .await
                .unwrap(),
            "{}"
        );
        assert_eq!(s.received_requests().await.unwrap().len(), 7);
    }

    #[tokio::test]
    async fn top_rung_primary_falls_to_next_rung() {
        let s = MockServer::start().await;
        mount(&s, P38, ResponseTemplate::new(503), 10).await;
        mount(&s, P37, ok(), 1).await;
        assert_eq!(
            run(&s, "gemini-3.8-flash", Duration::from_secs(30))
                .await
                .unwrap(),
            "{}"
        );
        let reqs = s.received_requests().await.unwrap();
        assert_eq!(reqs.last().unwrap().url.path(), P37);
    }

    #[tokio::test]
    async fn unlisted_primary_tries_ladder_in_order_capped_at_two_extra() {
        let s = MockServer::start().await;
        mount(
            &s,
            "/custom:generateContent",
            ResponseTemplate::new(503),
            10,
        )
        .await;
        mount(&s, P38, ResponseTemplate::new(503), 10).await;
        mount(&s, P37, ResponseTemplate::new(503), 10).await;
        mount(&s, PRIMARY, ok(), 10).await;
        let r = run(&s, "custom", Duration::from_secs(30)).await;
        assert!(matches!(r, Err(AiError::Unavailable(_))), "{r:?}");
        assert_eq!(s.received_requests().await.unwrap().len(), 9);
    }

    #[tokio::test]
    async fn persistent_503_is_unavailable() {
        let s = MockServer::start().await;
        for p in [PRIMARY, P38, P37] {
            mount(&s, p, ResponseTemplate::new(503), 10).await;
        }
        let r = run(&s, "gemini-3.6-flash", Duration::from_secs(30)).await;
        assert!(matches!(r, Err(AiError::Unavailable(_))), "{r:?}");
        assert_eq!(s.received_requests().await.unwrap().len(), 9);
    }

    fn body_429(quota_id: &str, retry_delay: Option<&str>) -> String {
        let mut details = vec![serde_json::json!({
            "@type": "type.googleapis.com/google.rpc.QuotaFailure",
            "violations": [{
                "quotaMetric": if quota_id.contains("FreeTier") {
                    "generativelanguage.googleapis.com/generate_content_free_tier_requests"
                } else {
                    "generativelanguage.googleapis.com/generate_content_requests"
                },
                "quotaId": quota_id,
                "quotaDimensions": {"location": "global", "model": "gemini-3.6-flash"},
                "quotaValue": "10"
            }]
        })];
        if let Some(d) = retry_delay {
            details.push(serde_json::json!({
                "@type": "type.googleapis.com/google.rpc.RetryInfo",
                "retryDelay": d
            }));
        }
        serde_json::json!({"error": {
            "code": 429,
            "message": "You exceeded your current quota.",
            "status": "RESOURCE_EXHAUSTED",
            "details": details
        }})
        .to_string()
    }

    const PER_MINUTE: &str = "GenerateRequestsPerMinutePerProjectPerModel-FreeTier";
    const PER_DAY: &str = "GenerateRequestsPerDayPerProjectPerModel-FreeTier";

    fn tpl_429(quota_id: &str, delay: Option<&str>) -> ResponseTemplate {
        ResponseTemplate::new(429).set_body_string(body_429(quota_id, delay))
    }

    #[test]
    fn classify_429_per_minute_uses_retry_info() {
        let r = classify_429(&body_429(PER_MINUTE, Some("12s")), None);
        assert!(
            matches!(
                r,
                AiError::RateLimited {
                    retry_after_secs: 12,
                    ..
                }
            ),
            "{r:?}"
        );
    }

    #[test]
    fn classify_429_per_day_is_quota() {
        let r = classify_429(&body_429(PER_DAY, Some("12s")), Some("3"));
        assert!(
            matches!(r, AiError::QuotaExceeded { free_tier: true }),
            "{r:?}"
        );
    }

    #[test]
    fn classify_429_free_tier_flag_follows_quota_id() {
        let r = classify_429(&body_429(PER_MINUTE, Some("12s")), None);
        assert!(
            matches!(
                r,
                AiError::RateLimited {
                    free_tier: true,
                    ..
                }
            ),
            "{r:?}"
        );
        let paid = body_429("GenerateRequestsPerMinutePerProjectPerModel", Some("12s"));
        let r = classify_429(&paid, None);
        assert!(
            matches!(
                r,
                AiError::RateLimited {
                    free_tier: false,
                    ..
                }
            ),
            "{r:?}"
        );
        let paid_day = body_429("GenerateRequestsPerDayPerProjectPerModel", None);
        let r = classify_429(&paid_day, None);
        assert!(
            matches!(r, AiError::QuotaExceeded { free_tier: false }),
            "{r:?}"
        );
    }

    #[test]
    fn classify_429_defaults_and_rounding() {
        let r = classify_429("", None);
        assert!(
            matches!(
                r,
                AiError::RateLimited {
                    retry_after_secs: 60,
                    ..
                }
            ),
            "{r:?}"
        );
        let r = classify_429("", Some("7"));
        assert!(
            matches!(
                r,
                AiError::RateLimited {
                    retry_after_secs: 7,
                    ..
                }
            ),
            "{r:?}"
        );
        let r = classify_429(&body_429(PER_MINUTE, Some("1.5s")), None);
        assert!(
            matches!(
                r,
                AiError::RateLimited {
                    retry_after_secs: 2,
                    ..
                }
            ),
            "{r:?}"
        );
    }

    #[tokio::test]
    async fn per_minute_429_waits_then_succeeds() {
        let s = MockServer::start().await;
        mount(&s, PRIMARY, tpl_429(PER_MINUTE, Some("1s")), 1).await;
        mount(&s, PRIMARY, ok(), 10).await;
        let r = run(&s, "gemini-3.6-flash", Duration::from_secs(30)).await;
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(s.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn persistent_per_minute_429_is_rate_limited() {
        let s = MockServer::start().await;
        mount(&s, PRIMARY, tpl_429(PER_MINUTE, Some("1s")), 10).await;
        let r = run(&s, "gemini-3.6-flash", Duration::from_secs(30)).await;
        assert!(
            matches!(
                r,
                Err(AiError::RateLimited {
                    retry_after_secs: 1,
                    ..
                })
            ),
            "{r:?}"
        );
    }

    #[tokio::test]
    async fn per_day_quota_is_not_retried_on_the_same_model() {
        let s = MockServer::start().await;
        mount(&s, PRIMARY, tpl_429(PER_DAY, None), 10).await;
        let r = run(&s, "gemini-3.6-flash", Duration::from_secs(30)).await;
        assert!(matches!(r, Err(AiError::QuotaExceeded { .. })), "{r:?}");
        let primary_hits = s
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|q| q.url.path() == PRIMARY)
            .count();
        assert_eq!(primary_hits, 1);
    }

    #[tokio::test]
    async fn per_day_quota_falls_back_to_another_model() {
        let s = MockServer::start().await;
        mount(&s, PRIMARY, tpl_429(PER_DAY, None), 10).await;
        mount(&s, P38, ok(), 10).await;
        let r = run(&s, "gemini-3.6-flash", Duration::from_secs(30)).await;
        assert!(r.is_ok(), "{r:?}");
    }

    #[tokio::test]
    async fn retry_after_header_delays_the_retry() {
        let s = MockServer::start().await;
        mount(
            &s,
            PRIMARY,
            ResponseTemplate::new(503).insert_header("retry-after", "1"),
            1,
        )
        .await;
        mount(&s, PRIMARY, ok(), 1).await;
        let t = Instant::now();
        run(&s, "gemini-3.6-flash", Duration::from_secs(30))
            .await
            .unwrap();
        assert!(t.elapsed() >= Duration::from_secs(1));
    }

    #[test]
    fn retry_delay_honors_and_caps_retry_after() {
        let base = Duration::from_millis(1000);
        assert_eq!(retry_delay(base, Some("2")), Duration::from_secs(2));
        assert_eq!(retry_delay(base, Some("120")), RETRY_AFTER_CAP);
        let d = retry_delay(base, Some("Wed, 21 Oct 2026 07:28:00 GMT"));
        assert!(d >= base && d <= base + base / 4);
        assert_eq!(retry_delay(Duration::ZERO, None), Duration::ZERO);
    }

    #[tokio::test]
    async fn exhausted_budget_stops_retrying_and_skips_fallback() {
        let s = MockServer::start().await;
        let slow_503 = ResponseTemplate::new(503).set_delay(Duration::from_millis(60));
        mount(&s, PRIMARY, slow_503, 10).await;
        mount(&s, P38, ok(), 10).await;
        // First attempt alone uses over half the budget, so neither a retry nor a fallback model
        // starts.
        let r = run(&s, "gemini-3.6-flash", Duration::from_millis(100)).await;
        assert!(matches!(r, Err(AiError::Unavailable(_))), "{r:?}");
        assert_eq!(s.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn timeout_after_503_is_unavailable() {
        let s = MockServer::start().await;
        mount(&s, PRIMARY, ResponseTemplate::new(503), 1).await;
        mount(&s, PRIMARY, ok().set_delay(Duration::from_secs(5)), 10).await;
        let r = run(&s, "gemini-3.6-flash", Duration::from_millis(300)).await;
        assert!(matches!(r, Err(AiError::Unavailable(_))), "{r:?}");
    }

    #[tokio::test]
    async fn timeout_without_5xx_is_unavailable() {
        let s = MockServer::start().await;
        mount(&s, PRIMARY, ok().set_delay(Duration::from_secs(5)), 10).await;
        let r = run(&s, "gemini-3.6-flash", Duration::from_millis(100)).await;
        assert!(matches!(r, Err(AiError::Unavailable(_))), "{r:?}");
    }

    fn short_cap() -> Limits {
        Limits {
            budget: Duration::from_secs(5),
            cap: Duration::from_millis(200),
            min_attempt: Duration::from_millis(100),
        }
    }

    #[tokio::test]
    async fn hung_attempt_is_cut_at_cap_and_retried() {
        let s = MockServer::start().await;
        mount(&s, PRIMARY, ok().set_delay(Duration::from_secs(10)), 1).await;
        mount(&s, PRIMARY, ok(), 1).await;
        // Built outside the timed section: client construction can take seconds on a loaded host.
        let http = reqwest::Client::new();
        let t = Instant::now();
        let out = run_with(&http, &s, "gemini-3.6-flash", short_cap())
            .await
            .unwrap();
        assert_eq!(out, "{}");
        // Under the 5s budget, so the 200ms cap (not the budget) cut the hung attempt.
        assert!(t.elapsed() < Duration::from_secs(4));
        assert_eq!(s.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn hanging_primary_falls_through_to_fallback_model() {
        let s = MockServer::start().await;
        mount(&s, PRIMARY, ok().set_delay(Duration::from_secs(3)), 10).await;
        mount(&s, P38, ok(), 1).await;
        run_with(&reqwest::Client::new(), &s, "gemini-3.6-flash", short_cap())
            .await
            .unwrap();
        let reqs = s.received_requests().await.unwrap();
        assert_eq!(reqs.last().unwrap().url.path(), P38);
    }

    #[tokio::test]
    async fn bad_request_is_generation_failed() {
        let s = MockServer::start().await;
        let body = r#"{"error":{"code":400,"message":"Thinking level MINIMAL is not supported","status":"INVALID_ARGUMENT"}}"#;
        mount(
            &s,
            PRIMARY,
            ResponseTemplate::new(400).set_body_string(body),
            10,
        )
        .await;
        let r = run(&s, "gemini-3.6-flash", Duration::from_secs(30)).await;
        assert!(
            matches!(&r, Err(AiError::GenerationFailed(m)) if m.contains("MINIMAL")),
            "{r:?}"
        );
        assert_eq!(s.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn bad_request_with_invalid_key_reason_is_invalid_key() {
        let s = MockServer::start().await;
        let body = r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT","details":[{"reason":"API_KEY_INVALID"}]}}"#;
        mount(
            &s,
            PRIMARY,
            ResponseTemplate::new(400).set_body_string(body),
            10,
        )
        .await;
        let r = run(&s, "gemini-3.6-flash", Duration::from_secs(30)).await;
        assert!(matches!(r, Err(AiError::InvalidKey)), "{r:?}");
    }

    #[tokio::test]
    async fn thinking_level_low_only_for_gemini_3() {
        use wiremock::matchers::body_partial_json;
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PRIMARY))
            .and(body_partial_json(serde_json::json!({
                "generationConfig": { "thinkingConfig": { "thinkingLevel": "low" } }
            })))
            .respond_with(ok())
            .expect(1)
            .mount(&s)
            .await;
        run(&s, "gemini-3.6-flash", Duration::from_secs(30))
            .await
            .unwrap();
        let s2 = MockServer::start().await;
        mount(&s2, "/gemini-2.5-flash:generateContent", ok(), 1).await;
        run(&s2, "gemini-2.5-flash", Duration::from_secs(30))
            .await
            .unwrap();
        let req = &s2.received_requests().await.unwrap()[0];
        let sent: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert!(sent["generationConfig"].get("thinkingConfig").is_none());
    }

    #[test]
    fn test_key_5xx_is_unavailable() {
        assert!(matches!(
            map_test_key_status(503, "", None),
            Err(AiError::Unavailable(_))
        ));
        assert!(matches!(
            map_test_key_status(401, "", None),
            Err(AiError::InvalidKey)
        ));
        assert!(matches!(
            map_test_key_status(404, "", None),
            Err(AiError::GenerationFailed(_))
        ));
    }

    #[test]
    fn test_key_429_distinguishes_rate_limit_from_quota() {
        let r = map_test_key_status(429, &body_429(PER_MINUTE, Some("12s")), None);
        assert!(
            matches!(
                r,
                Err(AiError::RateLimited {
                    retry_after_secs: 12,
                    ..
                })
            ),
            "{r:?}"
        );
        let r = map_test_key_status(429, &body_429(PER_DAY, None), None);
        assert!(matches!(r, Err(AiError::QuotaExceeded { .. })), "{r:?}");
    }

    #[tokio::test]
    async fn generate_json_returns_object_and_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/m:generateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"candidates":[{"content":{"parts":[{"text":"{\"a\":1}"}]}}],
                "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3}}"#,
            ))
            .mount(&server)
            .await;
        let g = generate_json_at(
            &reqwest::Client::new(),
            &server.uri(),
            &FAST,
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
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let r = generate_json_at(
            &reqwest::Client::new(),
            &server.uri(),
            &FAST,
            "m",
            "k",
            "p",
            &serde_json::json!({}),
        )
        .await;
        assert!(matches!(r, Err(AiError::InvalidKey)));
    }

    #[tokio::test]
    #[ignore = "calls the real Gemini API; needs GEMINI_API_KEY"]
    async fn live_sql_generation() {
        let key = std::env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY");
        let t = Instant::now();
        let g = GeminiProvider
            .generate_sql(
                &reqwest::Client::new(),
                &key,
                "gemini-3.6-flash",
                "Write a SQL query returning the number of rows in a table named orders.",
            )
            .await
            .unwrap();
        println!("live sql in {:?}: {}", t.elapsed(), g.sql);
        assert!(g.sql.to_lowercase().contains("select"));
    }

    #[test]
    fn adapted_schemas_use_gemini_dialect() {
        let viz = to_gemini_schema(&viz_schema(true));
        assert_eq!(
            viz["properties"]["title"],
            serde_json::json!({"type":"STRING","nullable":true})
        );
        assert_eq!(
            viz["properties"]["yColumns"],
            serde_json::json!({"type":"ARRAY","nullable":true,"items":{"type":"STRING"}})
        );
        assert_eq!(viz["properties"]["viz_type"]["type"], "STRING");
        assert_eq!(
            viz["required"].as_array().unwrap().len(),
            crate::ai::viz::VIZ_FIELDS.len() + 2
        );
        let suggest = to_gemini_schema(&suggest_schema(true));
        assert_eq!(
            suggest["properties"]["suggestions"]["items"]["properties"]["value"]["type"],
            "NUMBER"
        );
        for schema in [&viz, &suggest, &to_gemini_schema(&response_schema(true))] {
            assert_eq!(schema["type"], "OBJECT");
            assert!(!schema.to_string().contains("additionalProperties"));
        }
    }

    #[test]
    fn strips_additional_properties() {
        let strict = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "n": { "type": ["string", "null"] },
                "xs": { "type": "array", "items": { "type": "object", "additionalProperties": false,
                    "properties": { "type": { "type": "integer" } } } }
            }
        });
        let out = to_gemini_schema(&strict);
        assert!(!out.to_string().contains("additionalProperties"));
        assert_eq!(out["type"], "OBJECT");
        assert_eq!(
            out["properties"]["n"],
            serde_json::json!({"type":"STRING","nullable":true})
        );
        assert_eq!(
            out["properties"]["xs"]["items"]["properties"]["type"]["type"],
            "INTEGER"
        );
    }
}

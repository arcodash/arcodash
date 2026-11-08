//! Prometheus connector: instant queries against `/api/v1/query`.

use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::connectors::{
    ColumnMeta, ColumnSchema, NetworkTarget, QueryExecutionResult, QueryRunner, TableSchema,
};

use super::{percent_encode, pinned_http_client, HTTP_TIMEOUT};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrometheusOptions {
    pub host: String,
    #[serde(default = "prom_port")]
    pub port: u16,
    #[serde(default)]
    pub use_https: bool,
}

const fn prom_port() -> u16 {
    9090
}

pub struct PrometheusConnector;

fn prom_base(opts: &PrometheusOptions) -> String {
    let base = crate::connectors::http_base_url(opts.use_https, &opts.host, opts.port);
    format!("{base}/api/v1")
}

/// Decode one Prometheus sample value (`"1.23"`, `"NaN"`, `"+Inf"`, or absent) to JSON — `null`
/// for a missing/unparseable sample rather than a fabricated `0.0`, which is indistinguishable
/// from a genuine zero reading. `f64::parse` already accepts Prometheus's NaN/Inf spellings, and
/// `serde_json` maps a non-finite `f64` to `null` on its own (`Number::from_f64` returns `None`
/// for NaN/±Inf) — this just makes that outcome explicit instead of relying on it silently.
fn prom_sample_to_json(raw: Option<&str>) -> Value {
    raw.and_then(|s| s.parse::<f64>().ok())
        .map_or(Value::Null, |v| json!(v))
}

/// Same treatment as [`prom_sample_to_json`] for a result's `timestamp` field (always numeric in
/// Prometheus's wire format, never a string).
fn prom_timestamp_to_json(raw: Option<&Value>) -> Value {
    raw.and_then(Value::as_f64)
        .map_or(Value::Null, |v| json!(v))
}

/// Client pinned to the guard-approved addresses for `opts.host`.
async fn prom_client(opts: &PrometheusOptions) -> Result<reqwest::Client> {
    let addrs = crate::net_guard::guard_host(&opts.host).await?;
    pinned_http_client(&opts.host, &addrs)
}

#[async_trait]
impl QueryRunner for PrometheusConnector {
    fn type_name(&self) -> &'static str {
        "prometheus"
    }

    fn configuration_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": { "type": "string", "title": "Host" },
                "port": { "type": "integer", "title": "Port", "default": 9090 },
                "use_https": { "type": "boolean", "title": "Use HTTPS", "default": false }
            },
            "required": ["host"],
            "secret": []
        })
    }

    async fn test_connection(&self, opts: &Value) -> Result<()> {
        let opts: PrometheusOptions = serde_json::from_value(opts.clone())?;
        prom_client(&opts)
            .await?
            .get(format!("{}/query?query=1", prom_base(&opts)))
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .context("Prometheus connection failed")?
            .error_for_status()
            .context("Prometheus returned error status")?;
        Ok(())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let opts: PrometheusOptions = serde_json::from_value(opts.clone())?;
        let resp: Value = prom_client(&opts)
            .await?
            .get(format!("{}/label/__name__/values", prom_base(&opts)))
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .context("fetch metric names failed")?
            .error_for_status()
            .context("Prometheus returned error status")?
            .json()
            .await?;
        let tables = resp["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|name| TableSchema {
                name: name.to_string(),
                columns: vec![
                    ColumnSchema::untyped("timestamp"),
                    ColumnSchema::untyped("value"),
                ],
            })
            .collect();
        Ok(tables)
    }

    fn network_targets(&self, opts: &Value) -> Vec<NetworkTarget> {
        serde_json::from_value::<PrometheusOptions>(opts.clone())
            .map(|opts| vec![NetworkTarget::Host(opts.host)])
            .unwrap_or_default()
    }

    async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
        let opts: PrometheusOptions = serde_json::from_value(opts.clone())?;
        let client = prom_client(&opts).await?;
        let start = Instant::now();
        let url = format!(
            "{}/query?query={}",
            prom_base(&opts),
            percent_encode(query.trim())
        );
        let resp: Value = client
            .get(&url)
            .timeout(HTTP_TIMEOUT)
            .send()
            .await?
            .json()
            .await?;
        let runtime_seconds = start.elapsed().as_secs_f64();

        if resp["status"].as_str() != Some("success") {
            anyhow::bail!(
                "Prometheus error: {}",
                resp["error"].as_str().unwrap_or("unknown")
            );
        }

        let result_type = resp
            .pointer("/data/resultType")
            .and_then(Value::as_str)
            .unwrap_or("");
        let result_data = resp
            .pointer("/data/result")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();

        let columns = [
            ("metric", "string"),
            ("timestamp", "datetime"),
            ("value", "float"),
        ]
        .into_iter()
        .map(|(name, friendly_type)| ColumnMeta {
            name: name.to_string(),
            friendly_type: friendly_type.to_string(),
        })
        .collect();

        let json_rows = result_data
            .iter()
            .flat_map(|item| {
                let metric_str = item
                    .get("metric")
                    .map_or_else(|| "{}".to_string(), Value::to_string);
                // One `[timestamp, "value"]` pair; anything else decodes to nulls.
                let sample = |pair: &Value| {
                    json!({
                        "metric": metric_str,
                        "timestamp": prom_timestamp_to_json(pair.get(0)),
                        "value": prom_sample_to_json(pair.get(1).and_then(Value::as_str))
                    })
                };
                match result_type {
                    "matrix" => item
                        .get("values")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(sample)
                        .collect(),
                    _ => vec![sample(item.get("value").unwrap_or(&Value::Null))],
                }
            })
            .collect();

        Ok(QueryExecutionResult {
            columns,
            rows: json_rows,
            runtime_seconds,
        })
    }
}

#[cfg(test)]
mod prom_null_handling_tests {
    use super::{prom_sample_to_json, prom_timestamp_to_json};
    use serde_json::{json, Value};

    #[test]
    fn valid_sample_parses_to_a_real_number() {
        assert_eq!(prom_sample_to_json(Some("1.5")), json!(1.5));
    }

    #[test]
    fn absent_sample_is_null_not_zero() {
        assert_eq!(prom_sample_to_json(None), Value::Null);
    }

    #[test]
    fn unparseable_sample_is_null_not_zero() {
        assert_eq!(prom_sample_to_json(Some("not-a-number")), Value::Null);
    }

    #[test]
    fn nan_and_inf_samples_are_null() {
        assert_eq!(prom_sample_to_json(Some("NaN")), Value::Null);
        assert_eq!(prom_sample_to_json(Some("+Inf")), Value::Null);
        assert_eq!(prom_sample_to_json(Some("-Inf")), Value::Null);
    }

    #[test]
    fn absent_timestamp_is_null_not_zero() {
        assert_eq!(prom_timestamp_to_json(None), Value::Null);
    }

    #[test]
    fn valid_timestamp_parses_to_a_real_number() {
        assert_eq!(
            prom_timestamp_to_json(Some(&json!(1700000000.0))),
            json!(1700000000.0)
        );
    }
}

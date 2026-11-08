//! `InfluxDB` v1 connector: `SHOW MEASUREMENTS` for schema, `/query` for everything else.

use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::connectors::{
    ColumnMeta, ColumnSchema, NetworkTarget, QueryExecutionResult, QueryRunner, TableSchema,
};

use super::{classify_reqwest_err, percent_encode, pinned_http_client, HTTP_TIMEOUT};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfluxDbOptions {
    pub host: String,
    #[serde(default = "influx_port")]
    pub port: u16,
    pub db: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub use_https: bool,
}

const fn influx_port() -> u16 {
    8086
}

pub struct InfluxDbConnector;

async fn influx_query(opts: &InfluxDbOptions, q: &str) -> Result<Value> {
    let addrs = crate::net_guard::guard_host(&opts.host).await?;
    let url = format!(
        "{}/query?db={}&q={}",
        crate::connectors::http_base_url(opts.use_https, &opts.host, opts.port),
        percent_encode(&opts.db),
        percent_encode(q)
    );
    let mut req = pinned_http_client(&opts.host, &addrs)?
        .get(&url)
        .timeout(HTTP_TIMEOUT);
    if !opts.user.is_empty() {
        req = req.basic_auth(&opts.user, Some(&opts.password));
    }
    let resp = req
        .send()
        .await
        .map_err(classify_reqwest_err)
        .context("InfluxDB request failed")?;
    let status = resp.status();
    let body = resp.text().await.context("InfluxDB response read failed")?;
    if !status.is_success() {
        anyhow::bail!("InfluxDB returned {status}: {body}");
    }
    serde_json::from_str(&body).context("InfluxDB response parse failed")
}

#[async_trait]
impl QueryRunner for InfluxDbConnector {
    fn type_name(&self) -> &'static str {
        "influxdb"
    }

    fn configuration_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": { "type": "string", "title": "Host" },
                "port": { "type": "integer", "title": "Port", "default": 8086 },
                "db": { "type": "string", "title": "Database" },
                "user": { "type": "string", "title": "Username" },
                "password": { "type": "string", "title": "Password" },
                "use_https": { "type": "boolean", "title": "Use HTTPS", "default": false }
            },
            "required": ["host", "db"],
            "secret": ["password"]
        })
    }

    async fn test_connection(&self, opts: &Value) -> Result<()> {
        let opts: InfluxDbOptions = serde_json::from_value(opts.clone())?;
        influx_query(&opts, "SHOW MEASUREMENTS").await.map(|_| ())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let opts: InfluxDbOptions = serde_json::from_value(opts.clone())?;
        let result = influx_query(&opts, "SHOW MEASUREMENTS").await?;
        let tables = result
            .pointer("/results/0/series/0/values")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_array()?.first()?.as_str())
            .map(|name| TableSchema {
                name: name.to_string(),
                columns: vec![
                    ColumnSchema::untyped("time"),
                    ColumnSchema::untyped("value"),
                ],
            })
            .collect();
        Ok(tables)
    }

    fn network_targets(&self, opts: &Value) -> Vec<NetworkTarget> {
        serde_json::from_value::<InfluxDbOptions>(opts.clone())
            .map(|opts| vec![NetworkTarget::Host(opts.host)])
            .unwrap_or_default()
    }

    async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
        let opts: InfluxDbOptions = serde_json::from_value(opts.clone())?;
        let start = Instant::now();
        let result = influx_query(&opts, query).await?;
        let runtime_seconds = start.elapsed().as_secs_f64();
        let results = result["results"].as_array().map(Vec::as_slice);
        let (columns, rows) = flatten_influx_series(results.unwrap_or_default())?;
        Ok(QueryExecutionResult {
            columns,
            rows,
            runtime_seconds,
        })
    }
}

/// One series' data (from a `results[].series[]` entry) reduced to what
/// [`flatten_influx_series`] needs: its stable label, its own column list, and its raw value rows.
struct InfluxSeries<'a> {
    label: String,
    columns: Vec<String>,
    values: &'a [Value],
}

/// A `GROUP BY` query (the normal Influx idiom) returns one series per tag combination, and a
/// multi-statement query returns one `results[]` entry per statement — reading only
/// `/results/0/series/0` would silently drop everything else. `name` + a sorted
/// rendering of `tags` distinguishes rows from different series in the flattened output, since
/// two series can otherwise share identical column names.
fn influx_series_label(series: &Value) -> String {
    let name = series["name"].as_str().unwrap_or("");
    match series.get("tags").and_then(Value::as_object) {
        Some(tags) if !tags.is_empty() => {
            let mut parts: Vec<String> = tags
                .iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
                .collect();
            parts.sort();
            format!("{name}[{}]", parts.join(","))
        }
        _ => name.to_string(),
    }
}

/// Pure flatten step: every series across every statement result, unioned into one column set
/// (plus a `series` column carrying [`influx_series_label`]) and one row set, with
/// `Value::Null` filled in for a column a given series doesn't have. Exercised directly by unit
/// tests without a network round trip.
fn flatten_influx_series(results: &[Value]) -> Result<(Vec<ColumnMeta>, Vec<Value>)> {
    for r in results {
        if let Some(err) = r.get("error") {
            anyhow::bail!("InfluxDB error: {err}");
        }
    }

    let mut col_names: Vec<String> = vec!["series".to_string()];
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert("series".to_string());
    let mut all_series: Vec<InfluxSeries<'_>> = Vec::new();

    for series in results
        .iter()
        .filter_map(|r| r["series"].as_array())
        .flatten()
    {
        let columns: Vec<String> = series["columns"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        for c in &columns {
            if seen.insert(c.clone()) {
                col_names.push(c.clone());
            }
        }
        all_series.push(InfluxSeries {
            label: influx_series_label(series),
            columns,
            values: series["values"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default(),
        });
    }

    let types = col_names.iter().map(|n| match n.as_str() {
        "time" => "datetime",
        "series" => "string",
        _ => "float",
    });
    let column_meta = ColumnMeta::zip_types(&col_names, types);

    let mut json_rows = Vec::with_capacity(all_series.iter().map(|s| s.values.len()).sum());
    for series in &all_series {
        for row in series.values {
            let mut obj = serde_json::Map::with_capacity(col_names.len());
            obj.insert("series".to_string(), json!(series.label));
            for name in col_names.iter().filter(|n| n.as_str() != "series") {
                let value = series
                    .columns
                    .iter()
                    .position(|c| c == name)
                    .map_or(Value::Null, |idx| row[idx].clone());
                obj.insert(name.clone(), value);
            }
            json_rows.push(Value::Object(obj));
        }
    }

    Ok((column_meta, json_rows))
}

#[cfg(test)]
mod influx_flatten_tests {
    use super::flatten_influx_series;
    use serde_json::json;

    #[test]
    fn single_series_matches_prior_single_series_behavior() {
        let results = vec![json!({
            "series": [{
                "name": "conn_test",
                "columns": ["time", "value"],
                "values": [["2024-01-01T00:00:00Z", 1.0]]
            }]
        })];
        let (columns, rows) = flatten_influx_series(&results).expect("flatten failed");
        assert!(columns.iter().any(|c| c.name == "time"));
        assert!(columns.iter().any(|c| c.name == "value"));
        assert!(columns.iter().any(|c| c.name == "series"));
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn multiple_series_across_one_statement_all_come_back_and_are_distinguishable() {
        let results = vec![json!({
            "series": [
                {
                    "name": "conn_test",
                    "tags": {"host": "a"},
                    "columns": ["time", "value"],
                    "values": [["2024-01-01T00:00:00Z", 1.0]]
                },
                {
                    "name": "conn_test",
                    "tags": {"host": "b"},
                    "columns": ["time", "value"],
                    "values": [["2024-01-01T00:00:00Z", 2.0]]
                }
            ]
        })];
        let (_columns, rows) = flatten_influx_series(&results).expect("flatten failed");
        assert_eq!(rows.len(), 2, "rows from both series must come back");
        let labels: Vec<&str> = rows.iter().map(|r| r["series"].as_str().unwrap()).collect();
        assert_ne!(labels[0], labels[1], "series must be distinguishable");
        assert!(labels[0].contains("host=a") || labels[1].contains("host=a"));
    }

    #[test]
    fn multiple_statements_all_come_back() {
        let results = vec![
            json!({"series": [{"name": "a", "columns": ["time", "value"], "values": [["t1", 1.0]]}]}),
            json!({"series": [{"name": "b", "columns": ["time", "value"], "values": [["t2", 2.0]]}]}),
        ];
        let (_columns, rows) = flatten_influx_series(&results).expect("flatten failed");
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn a_column_missing_from_one_series_is_null_not_missing() {
        let results = vec![json!({
            "series": [
                {"name": "a", "columns": ["time", "value"], "values": [["t1", 1.0]]},
                {"name": "b", "columns": ["time", "extra"], "values": [["t2", 9.0]]}
            ]
        })];
        let (_columns, rows) = flatten_influx_series(&results).expect("flatten failed");
        let b_row = rows.iter().find(|r| r["series"] == json!("b")).unwrap();
        assert_eq!(b_row["value"], serde_json::Value::Null);
        assert_eq!(b_row["extra"], json!(9.0));
    }

    #[test]
    fn any_results_error_bails() {
        let results = vec![json!({"error": "measurement not found"})];
        assert!(flatten_influx_series(&results).is_err());
    }

    #[test]
    fn a_later_statements_error_is_not_ignored() {
        let results = vec![
            json!({"series": [{"name": "a", "columns": ["time"], "values": []}]}),
            json!({"error": "boom"}),
        ];
        assert!(flatten_influx_series(&results).is_err());
    }
}

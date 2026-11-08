//! Typed wrappers over the jsonb columns of `queries`, `widgets`, `monitors` and `data_sources`,
//! accessed via [`ParseOptions`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `queries.options` — editor/runtime options (parameters, table view settings, etc.).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryOptions {
    /// Query parameter definitions (name, type, default value, etc.).
    #[serde(default)]
    pub parameters: Vec<QueryParameterDefinition>,

    /// Optional per-query cache max-age override, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age: Option<i64>,

    /// Catch-all for fields not yet modeled explicitly.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// Supported query parameter types; drive the input control and `{{ name }}` substitution rules.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ParameterType {
    Text,
    Number,
    Date,
    /// "Date Range" — `{"start", "end"}`, accessed via `{{ name.start }}` / `{{ name.end }}`.
    #[serde(rename = "date-range")]
    DateRange,
    /// "Date/Time Range" — same shape as `DateRange` with datetime precision.
    #[serde(rename = "datetime-range")]
    DateTimeRange,
    Dropdown,
    /// "Query-based Dropdown" — options come from another query's result (`query_id`).
    #[serde(rename = "query")]
    QueryDropdown,
}

/// A query parameter definition in `queries.options.parameters`, substituted into the `{{ name }}`
/// token at execute time by `services::parameters::render_query`; `value` is the default.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryParameterDefinition {
    /// The `{{ name }}` token this definition binds to.
    pub name: String,

    /// Display label; defaults to `name` when absent.
    #[serde(default)]
    pub title: String,

    #[serde(rename = "type")]
    pub param_type: ParameterType,

    /// Default value: a bare string/number, an array of strings for multi-value dropdowns, or
    /// `{"start", "end"}` for range types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,

    /// Newline-separated static options for `Dropdown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enum_options: Option<String>,

    /// Source query id for `QueryDropdown`; its latest result's rows become the options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_id: Option<i64>,

    /// `Some` iff "allow multiple values" is enabled for a `Dropdown`/`QueryDropdown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_values_options: Option<MultiValuesOptions>,
}

/// Join config for a multi-select parameter: each value is wrapped in `prefix`/`suffix` and joined
/// with `separator` (defaults `'`, `'`, `,` give `IN ('a','b')`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MultiValuesOptions {
    pub prefix: String,
    pub suffix: String,
    pub separator: String,
}

impl Default for MultiValuesOptions {
    fn default() -> Self {
        Self {
            prefix: "'".to_string(),
            suffix: "'".to_string(),
            separator: ",".to_string(),
        }
    }
}

/// `monitors.options` — trigger condition evaluated against the first row of the latest result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MonitorOptions {
    /// Result column name the condition is evaluated against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,

    /// Comparison operator: one of "greater than", "less than", "equals".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,

    /// Threshold; raw JSON since it may be numeric or string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,

    /// Optional custom alert body (email/chat only): plain text with `{{name}}` and `{{state}}`
    /// replaced via `str::replace`. Falls back to the default when unset or blank.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_body: Option<String>,

    /// Optional custom subject (chat title); same substitution and fallback as `custom_body`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_subject: Option<String>,

    /// Permanently silenced until explicitly unmuted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,

    /// Snoozed until this timestamp (RFC3339); expires by comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted_until: Option<DateTime<Utc>>,

    /// Max data age to skip a refresh; `None` ⇒ the monitor's check interval. API-only; also the
    /// input for the "data age" stale label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_data_staleness_secs: Option<i64>,

    /// How long a check may wait for a refresh: `None` ⇒ always wait, `Some(n)` ⇒ up to `n` seconds
    /// (API-validated `<= 7200`), then evaluate the last result labeled stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fresh_wait_secs: Option<i64>,

    /// Evaluation mode; `None` ⇒ `Threshold`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MonitorKind>,

    /// Baseline alerting sensitivity; `None` ⇒ `Medium`. Only read when `kind` is `Baseline`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitivity: Option<Sensitivity>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MonitorKind {
    #[default]
    Threshold,
    Baseline,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Low,
    #[default]
    Medium,
    High,
}

impl Sensitivity {
    /// Robust z-score above which a baseline observation is anomalous.
    #[must_use]
    pub const fn z_threshold(self) -> f64 {
        match self {
            Self::Low => 5.0,
            Self::Medium => 3.5,
            Self::High => 2.5,
        }
    }
}

impl MonitorOptions {
    /// Whether this monitor is silenced (muted, or snooze not yet expired). The one place
    /// notify/escalate permission is decided; callers must not read `muted` directly.
    #[must_use]
    pub fn is_silenced(&self, now: DateTime<Utc>) -> bool {
        self.muted.unwrap_or(false) || self.muted_until.is_some_and(|u| u > now)
    }
}

/// Parses a monitor's stored `options`; unparseable options yield no condition, logged and counted.
///
/// Mute and snooze are salvaged field-by-field from object-shaped blobs ([`salvage_silence`]) so a
/// corrupt blob never resurrects a silenced monitor. A non-object blob has nothing to salvage: it
/// surfaces as `unevaluable`, which the worker's notification path does not silence-gate, so it can
/// still notify for a monitor believed muted.
#[must_use]
pub fn parse_monitor_options(monitor_id: i64, options: &Value) -> MonitorOptions {
    // A derived struct `Deserialize` also accepts a positional array, and every field defaults, so
    // `[]` would otherwise parse as all-default options.
    if !options.is_object() {
        return unparseable_monitor_options(monitor_id, options, None);
    }
    match parse_options(options) {
        Ok(parsed) => parsed,
        Err(e) => unparseable_monitor_options(monitor_id, options, Some(e)),
    }
}

/// Shared by both `parse_monitor_options` failure arms: log, count, and salvage. `error` is `None`
/// for the non-object shape.
fn unparseable_monitor_options(
    monitor_id: i64,
    options: &Value,
    error: Option<serde_json::Error>,
) -> MonitorOptions {
    crate::metrics::record_monitor_options_corrupt();
    if let Some(e) = error {
        tracing::warn!(
            monitor_id,
            error = %e,
            "monitor options are unparseable; evaluating against defaults"
        );
    } else {
        tracing::warn!(
            monitor_id,
            value = %options,
            "monitor options are unparseable; evaluating against defaults"
        );
    }
    salvage_silence(options)
}

/// Recovers silence state from options that failed to parse as a whole. A silence key that is
/// present but unreadable counts as silenced: suppressing a notification is recoverable, firing
/// against a mute is not.
fn salvage_silence(options: &Value) -> MonitorOptions {
    let mut salvaged = MonitorOptions::default();
    if let Some(v) = options.get("muted").filter(|v| !v.is_null()) {
        salvaged.muted = Some(serde_json::from_value(v.clone()).unwrap_or(true));
    }
    if let Some(v) = options.get("muted_until").filter(|v| !v.is_null()) {
        match serde_json::from_value::<DateTime<Utc>>(v.clone()) {
            Ok(until) => salvaged.muted_until = Some(until),
            Err(_) => salvaged.muted = Some(true),
        }
    }
    salvaged
}

/// `data_sources.schema` — cached schema introspection, refreshed periodically by the worker.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataSourceSchema {
    #[serde(default)]
    pub tables: Vec<SchemaTable>,
}

/// A single table/collection within a cached data-source schema.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaTable {
    pub name: String,
    #[serde(default)]
    pub columns: Vec<crate::connectors::ColumnSchema>,
}

/// TTL of the pending key; the worker re-arms it while a long profile job runs.
pub const AI_PROFILE_PENDING_TTL_SECS: u64 = 300;

/// Redis dedupe key set by the producer; cleared by the worker when the profile job ends.
#[must_use]
pub fn ai_profile_pending_key(data_source_id: i64) -> String {
    format!("arcodash:ai_profile:pending:{data_source_id}")
}

/// Redis guard that rate-limits LLM description generation per data source.
#[must_use]
pub fn ai_profile_describing_key(data_source_id: i64) -> String {
    format!("arcodash:ai_profile:describing:{data_source_id}")
}

/// `data_sources.ai_profile` — sampled-data digest fed to AI prompts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DataSourceAiProfile {
    #[serde(default)]
    pub tables: Vec<AiProfileTable>,
    #[serde(default)]
    pub relationships: Vec<AiRelationship>,
    /// `schema_refreshed_at` of the schema this profile was built from.
    pub schema_refreshed_at_seen: DateTime<Utc>,
    /// LLM column descriptions came from sampled values, so they must not reach a prompt once the
    /// org turns sample sharing off.
    #[serde(default)]
    pub descriptions_used_samples: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiProfileTable {
    pub name: String,
    pub sampled_rows: u32,
    #[serde(default)]
    pub columns: Vec<AiProfileColumn>,
    /// None for a table never sampled successfully (schema-only entry or failed sample).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_profiled_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Redis cooldown that rate-limits profile refreshes triggered from Ask/Fix.
#[must_use]
pub fn ai_profile_refresh_cooldown_key(data_source_id: i64) -> String {
    format!("arcodash:ai_profile:refresh_cooldown:{data_source_id}")
}

pub const AI_PROFILE_REFRESH_COOLDOWN_SECS: u64 = 600;

/// Source types that bill per scanned byte.
pub const METERED_SOURCE_TYPES: &[&str] = &["bigquery", "athena", "snowflake", "redshift"];

#[must_use]
pub fn is_metered(t: &str) -> bool {
    METERED_SOURCE_TYPES.contains(&t)
}

/// Which tables a profile job samples.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProfileData {
    #[default]
    Changed,
    All,
    None,
}

/// Current profile phase (`schema` or `data`); a legacy `1` value reads as `schema`.
pub async fn ai_profile_phase(
    redis: &mut redis::aio::ConnectionManager,
    data_source_id: i64,
) -> Option<&'static str> {
    let v: Option<String> =
        redis::AsyncCommands::get(redis, ai_profile_pending_key(data_source_id))
            .await
            .ok()?;
    match v.as_deref()? {
        "data" => Some("data"),
        _ => Some("schema"),
    }
}

/// Drops profile tables, columns and relationships that no longer exist in the schema.
pub fn prune_to_schema(p: &mut DataSourceAiProfile, s: &DataSourceSchema) {
    p.tables
        .retain_mut(|t| match s.tables.iter().find(|st| st.name == t.name) {
            Some(st) => {
                t.columns
                    .retain(|c| st.columns.iter().any(|sc| sc.name == c.name));
                true
            }
            None => false,
        });
    let has = |table: &str, col: &str| {
        s.tables
            .iter()
            .any(|st| st.name == table && st.columns.iter().any(|c| c.name == col))
    };
    p.relationships
        .retain(|r| has(&r.from_table, &r.from_column) && has(&r.to_table, &r.to_column));
}

/// Schema tables with no profile entry, a column the profile lacks, or a failed sample.
#[must_use]
pub fn tables_needing_data(p: Option<&DataSourceAiProfile>, s: &DataSourceSchema) -> Vec<String> {
    s.tables
        .iter()
        .filter(|st| {
            let Some(pt) = p.and_then(|p| p.tables.iter().find(|t| t.name == st.name)) else {
                return true;
            };
            pt.error.is_some()
                || st
                    .columns
                    .iter()
                    .any(|c| !pt.columns.iter().any(|pc| pc.name == c.name))
        })
        .map(|st| st.name.clone())
        .collect()
}

/// Replaces/inserts freshly sampled tables, keeping old descriptions of columns that persist.
pub fn merge_sampled(p: &mut DataSourceAiProfile, fresh: Vec<AiProfileTable>) {
    for mut ft in fresh {
        match p.tables.iter().position(|t| t.name == ft.name) {
            Some(i) => {
                for c in &mut ft.columns {
                    if let Some(oc) = p.tables[i].columns.iter().find(|oc| oc.name == c.name) {
                        c.description = oc.description.clone();
                    }
                }
                p.tables[i] = ft;
            }
            None => p.tables.push(ft),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiProfileColumn {
    pub name: String,
    pub null_frac: f32,
    pub distinct_in_sample: u32,
    #[serde(default)]
    pub samples: Vec<String>,
    #[serde(default)]
    pub min: Option<String>,
    #[serde(default)]
    pub max: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiRelationship {
    pub from_table: String,
    pub from_column: String,
    pub to_table: String,
    pub to_column: String,
    pub source: RelationshipSource,
}

/// Declaration order is precedence: `Fk` < `Mined` < `Name`, so the smallest wins on a duplicate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum RelationshipSource {
    Fk,
    Mined,
    Name,
}

/// Deserialize a typed options wrapper from a raw JSON column. `null` means "never set" and
/// parses like `{}` instead of failing with "invalid type: null".
pub fn parse_options<T: for<'de> Deserialize<'de>>(v: &Value) -> serde_json::Result<T> {
    match v {
        Value::Null => serde_json::from_value(Value::Object(serde_json::Map::default())),
        other => serde_json::from_value(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_profile_round_trips_and_rejects_unknown_source() {
        let v = serde_json::json!({
            "tables": [{"name": "t", "sampled_rows": 5,
                "columns": [{"name": "c", "null_frac": 0.5, "distinct_in_sample": 2}]}],
            "relationships": [{"from_table": "a", "from_column": "x", "to_table": "b",
                "to_column": "id", "source": "mined"}],
            "schema_refreshed_at_seen": "2026-09-30T00:00:00Z"
        });
        let p: DataSourceAiProfile = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(p.relationships[0].source, RelationshipSource::Mined);
        assert!(p.tables[0].columns[0].samples.is_empty());
        let back: DataSourceAiProfile =
            serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
        assert_eq!(back, p);

        let mut bad = v;
        bad["relationships"][0]["source"] = "guess".into();
        assert!(serde_json::from_value::<DataSourceAiProfile>(bad).is_err());
        assert!(RelationshipSource::Fk < RelationshipSource::Mined);
        assert!(RelationshipSource::Mined < RelationshipSource::Name);
    }

    fn pcol(name: &str, desc: Option<&str>) -> AiProfileColumn {
        AiProfileColumn {
            name: name.into(),
            null_frac: 0.0,
            distinct_in_sample: 1,
            samples: vec![],
            min: None,
            max: None,
            description: desc.map(Into::into),
        }
    }

    fn ptable(name: &str, cols: Vec<AiProfileColumn>) -> AiProfileTable {
        AiProfileTable {
            name: name.into(),
            sampled_rows: 1,
            columns: cols,
            data_profiled_at: None,
            error: None,
        }
    }

    fn stable(name: &str, cols: &[&str]) -> SchemaTable {
        SchemaTable {
            name: name.into(),
            columns: cols
                .iter()
                .map(|c| crate::connectors::ColumnSchema::typed(*c, "text"))
                .collect(),
        }
    }

    fn profile(tables: Vec<AiProfileTable>, rels: Vec<AiRelationship>) -> DataSourceAiProfile {
        DataSourceAiProfile {
            tables,
            relationships: rels,
            schema_refreshed_at_seen: Utc::now(),
            descriptions_used_samples: false,
        }
    }

    fn rel(ft: &str, fc: &str, tt: &str, tc: &str) -> AiRelationship {
        AiRelationship {
            from_table: ft.into(),
            from_column: fc.into(),
            to_table: tt.into(),
            to_column: tc.into(),
            source: RelationshipSource::Fk,
        }
    }

    #[test]
    fn profile_table_new_fields_default_and_skip_when_none() {
        let t: AiProfileTable = serde_json::from_value(serde_json::json!({
            "name": "t", "sampled_rows": 1
        }))
        .unwrap();
        assert!(t.data_profiled_at.is_none() && t.error.is_none());
        let v = serde_json::to_value(&t).unwrap();
        assert!(v.get("data_profiled_at").is_none() && v.get("error").is_none());

        let mut t2 = t;
        t2.data_profiled_at = Some(Utc::now());
        t2.error = Some("boom".into());
        let back: AiProfileTable =
            serde_json::from_value(serde_json::to_value(&t2).unwrap()).unwrap();
        assert_eq!(back, t2);
    }

    #[test]
    fn prune_drops_gone_table_column_and_relationship() {
        let mut p = profile(
            vec![
                ptable("a", vec![pcol("id", None), pcol("old", None)]),
                ptable("gone", vec![pcol("id", None)]),
            ],
            vec![
                rel("a", "id", "gone", "id"),
                rel("a", "old", "a", "id"),
                rel("a", "id", "a", "id"),
            ],
        );
        let s = DataSourceSchema {
            tables: vec![stable("a", &["id"])],
        };
        prune_to_schema(&mut p, &s);
        assert_eq!(p.tables.len(), 1);
        assert_eq!(p.tables[0].columns.len(), 1);
        assert_eq!(p.relationships, vec![rel("a", "id", "a", "id")]);
    }

    #[test]
    fn tables_needing_data_cases() {
        let mut errored = ptable("err", vec![pcol("id", None)]);
        errored.error = Some("x".into());
        let p = profile(
            vec![
                ptable("same", vec![pcol("id", None)]),
                ptable("grown", vec![pcol("id", None)]),
                errored,
            ],
            vec![],
        );
        let s = DataSourceSchema {
            tables: vec![
                stable("new", &["id"]),
                stable("same", &["id"]),
                stable("grown", &["id", "extra"]),
                stable("err", &["id"]),
            ],
        };
        assert_eq!(
            tables_needing_data(Some(&p), &s),
            vec!["new", "grown", "err"]
        );
        assert_eq!(tables_needing_data(None, &s).len(), 4);
    }

    #[test]
    fn merge_sampled_keeps_descriptions_of_surviving_columns() {
        let mut p = profile(vec![ptable("t", vec![pcol("a", Some("desc"))])], vec![]);
        merge_sampled(
            &mut p,
            vec![
                ptable("t", vec![pcol("a", None), pcol("b", None)]),
                ptable("u", vec![pcol("x", None)]),
            ],
        );
        assert_eq!(p.tables.len(), 2);
        assert_eq!(p.tables[0].columns[0].description.as_deref(), Some("desc"));
        assert_eq!(p.tables[0].columns[1].description, None);
    }

    #[test]
    fn profile_data_default_and_metered() {
        assert_eq!(ProfileData::default(), ProfileData::Changed);
        assert_eq!(serde_json::to_string(&ProfileData::All).unwrap(), "\"all\"");
        assert!(is_metered("bigquery") && !is_metered("pg"));
    }

    #[test]
    fn query_options_round_trips_through_json_value() {
        let value = serde_json::json!({
            "parameters": [{"name": "foo", "title": "Foo", "type": "text"}],
            "max_age": 60,
            "some_future_field": "kept"
        });
        let parsed: QueryOptions = parse_options(&value).unwrap();
        assert_eq!(parsed.max_age, Some(60));
        assert_eq!(parsed.parameters.len(), 1);
        assert_eq!(parsed.parameters[0].name, "foo");
        assert_eq!(parsed.parameters[0].param_type, ParameterType::Text);
        assert_eq!(parsed.extra.get("some_future_field").unwrap(), "kept");
    }

    /// `null` (an absent options field) must parse as the type's default; malformed JSON must still
    /// fail.
    #[test]
    fn null_options_parse_as_default_not_an_error() {
        let parsed: QueryOptions = parse_options(&Value::Null).unwrap();
        assert_eq!(parsed, QueryOptions::default());

        let parsed: DataSourceSchema = parse_options(&Value::Null).unwrap();
        assert_eq!(parsed, DataSourceSchema::default());

        let parsed: MonitorOptions = parse_options(&Value::Null).unwrap();
        assert_eq!(parsed, MonitorOptions::default());

        // A malformed (non-null) blob must still fail — this isn't a blanket fallback-to-default.
        assert!(parse_options::<QueryOptions>(&serde_json::json!("not an object")).is_err());
    }

    #[test]
    fn query_parameter_definition_round_trips_all_types() {
        for (json_type, expected) in [
            ("text", ParameterType::Text),
            ("number", ParameterType::Number),
            ("date", ParameterType::Date),
            ("date-range", ParameterType::DateRange),
            ("datetime-range", ParameterType::DateTimeRange),
            ("dropdown", ParameterType::Dropdown),
            ("query", ParameterType::QueryDropdown),
        ] {
            let value = serde_json::json!({"name": "p", "title": "P", "type": json_type});
            let parsed: QueryParameterDefinition = parse_options(&value).unwrap();
            assert_eq!(parsed.param_type, expected, "type {json_type}");
        }
    }

    #[test]
    fn multi_values_options_default_to_single_quotes_and_comma() {
        let value = serde_json::json!({
            "name": "p",
            "title": "P",
            "type": "dropdown",
            "enum_options": "a\nb\nc",
            "multi_values_options": {}
        });
        let parsed: QueryParameterDefinition = parse_options(&value).unwrap();
        let mv = parsed.multi_values_options.unwrap();
        assert_eq!(mv.prefix, "'");
        assert_eq!(mv.suffix, "'");
        assert_eq!(mv.separator, ",");
    }

    #[test]
    fn monitor_options_parses_condition() {
        let value = serde_json::json!({"column": "value", "op": "greater than", "value": 100});
        let parsed: MonitorOptions = parse_options(&value).unwrap();
        assert_eq!(parsed.column, Some("value".to_string()));
        assert_eq!(parsed.op, Some("greater than".to_string()));
    }

    #[test]
    fn is_silenced_muted_alone() {
        let opts = MonitorOptions {
            muted: Some(true),
            ..Default::default()
        };
        assert!(opts.is_silenced(Utc::now()));
    }

    #[test]
    fn is_silenced_muted_until_future() {
        let opts = MonitorOptions {
            muted_until: Some(Utc::now() + chrono::Duration::minutes(30)),
            ..Default::default()
        };
        assert!(opts.is_silenced(Utc::now()));
    }

    #[test]
    fn is_silenced_muted_until_past_is_not_silenced() {
        let opts = MonitorOptions {
            muted_until: Some(Utc::now() - chrono::Duration::minutes(30)),
            ..Default::default()
        };
        assert!(!opts.is_silenced(Utc::now()));
    }

    #[test]
    fn is_silenced_neither_set() {
        let opts = MonitorOptions::default();
        assert!(!opts.is_silenced(Utc::now()));
    }

    #[test]
    fn is_silenced_both_set() {
        let opts = MonitorOptions {
            muted: Some(true),
            muted_until: Some(Utc::now() + chrono::Duration::minutes(30)),
            ..Default::default()
        };
        assert!(opts.is_silenced(Utc::now()));
    }

    #[test]
    fn monitor_options_freshness_fields_default_to_none() {
        let value = serde_json::json!({"column": "value", "op": "greater than", "value": 100});
        let parsed: MonitorOptions = parse_options(&value).unwrap();
        assert_eq!(parsed.max_data_staleness_secs, None);
        assert_eq!(parsed.max_fresh_wait_secs, None);
    }

    #[test]
    fn monitor_options_freshness_fields_parse_when_present() {
        let value = serde_json::json!({
            "column": "value", "op": "greater than", "value": 100,
            "max_data_staleness_secs": 300, "max_fresh_wait_secs": 5,
        });
        let parsed: MonitorOptions = parse_options(&value).unwrap();
        assert_eq!(parsed.max_data_staleness_secs, Some(300));
        assert_eq!(parsed.max_fresh_wait_secs, Some(5));
    }

    #[test]
    fn data_source_schema_parses_tables() {
        let value = serde_json::json!({
            "tables": [{"name": "users", "columns": [
                {"name": "id", "type_name": "integer"},
                {"name": "email", "type_name": "text"}
            ]}]
        });
        let parsed: DataSourceSchema = parse_options(&value).unwrap();
        assert_eq!(parsed.tables.len(), 1);
        assert_eq!(parsed.tables[0].name, "users");
        assert_eq!(parsed.tables[0].columns[0].name, "id");
        assert_eq!(
            parsed.tables[0].columns[0].type_name.as_deref(),
            Some("integer")
        );
    }

    #[test]
    fn data_source_schema_parses_bare_column_names() {
        // Cached payloads that predate per-column types store plain strings.
        let value = serde_json::json!({
            "tables": [{"name": "users", "columns": ["id", "email"]}]
        });
        let parsed: DataSourceSchema = parse_options(&value).unwrap();
        let cols = &parsed.tables[0].columns;
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name, "id");
        assert!(cols[0].type_name.is_none());
        assert!(cols[1].type_name.is_none());
    }

    #[test]
    fn data_source_schema_parses_mixed_column_shapes() {
        let value = serde_json::json!({
            "tables": [{"name": "t", "columns": ["a", {"name": "b", "type_name": "text"}, {"name": "c"}]}]
        });
        let cols = parse_options::<DataSourceSchema>(&value)
            .unwrap()
            .tables
            .remove(0)
            .columns;
        assert_eq!(cols[0].type_name, None);
        assert_eq!(cols[1].type_name.as_deref(), Some("text"));
        assert_eq!(cols[2].type_name, None);
    }

    #[test]
    fn data_source_schema_serializes_columns_as_objects() {
        let schema = DataSourceSchema {
            tables: vec![SchemaTable {
                name: "t".into(),
                columns: vec![
                    crate::connectors::ColumnSchema::typed("a", "int"),
                    crate::connectors::ColumnSchema::untyped("b"),
                ],
            }],
        };
        let json = serde_json::to_value(&schema).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"tables": [{"name": "t", "columns": [
                {"name": "a", "type_name": "int"},
                {"name": "b"}
            ]}]})
        );
    }

    #[derive(Clone, Default)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Non-object `options` must fall back to defaults and be logged; nothing is salvageable, so it
    /// comes back not silenced and surfaces as `unevaluable`.
    #[test]
    fn parse_monitor_options_defaults_and_logs_on_unparseable_input() {
        let _guard = lock_corrupt_metrics();
        let buf = SharedBuf::default();
        let writer_buf = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer_buf.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        // `[]` would deserialize into all-default options via serde's positional fallback; the
        // `is_object` guard catches it first.
        let result = parse_monitor_options(42, &serde_json::json!([]));

        assert_eq!(result, MonitorOptions::default());
        assert!(!result.is_silenced(Utc::now()));
        let text = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
        assert!(
            text.contains("monitor options are unparseable"),
            "expected a warning log, got: {text}"
        );
    }

    /// `{"op": 5}` fails to deserialize: `op` is `Option<String>` and `5` isn't a string.
    fn corrupt_op_value() -> Value {
        serde_json::json!(5)
    }

    /// Serializes tests that read the process-global `metrics::MONITOR_OPTIONS_CORRUPT`
    /// before/after values.
    fn lock_corrupt_metrics() -> std::sync::MutexGuard<'static, ()> {
        crate::metrics::MONITOR_OPTIONS_CORRUPT_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn unparseable_options_keep_an_explicit_mute() {
        let _guard = lock_corrupt_metrics();
        let value = serde_json::json!({"muted": true, "max_fresh_wait_secs": "600"});
        assert!(parse_options::<MonitorOptions>(&value).is_err());
        let result = parse_monitor_options(1, &value);
        assert!(result.is_silenced(Utc::now()));
    }

    #[test]
    fn unparseable_options_keep_an_active_snooze() {
        let _guard = lock_corrupt_metrics();
        let until = Utc::now() + chrono::Duration::hours(1);
        let value =
            serde_json::json!({"muted_until": until.to_rfc3339(), "op": corrupt_op_value()});
        assert!(parse_options::<MonitorOptions>(&value).is_err());
        let result = parse_monitor_options(1, &value);
        assert!(result.is_silenced(Utc::now()));
    }

    #[test]
    fn unparseable_options_do_not_over_suppress_an_expired_snooze() {
        let _guard = lock_corrupt_metrics();
        let until = Utc::now() - chrono::Duration::hours(1);
        let value =
            serde_json::json!({"muted_until": until.to_rfc3339(), "op": corrupt_op_value()});
        assert!(parse_options::<MonitorOptions>(&value).is_err());
        let result = parse_monitor_options(1, &value);
        assert!(!result.is_silenced(Utc::now()));
    }

    #[test]
    fn unparseable_options_fail_closed_on_an_unreadable_muted_value() {
        let _guard = lock_corrupt_metrics();
        let value = serde_json::json!({"muted": "yes", "op": corrupt_op_value()});
        assert!(parse_options::<MonitorOptions>(&value).is_err());
        let result = parse_monitor_options(1, &value);
        assert!(result.is_silenced(Utc::now()));
    }

    #[test]
    fn unparseable_options_fail_closed_on_an_unreadable_muted_until_value() {
        let _guard = lock_corrupt_metrics();
        let value = serde_json::json!({"muted_until": "garbage", "op": corrupt_op_value()});
        assert!(parse_options::<MonitorOptions>(&value).is_err());
        let result = parse_monitor_options(1, &value);
        assert!(result.is_silenced(Utc::now()));
    }

    #[test]
    fn unparseable_options_with_no_silence_key_do_not_go_dark() {
        let _guard = lock_corrupt_metrics();
        let value = serde_json::json!({"column": "x", "op": corrupt_op_value(), "value": 3});
        assert!(parse_options::<MonitorOptions>(&value).is_err());
        let result = parse_monitor_options(1, &value);
        assert!(!result.is_silenced(Utc::now()));
    }

    #[test]
    fn unparseable_options_with_a_cleared_snooze_are_not_silenced() {
        let _guard = lock_corrupt_metrics();
        let value = serde_json::json!({"muted_until": null, "op": corrupt_op_value()});
        assert!(parse_options::<MonitorOptions>(&value).is_err());
        let result = parse_monitor_options(1, &value);
        assert!(!result.is_silenced(Utc::now()));
    }

    #[test]
    fn unparseable_options_with_a_cleared_mute_are_not_silenced() {
        let _guard = lock_corrupt_metrics();
        let value = serde_json::json!({"muted": null, "op": corrupt_op_value()});
        assert!(parse_options::<MonitorOptions>(&value).is_err());
        let result = parse_monitor_options(1, &value);
        assert!(!result.is_silenced(Utc::now()));
    }

    #[test]
    fn non_object_options_increment_the_corrupt_counter() {
        let _guard = lock_corrupt_metrics();
        let before = crate::metrics::monitor_options_corrupt_count();
        let _ = parse_monitor_options(1, &serde_json::json!([]));
        let after = crate::metrics::monitor_options_corrupt_count();
        assert_eq!(after, before + 1);
    }

    #[test]
    fn deserialize_failure_options_increment_the_corrupt_counter() {
        let _guard = lock_corrupt_metrics();
        let before = crate::metrics::monitor_options_corrupt_count();
        let value = serde_json::json!({"op": corrupt_op_value()});
        let _ = parse_monitor_options(1, &value);
        let after = crate::metrics::monitor_options_corrupt_count();
        assert_eq!(after, before + 1);
    }
}

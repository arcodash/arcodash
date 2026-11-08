//! "Auto Visualize": given a query result's column metadata (never row values) and row count, asks
//! the org's AI provider to pick one of the 4 visualization types and its full config.
//!
//! Every provider returns the same flat JSON object (a `viz_type` tag plus all option fields,
//! optional), since cross-provider support for unions is inconsistent; [`project_flat_viz`] turns
//! it into the real option structs, which mirror the frontend's option shapes so a suggestion
//! persists straight into a widget's `options`.

use serde::{Deserialize, Serialize};
use std::fmt::Write;

/// Column metadata the frontend sends for a result: name and friendly type only, never a row value
/// (the privacy constraint: the AI provider sees no data).
#[derive(Debug, Clone, Deserialize)]
pub struct ColumnMeta {
    pub name: String,
    pub friendly_type: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartOptions {
    pub global_series_type: Option<String>,
    pub x_column: Option<String>,
    pub y_columns: Option<Vec<String>>,
    /// Group-by column (one series per distinct value), mirroring `ChartOptions.seriesColumn`;
    /// `None` means one series.
    pub series_column: Option<String>,
    /// One of `"none" | "sum" | "avg" | "min" | "max" | "count"`; validated in
    /// [`sanitize_chart_suggestion`], where an unrecognized value is dropped, not rejected.
    pub aggregation: Option<String>,
    pub legend: Option<bool>,
    pub stacking: Option<String>,
}

/// Friendly types the frontend's chart resolvers treat as plottable numbers (`NUMERIC_TYPES` in
/// `chart/index.tsx`); kept in sync by hand.
const NUMERIC_FRIENDLY_TYPES: &[&str] = &["integer", "float"];

/// The only `aggregation` values the chart renderer understands.
const CHART_AGGREGATIONS: &[&str] = &["none", "sum", "avg", "min", "max", "count"];

/// Validates a Chart suggestion's columns and `aggregation` against the result's real columns
/// before it's persisted.
///
/// A correctness net, not a security boundary (the model sees no row values): invalid pieces are
/// dropped so a partially-wrong answer still renders.
#[must_use]
pub fn sanitize_chart_suggestion(mut opts: ChartOptions, columns: &[ColumnMeta]) -> ChartOptions {
    use std::collections::HashMap;
    let types: HashMap<&str, &str> = columns
        .iter()
        .map(|c| (c.name.as_str(), c.friendly_type.as_str()))
        .collect();

    opts.y_columns = opts.y_columns.map(|cols| {
        cols.into_iter()
            .filter(|c| {
                types
                    .get(c.as_str())
                    .is_some_and(|t| NUMERIC_FRIENDLY_TYPES.contains(t))
            })
            .collect()
    });
    opts.aggregation = opts
        .aggregation
        .filter(|a| CHART_AGGREGATIONS.contains(&a.as_str()));
    let known = |c: &String| !c.is_empty() && types.contains_key(c.as_str());
    opts.series_column = opts
        .series_column
        .filter(|s| known(s) && opts.x_column.as_ref() != Some(s));
    // Runs after the series-column check, which needs the hallucinated xColumn to catch series ==
    // x.
    opts.x_column = opts.x_column.filter(known);

    // Grouping with no aggregation is undefined for repeated (x, series) pairs; force "sum", as the
    // chart editor does.
    if opts.series_column.is_some() && opts.aggregation.as_deref().unwrap_or("none") == "none" {
        opts.aggregation = Some("sum".to_string());
    }

    opts
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CounterOptions {
    pub column_name: Option<String>,
    pub row_number: Option<i64>,
    pub count_row: Option<bool>,
    pub string_prefix: Option<String>,
    pub string_suffix: Option<String>,
    pub target_col_name: Option<String>,
    pub target_row_index: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunnelOptions {
    pub column: Option<String>,
    pub value_column: Option<String>,
}

/// Per-column override, mirroring the frontend's `ColOverride`; `friendly_name` stays `snake_case`
/// because that's the key the table renderer reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColOverride {
    pub name: Option<String>,
    #[serde(rename = "friendly_name")]
    pub friendly_name: Option<String>,
    pub visible: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableOptions {
    pub items_per_page: Option<i64>,
    pub columns: Option<Vec<ColOverride>>,
}

/// One AI-suggested visualization with fully-typed options: the trust boundary between the model's
/// output and what is persisted as a widget's config.
#[derive(Debug, Clone)]
pub enum VizSuggestion {
    Chart(ChartOptions),
    Counter(CounterOptions),
    Funnel(FunnelOptions),
    Table(TableOptions),
}

impl VizSuggestion {
    #[must_use]
    pub const fn type_str(&self) -> &'static str {
        match self {
            Self::Chart(_) => "CHART",
            Self::Counter(_) => "COUNTER",
            Self::Funnel(_) => "FUNNEL",
            Self::Table(_) => "TABLE",
        }
    }

    #[must_use]
    pub fn options_json(&self) -> serde_json::Value {
        match self {
            Self::Chart(o) => serde_json::to_value(o),
            Self::Counter(o) => serde_json::to_value(o),
            Self::Funnel(o) => serde_json::to_value(o),
            Self::Table(o) => serde_json::to_value(o),
        }
        .unwrap_or(serde_json::Value::Null)
    }
}

/// A parsed suggestion plus token counts from the provider's response envelope (cf.
/// `SqlGeneration`).
#[derive(Debug, Clone)]
pub struct VizGeneration {
    pub title: Option<String>,
    pub suggestion: VizSuggestion,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
}

/// A field shared by every provider's flat JSON schema, tagged with its JSON-schema primitive kind.
/// Single source of truth for property names across the three provider schemas; a typo would only
/// show as an ignored field in a live response.
pub(crate) enum FieldKind {
    Str,
    StrArray,
    Bool,
    Int,
}

/// `(property name, JSON type, schema `description`)`. The description rides in the schema because
/// constrained decoding attends to it more reliably than prompt prose.
pub(crate) const VIZ_FIELDS: &[(&str, FieldKind, &str)] = &[
    ("globalSeriesType", FieldKind::Str, ""),
    ("xColumn", FieldKind::Str, ""),
    ("yColumns", FieldKind::StrArray, ""),
    (
        "seriesColumn",
        FieldKind::Str,
        "Group-by column for a CHART: one line/bar per distinct value of this column. Set this \
        whenever the result has 2+ non-numeric dimension columns, since that almost always means \
        xColumn's values repeat once per value of another dimension (e.g. one row per store per \
        month) — leave unset only when you're sure each xColumn value already appears once.",
    ),
    (
        "aggregation",
        FieldKind::Str,
        "One of \"none\", \"sum\", \"avg\", \"min\", \"max\", \"count\": how rows sharing the \
        same xColumn value (and seriesColumn value, if set) are combined into one point. Required \
        to be something other than \"none\" whenever seriesColumn is set — \"sum\" is almost \
        always right.",
    ),
    ("legend", FieldKind::Bool, ""),
    ("stacking", FieldKind::Str, ""),
    ("columnName", FieldKind::Str, ""),
    ("rowNumber", FieldKind::Int, ""),
    ("countRow", FieldKind::Bool, ""),
    ("stringPrefix", FieldKind::Str, ""),
    ("stringSuffix", FieldKind::Str, ""),
    ("targetColName", FieldKind::Str, ""),
    ("targetRowIndex", FieldKind::Int, ""),
    ("column", FieldKind::Str, ""),
    ("valueColumn", FieldKind::Str, ""),
    ("itemsPerPage", FieldKind::Int, ""),
    // Flat hide-only list (the frontend's nested `ColOverride[]` collapsed to names) keeps schemas
    // simple, esp. OpenAI strict mode.
    ("tableHiddenColumns", FieldKind::StrArray, ""),
];

pub(crate) const VIZ_TYPE_ENUM: &[&str] = &["CHART", "COUNTER", "FUNNEL", "TABLE"];

/// The flat response schema. `strict` is `OpenAI` strict mode, where every property must also be
/// `required` (so an optional field is a nullable type union) plus `additionalProperties: false`;
/// a violation is only caught by a live call. Otherwise (Anthropic) fields are plain types and
/// only `viz_type` is required. Gemini adapts the strict form.
pub(crate) fn viz_schema(strict: bool) -> serde_json::Value {
    use serde_json::json;
    let ty = |t: &str| if strict { json!([t, "null"]) } else { json!(t) };
    let mut properties = serde_json::Map::new();
    properties.insert(
        "viz_type".into(),
        json!({ "type": "string", "enum": VIZ_TYPE_ENUM }),
    );
    properties.insert("title".into(), json!({ "type": ty("string") }));
    for (name, kind, description) in VIZ_FIELDS {
        let mut schema = match kind {
            FieldKind::Str => json!({ "type": ty("string") }),
            FieldKind::Bool => json!({ "type": ty("boolean") }),
            FieldKind::Int => json!({ "type": ty("integer") }),
            FieldKind::StrArray => json!({ "type": ty("array"), "items": { "type": "string" } }),
        };
        if !description.is_empty() {
            schema["description"] = (*description).into();
        }
        properties.insert((*name).into(), schema);
    }
    if !strict {
        return json!({ "type": "object", "properties": properties, "required": ["viz_type"] });
    }
    let required: Vec<&str> = ["viz_type", "title"]
        .into_iter()
        .chain(VIZ_FIELDS.iter().map(|(name, ..)| *name))
        .collect();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// `TableOptions` from the flat `tableHiddenColumns`/`itemsPerPage` fields; its names don't match
/// the flat schema 1:1, so it can't use a plain `from_value`.
fn table_options_from_flat(flat: &serde_json::Value) -> TableOptions {
    let hidden: Vec<ColOverride> = flat
        .get("tableHiddenColumns")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|c| c.as_str())
        .map(|name| ColOverride {
            name: Some(name.to_string()),
            friendly_name: None,
            visible: Some(false),
        })
        .collect();
    TableOptions {
        items_per_page: flat
            .get("itemsPerPage")
            .and_then(sea_orm::JsonValue::as_i64),
        columns: (!hidden.is_empty()).then_some(hidden),
    }
}

/// Projects the flat JSON object every provider returns into a [`VizSuggestion`].
///
/// Unknown keys are ignored and missing ones stay `None`; an unknown/missing `viz_type`, or fields
/// of the wrong JSON type, fall back to `Table` with default options so one bad AI answer never
/// errors the feature.
pub fn project_flat_viz(flat: &serde_json::Value) -> (Option<String>, VizSuggestion) {
    let title = flat
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let viz_type = flat.get("viz_type").and_then(|v| v.as_str()).unwrap_or("");

    let parsed = match viz_type {
        "CHART" => serde_json::from_value(flat.clone()).map(VizSuggestion::Chart),
        "COUNTER" => serde_json::from_value(flat.clone()).map(VizSuggestion::Counter),
        "FUNNEL" => serde_json::from_value(flat.clone()).map(VizSuggestion::Funnel),
        "TABLE" => Ok(VizSuggestion::Table(table_options_from_flat(flat))),
        _ => Ok(VizSuggestion::Table(TableOptions::default())),
    };

    (
        title,
        parsed.unwrap_or_else(|_| VizSuggestion::Table(TableOptions::default())),
    )
}

/// Describes the 4 visualization types, the result's columns (name + friendly type) and row count,
/// and asks for a concise title; no row values appear.
///
/// `row_count` is safe aggregate metadata that helps prefer Counter or Funnel.
#[must_use]
pub fn build_viz_prompt(columns: &[ColumnMeta], row_count: usize) -> String {
    let mut prompt = String::from(
        "You are picking the best visualization for a SQL query result, from exactly these four types:\n\
        - CHART: a line/bar/area/pie/scatter chart. Fields: globalSeriesType (one of \"line\", \"bar\", \"area\", \"pie\", \"scatter\"), xColumn, yColumns (array of column names — numeric columns only), seriesColumn (optional group-by column — one line/bar per distinct value), aggregation (one of \"none\", \"sum\", \"avg\", \"min\", \"max\", \"count\" — how rows sharing the same x value, and group if seriesColumn is set, are combined), legend (boolean), stacking (one of \"\", \"normal\", \"percent\").\n\
        - COUNTER: a single big number, best when the result is a single row. Fields: columnName, rowNumber (1-indexed row to read), countRow (boolean — counts result rows instead of reading a column), stringPrefix, stringSuffix, targetColName, targetRowIndex.\n\
        - FUNNEL: an ordered set of labeled stages with values, best for a small number of staged/ranked rows. Fields: column (the label column), valueColumn (the value column).\n\
        - TABLE: a plain data table — the safe default for tabular results or when nothing else clearly fits. Fields: itemsPerPage, tableHiddenColumns (array of column names to hide).\n\n",
    );
    let _ = writeln!(prompt, "Row count: {row_count}");
    prompt.push_str("Columns (name (friendly_type)):\n");
    let is_numeric = |c: &ColumnMeta| NUMERIC_FRIENDLY_TYPES.contains(&c.friendly_type.as_str());
    let (numeric, dimensions): (Vec<&ColumnMeta>, Vec<&ColumnMeta>) =
        columns.iter().partition(|c| is_numeric(c));
    for col in columns {
        let role = if is_numeric(col) {
            "measure"
        } else {
            "dimension"
        };
        let _ = writeln!(prompt, "- {} ({}, {})", col.name, col.friendly_type, role);
    }
    let _ = write!(
        prompt,
        "\n{} measure column(s) (numeric — plottable as Y/value) and {} dimension column(s) \
        (non-numeric — plottable as X or group-by) above.\n",
        numeric.len(),
        dimensions.len()
    );
    if dimensions.len() >= 2 {
        let dim_names: Vec<&str> = dimensions.iter().map(|c| c.name.as_str()).collect();
        // Prefer a date/datetime dimension for the example's xColumn, as the instruction below
        // asks.
        let example_x = dimensions
            .iter()
            .find(|c| c.friendly_type == "date" || c.friendly_type == "datetime")
            .or_else(|| dimensions.first())
            .map_or("", |c| c.name.as_str());
        let example_series = dimensions
            .iter()
            .map(|c| c.name.as_str())
            .find(|n| *n != example_x)
            .unwrap_or("");
        let example_y = numeric.first().map_or("", |c| c.name.as_str());
        let _ = write!(
            prompt,
            "This result has {} dimension columns: {}. With more than one dimension column, the \
            same X value almost always repeats once per distinct value of another dimension \
            (e.g. {} rows because one row exists per {} per {}) rather than each row being a \
            unique point — this is the single most common mistake when picking a CHART for this \
            kind of result. Plotting it as a plain line/bar with no group-by silently collapses \
            every dimension onto one axis and produces a meaningless sawtooth line.\n\
            \n\
            MANDATORY for a CHART when there are 2+ dimension columns: set xColumn to one \
            dimension (prefer a date/datetime one, for a time series), set seriesColumn to a \
            DIFFERENT dimension from {:?} so each distinct value gets its own line/bar, and set \
            aggregation to \"sum\" so rows sharing the same (x, series) pair combine into one \
            point. Do not leave seriesColumn unset for this result unless you have concrete \
            evidence that xColumn's values are already unique — with {} dimension columns and \
            {} rows, assume they are not.\n\
            \n\
            A correctly-filled CHART response for exactly this result's columns looks like this \
            (adjust globalSeriesType/legend/stacking/title to taste, but keep xColumn/yColumns/\
            seriesColumn/aggregation this shape): {{\"viz_type\": \"CHART\", \"title\": \"...\", \
            \"xColumn\": \"{example_x}\", \"yColumns\": [\"{example_y}\"], \"seriesColumn\": \
            \"{example_series}\", \"aggregation\": \"sum\"}}\n",
            dimensions.len(),
            dim_names.join(", "),
            row_count,
            dim_names.first().copied().unwrap_or(""),
            dim_names.get(1).copied().unwrap_or(""),
            dim_names,
            dimensions.len(),
            row_count,
        );
    } else {
        prompt.push_str(
            "This result has at most one dimension column, so a CHART's rows are already one \
            point per X value — leave seriesColumn unset and aggregation \"none\" unless you have \
            a specific reason to combine rows.\n",
        );
    }
    prompt.push_str(
        "\nRespond with exactly one flat JSON object: viz_type (one of CHART, COUNTER, FUNNEL, TABLE), \
        a short concise title for this visualization, and only the fields relevant to your chosen \
        viz_type. Do not include row values — none are provided, and none should appear in your response.",
    );
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns() -> Vec<ColumnMeta> {
        vec![
            ColumnMeta {
                name: "status".to_string(),
                friendly_type: "string".to_string(),
            },
            ColumnMeta {
                name: "count".to_string(),
                friendly_type: "integer".to_string(),
            },
        ]
    }

    #[test]
    fn projects_chart() {
        let flat = serde_json::json!({
            "viz_type": "CHART",
            "title": "Status breakdown",
            "globalSeriesType": "bar",
            "xColumn": "status",
            "yColumns": ["count"],
            "legend": true,
            "stacking": ""
        });
        let (title, suggestion) = project_flat_viz(&flat);
        assert_eq!(title, Some("Status breakdown".to_string()));
        match suggestion {
            VizSuggestion::Chart(opts) => {
                assert_eq!(opts.global_series_type, Some("bar".to_string()));
                assert_eq!(opts.x_column, Some("status".to_string()));
                assert_eq!(opts.y_columns, Some(vec!["count".to_string()]));
                assert_eq!(opts.legend, Some(true));
            }
            other => panic!("expected Chart, got {other:?}"),
        }
    }

    #[test]
    fn projects_counter() {
        let flat = serde_json::json!({
            "viz_type": "COUNTER",
            "columnName": "count",
            "countRow": false
        });
        let (_, suggestion) = project_flat_viz(&flat);
        match suggestion {
            VizSuggestion::Counter(opts) => {
                assert_eq!(opts.column_name, Some("count".to_string()));
                assert_eq!(opts.count_row, Some(false));
            }
            other => panic!("expected Counter, got {other:?}"),
        }
    }

    #[test]
    fn projects_funnel() {
        let flat = serde_json::json!({
            "viz_type": "FUNNEL",
            "column": "status",
            "valueColumn": "count"
        });
        let (_, suggestion) = project_flat_viz(&flat);
        match suggestion {
            VizSuggestion::Funnel(opts) => {
                assert_eq!(opts.column, Some("status".to_string()));
                assert_eq!(opts.value_column, Some("count".to_string()));
            }
            other => panic!("expected Funnel, got {other:?}"),
        }
    }

    #[test]
    fn projects_table_with_hidden_columns() {
        let flat = serde_json::json!({
            "viz_type": "TABLE",
            "itemsPerPage": 25,
            "tableHiddenColumns": ["status"]
        });
        let (_, suggestion) = project_flat_viz(&flat);
        match suggestion {
            VizSuggestion::Table(opts) => {
                assert_eq!(opts.items_per_page, Some(25));
                let cols = opts
                    .columns
                    .expect("hidden columns should populate overrides");
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].name, Some("status".to_string()));
                assert_eq!(cols[0].visible, Some(false));
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[test]
    fn unknown_viz_type_falls_back_to_table() {
        let flat = serde_json::json!({ "viz_type": "PIE_CHART_3D", "title": "Nope" });
        let (title, suggestion) = project_flat_viz(&flat);
        assert_eq!(title, Some("Nope".to_string()));
        assert!(matches!(suggestion, VizSuggestion::Table(_)));
    }

    #[test]
    fn missing_viz_type_falls_back_to_table() {
        let flat = serde_json::json!({ "title": "No type at all" });
        let (_, suggestion) = project_flat_viz(&flat);
        assert!(matches!(suggestion, VizSuggestion::Table(_)));
    }

    #[test]
    fn garbage_fields_for_a_known_type_falls_back_to_table() {
        // yColumns should be an array: a lone string is a type mismatch that must fail Chart's
        // deserialize and fall back, not panic.
        let flat = serde_json::json!({
            "viz_type": "CHART",
            "yColumns": "not-an-array"
        });
        let (_, suggestion) = project_flat_viz(&flat);
        assert!(matches!(suggestion, VizSuggestion::Table(_)));
    }

    #[test]
    fn missing_title_is_none() {
        let flat = serde_json::json!({ "viz_type": "TABLE" });
        let (title, _) = project_flat_viz(&flat);
        assert_eq!(title, None);
    }

    #[test]
    fn table_override_friendly_name_round_trips_snake_case() {
        let opts = TableOptions {
            items_per_page: Some(10),
            columns: Some(vec![ColOverride {
                name: Some("status".to_string()),
                friendly_name: Some("Status".to_string()),
                visible: Some(true),
            }]),
        };
        let json = serde_json::to_value(&opts).unwrap();
        let col = &json["columns"][0];
        assert_eq!(col["friendly_name"], "Status");
        assert!(col.get("friendlyName").is_none());
        assert_eq!(json["itemsPerPage"], 10);

        let back: TableOptions = serde_json::from_value(json).unwrap();
        assert_eq!(
            back.columns.unwrap()[0].friendly_name,
            Some("Status".to_string())
        );
    }

    #[test]
    fn build_viz_prompt_lists_columns_and_row_count_without_row_values() {
        let prompt = build_viz_prompt(&columns(), 42);
        assert!(prompt.contains("status (string, dimension)"));
        assert!(prompt.contains("count (integer, measure)"));
        assert!(prompt.contains("Row count: 42"));
        assert!(prompt.contains("CHART"));
        assert!(prompt.contains("COUNTER"));
        assert!(prompt.contains("FUNNEL"));
        assert!(prompt.contains("TABLE"));
    }

    #[test]
    fn build_viz_prompt_flags_repeating_x_with_two_or_more_dimensions() {
        let cols = vec![
            ColumnMeta {
                name: "store".to_string(),
                friendly_type: "string".to_string(),
            },
            ColumnMeta {
                name: "month".to_string(),
                friendly_type: "date".to_string(),
            },
            ColumnMeta {
                name: "net_revenue".to_string(),
                friendly_type: "float".to_string(),
            },
        ];
        let prompt = build_viz_prompt(&cols, 328);
        assert!(prompt.contains("2 dimension column(s)"));
        assert!(prompt.contains("1 measure column(s)"));
        assert!(prompt.contains("seriesColumn"));
        assert!(prompt.to_lowercase().contains("sawtooth"));
    }

    #[test]
    fn build_viz_prompt_does_not_suggest_grouping_with_one_dimension() {
        let prompt = build_viz_prompt(&columns(), 10);
        assert!(prompt.contains("at most one dimension column"));
        assert!(!prompt.to_lowercase().contains("sawtooth"));
    }

    fn store_month_revenue_columns() -> Vec<ColumnMeta> {
        vec![
            ColumnMeta {
                name: "store".to_string(),
                friendly_type: "string".to_string(),
            },
            ColumnMeta {
                name: "month".to_string(),
                friendly_type: "date".to_string(),
            },
            ColumnMeta {
                name: "net_revenue".to_string(),
                friendly_type: "float".to_string(),
            },
        ]
    }

    #[test]
    fn sanitize_keeps_valid_group_by_and_forces_sum() {
        let opts = ChartOptions {
            global_series_type: Some("line".to_string()),
            x_column: Some("month".to_string()),
            y_columns: Some(vec!["net_revenue".to_string()]),
            series_column: Some("store".to_string()),
            aggregation: None,
            legend: Some(true),
            stacking: None,
        };
        let sanitized = sanitize_chart_suggestion(opts, &store_month_revenue_columns());
        assert_eq!(sanitized.series_column, Some("store".to_string()));
        assert_eq!(sanitized.aggregation, Some("sum".to_string()));
        assert_eq!(sanitized.y_columns, Some(vec!["net_revenue".to_string()]));
    }

    #[test]
    fn sanitize_drops_series_column_not_in_result() {
        let opts = ChartOptions {
            x_column: Some("month".to_string()),
            y_columns: Some(vec!["net_revenue".to_string()]),
            series_column: Some("region".to_string()),
            ..Default::default()
        };
        let sanitized = sanitize_chart_suggestion(opts, &store_month_revenue_columns());
        assert_eq!(sanitized.series_column, None);
        // No group-by survived, so nothing forces an aggregation.
        assert_eq!(sanitized.aggregation, None);
    }

    #[test]
    fn sanitize_drops_series_column_equal_to_x_column() {
        let opts = ChartOptions {
            x_column: Some("month".to_string()),
            y_columns: Some(vec!["net_revenue".to_string()]),
            series_column: Some("month".to_string()),
            ..Default::default()
        };
        let sanitized = sanitize_chart_suggestion(opts, &store_month_revenue_columns());
        assert_eq!(sanitized.series_column, None);
    }

    #[test]
    fn sanitize_drops_non_numeric_y_columns() {
        let opts = ChartOptions {
            x_column: Some("month".to_string()),
            y_columns: Some(vec!["net_revenue".to_string(), "store".to_string()]),
            ..Default::default()
        };
        let sanitized = sanitize_chart_suggestion(opts, &store_month_revenue_columns());
        assert_eq!(sanitized.y_columns, Some(vec!["net_revenue".to_string()]));
    }

    #[test]
    fn sanitize_drops_unsupported_aggregation() {
        let opts = ChartOptions {
            x_column: Some("month".to_string()),
            y_columns: Some(vec!["net_revenue".to_string()]),
            aggregation: Some("median".to_string()),
            ..Default::default()
        };
        let sanitized = sanitize_chart_suggestion(opts, &store_month_revenue_columns());
        assert_eq!(sanitized.aggregation, None);
    }

    #[test]
    fn sanitize_drops_an_x_column_that_is_not_a_real_column() {
        let opts = ChartOptions {
            x_column: Some("nonexistent".to_string()),
            y_columns: Some(vec!["net_revenue".to_string()]),
            ..Default::default()
        };
        let sanitized = sanitize_chart_suggestion(opts, &store_month_revenue_columns());
        assert_eq!(sanitized.x_column, None);
    }

    #[test]
    fn sanitize_keeps_a_valid_x_column() {
        let opts = ChartOptions {
            x_column: Some("month".to_string()),
            y_columns: Some(vec!["net_revenue".to_string()]),
            ..Default::default()
        };
        let sanitized = sanitize_chart_suggestion(opts, &store_month_revenue_columns());
        assert_eq!(sanitized.x_column, Some("month".to_string()));
    }

    #[test]
    fn sanitize_still_clears_a_series_equal_to_a_hallucinated_x() {
        // Both xColumn and seriesColumn are hallucinated but equal; the series check must fire
        // before xColumn is cleared.
        let opts = ChartOptions {
            x_column: Some("bogus".to_string()),
            y_columns: Some(vec!["net_revenue".to_string()]),
            series_column: Some("bogus".to_string()),
            ..Default::default()
        };
        let sanitized = sanitize_chart_suggestion(opts, &store_month_revenue_columns());
        assert_eq!(sanitized.series_column, None);
        assert_eq!(sanitized.x_column, None);
    }

    #[test]
    fn sanitize_is_a_no_op_with_neither_new_field_set() {
        let opts = ChartOptions {
            global_series_type: Some("bar".to_string()),
            x_column: Some("month".to_string()),
            y_columns: Some(vec!["net_revenue".to_string()]),
            legend: Some(false),
            stacking: Some("".to_string()),
            ..Default::default()
        };
        let before = opts.clone();
        let sanitized = sanitize_chart_suggestion(opts, &store_month_revenue_columns());
        assert_eq!(sanitized.global_series_type, before.global_series_type);
        assert_eq!(sanitized.x_column, before.x_column);
        assert_eq!(sanitized.y_columns, before.y_columns);
        assert_eq!(sanitized.legend, before.legend);
        assert_eq!(sanitized.stacking, before.stacking);
        assert_eq!(sanitized.series_column, None);
        assert_eq!(sanitized.aggregation, None);
    }
}

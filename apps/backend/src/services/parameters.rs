//! Parameter rendering: pure functions that turn a query's text, saved parameter definitions and
//! runtime values into the final SQL.
//!
//! Tokens are `{{ name }}` or `{{ name.start }}`/`{{ name.end }}` (range types only); undefined
//! names and missing values are render errors. Number, Date and Dropdown values are validated and
//! re-emitted; Text and `QueryDropdown` are raw drop-ins, which is why they require manage access
//! ([`manage_access_param`]).

use std::collections::HashMap;
use std::sync::LazyLock;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime};
use regex::Regex;
use serde_json::Value;

use arcodash_core::models::{ParameterType, QueryParameterDefinition};

/// Matches `{{ name }}` or `{{ name.start }}`/`{{ name.end }}` with optional inner whitespace;
/// group 1 = base name, group 2 = optional `start`/`end` suffix.
static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{\{\s*([A-Za-z0-9_\-]+)(?:\.(start|end))?\s*\}\}")
        .expect("static regex must compile")
});

/// Render `query_text`, substituting every token from `definitions` and `values` (falling back to
/// each definition's default). Errors on the first unresolvable token; partial substitution is
/// never returned.
pub fn render_query(
    query_text: &str,
    definitions: &[QueryParameterDefinition],
    values: &Value,
) -> anyhow::Result<String> {
    let by_name: HashMap<&str, &QueryParameterDefinition> =
        definitions.iter().map(|d| (d.name.as_str(), d)).collect();

    let values_obj = values.as_object();

    let mut error: Option<anyhow::Error> = None;
    let rendered = TOKEN_RE.replace_all(query_text, |caps: &regex::Captures<'_>| {
        let accessor = caps.get(2).map(|m| m.as_str());
        render_token(&caps[1], accessor, &by_name, values_obj).unwrap_or_else(|e| {
            error.get_or_insert(e);
            String::new()
        })
    });

    error.map_or_else(|| Ok(rendered.into_owned()), Err)
}

fn render_token(
    name: &str,
    accessor: Option<&str>,
    by_name: &HashMap<&str, &QueryParameterDefinition>,
    values_obj: Option<&serde_json::Map<String, Value>>,
) -> anyhow::Result<String> {
    let def = by_name.get(name).ok_or_else(|| {
        anyhow::anyhow!("{{{{ {name} }}}}: no parameter named \"{name}\" is defined for this query")
    })?;

    let is_range = matches!(
        def.param_type,
        ParameterType::DateRange | ParameterType::DateTimeRange
    );

    match (is_range, accessor) {
        (true, None) => Err(anyhow::anyhow!(
            "{{{{ {name} }}}}: \"{name}\" is a range parameter — use {{{{ {name}.start }}}} or {{{{ {name}.end }}}}"
        )),
        (false, Some(suffix)) => Err(anyhow::anyhow!(
            "{{{{ {name}.{suffix} }}}}: \"{name}\" is not a range parameter — use {{{{ {name} }}}}"
        )),
        (true, Some(suffix)) => render_range(name, suffix, def, values_obj),
        (false, None) => render_scalar(name, def, values_obj),
    }
}

/// Resolve the effective runtime value for `name`: the request's supplied value if present,
/// else the definition's own default, else a render error.
fn resolve_value<'a>(
    name: &str,
    def: &'a QueryParameterDefinition,
    values_obj: Option<&'a serde_json::Map<String, Value>>,
) -> anyhow::Result<&'a Value> {
    if let Some(v) = values_obj.and_then(|m| m.get(name)) {
        return Ok(v);
    }
    def.value
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("{{{{ {name} }}}}: no value supplied and no default set"))
}

fn render_range(
    name: &str,
    suffix: &str,
    def: &QueryParameterDefinition,
    values_obj: Option<&serde_json::Map<String, Value>>,
) -> anyhow::Result<String> {
    let value = resolve_value(name, def, values_obj)?;
    let obj = value.as_object().ok_or_else(|| {
        anyhow::anyhow!(
            "{{{{ {name}.{suffix} }}}}: range parameter \"{name}\" value must be an object with \"start\"/\"end\""
        )
    })?;
    let part = obj.get(suffix).ok_or_else(|| {
        anyhow::anyhow!(
            "{{{{ {name}.{suffix} }}}}: range parameter \"{name}\" is missing \"{suffix}\""
        )
    })?;
    match def.param_type {
        ParameterType::DateRange => render_date(name, part),
        ParameterType::DateTimeRange => render_datetime(name, part),
        _ => unreachable!("render_range only called for range types"),
    }
}

fn render_scalar(
    name: &str,
    def: &QueryParameterDefinition,
    values_obj: Option<&serde_json::Map<String, Value>>,
) -> anyhow::Result<String> {
    let value = resolve_value(name, def, values_obj)?;

    if let (Some(mv), ParameterType::Dropdown | ParameterType::QueryDropdown) =
        (&def.multi_values_options, &def.param_type)
    {
        let items = value.as_array().ok_or_else(|| {
            anyhow::anyhow!(
                "{{{{ {name} }}}}: multi-select parameter \"{name}\" value must be a JSON array"
            )
        })?;
        return Ok(items
            .iter()
            .map(|item| {
                let literal = match def.param_type {
                    ParameterType::Dropdown => render_dropdown_member(name, def, item)?,
                    ParameterType::QueryDropdown => to_sql_literal(item)?,
                    _ => unreachable!("multi only set for dropdown types"),
                };
                Ok(format!("{}{}{}", mv.prefix, literal, mv.suffix))
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .join(&mv.separator));
    }

    match def.param_type {
        ParameterType::Number => render_number(name, value),
        ParameterType::Date => render_date(name, value),
        ParameterType::Dropdown => render_dropdown_member(name, def, value),
        ParameterType::Text | ParameterType::QueryDropdown => to_sql_literal(value),
        ParameterType::DateRange | ParameterType::DateTimeRange => {
            unreachable!("range types are handled by render_range, not render_scalar")
        }
    }
}

/// Render a resolved JSON value as raw, unescaped literal text. Used only for `Text` and
/// `QueryDropdown`, which stay behind the manage-access gate in [`manage_access_param`].
fn to_sql_literal(value: &Value) -> anyhow::Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Null => Err(anyhow::anyhow!("parameter value is null")),
        other => Ok(other.to_string()),
    }
}

/// Number: accepts a JSON number or a string parsing cleanly as an integer or finite float;
/// rejecting anything else neutralizes UNION payloads.
fn render_number(name: &str, value: &Value) -> anyhow::Result<String> {
    match value {
        Value::Number(n) => Ok(n.to_string()),
        Value::String(s) => {
            let t = s.trim();
            if let Ok(i) = t.parse::<i64>() {
                return Ok(i.to_string());
            }
            match t.parse::<f64>() {
                Ok(f) if f.is_finite() => Ok(f.to_string()),
                Ok(_) => Err(anyhow::anyhow!(
                    "{{{{ {name} }}}}: number parameter value is not finite"
                )),
                Err(_) => Err(anyhow::anyhow!(
                    "{{{{ {name} }}}}: number parameter value \"{s}\" is not a valid number"
                )),
            }
        }
        _ => Err(anyhow::anyhow!(
            "{{{{ {name} }}}}: number parameter value must be a number"
        )),
    }
}

/// Date: strictly YYYY-MM-DD, re-emitted canonically so only date characters reach the SQL.
fn render_date(name: &str, value: &Value) -> anyhow::Result<String> {
    let s = value.as_str().ok_or_else(|| {
        anyhow::anyhow!("{{{{ {name} }}}}: date parameter value must be a string")
    })?;
    let d = NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").map_err(|_| {
        anyhow::anyhow!(
            "{{{{ {name} }}}}: date parameter value \"{s}\" is not a valid YYYY-MM-DD date"
        )
    })?;
    Ok(d.format("%Y-%m-%d").to_string())
}

/// Datetime (range halves): accepts RFC3339 or naive YYYY-MM-DD[T ]HH:MM:SS; re-emitted in
/// canonical form.
fn render_datetime(name: &str, value: &Value) -> anyhow::Result<String> {
    let s = value.as_str().ok_or_else(|| {
        anyhow::anyhow!("{{{{ {name} }}}}: datetime parameter value must be a string")
    })?;
    let t = s.trim();
    if let Ok(dt) = DateTime::<FixedOffset>::parse_from_rfc3339(t) {
        return Ok(dt.to_rfc3339());
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(t, fmt) {
            return Ok(dt.format("%Y-%m-%d %H:%M:%S").to_string());
        }
    }
    Err(anyhow::anyhow!(
        "{{{{ {name} }}}}: datetime parameter value \"{s}\" is not a valid date-time"
    ))
}

/// Dropdown: the value must exactly match one of the definition's own (author-controlled, trusted)
/// `enum_options` lines.
fn render_dropdown_member(
    name: &str,
    def: &QueryParameterDefinition,
    value: &Value,
) -> anyhow::Result<String> {
    let s = value.as_str().ok_or_else(|| {
        anyhow::anyhow!("{{{{ {name} }}}}: dropdown parameter value must be a string")
    })?;
    let opts = def.enum_options.as_deref().unwrap_or("");
    if opts.lines().any(|opt| opt == s) {
        Ok(s.to_string())
    } else {
        Err(anyhow::anyhow!(
            "{{{{ {name} }}}}: \"{s}\" is not a valid option for dropdown parameter \"{name}\""
        ))
    }
}

/// Base parameter names referenced by a live `{{ token }}` in `sql`, built on [`TOKEN_RE`] so
/// save-time pruning and execute-time filtering agree on what is "referenced".
pub fn referenced_names(sql: &str) -> std::collections::HashSet<String> {
    TOKEN_RE
        .captures_iter(sql)
        .map(|caps| caps[1].to_string())
        .collect()
}

/// The first live-referenced parameter whose value goes into the SQL raw and so requires
/// data-source manage access: Text and `QueryDropdown`.
///
/// Other types are validated during rendering. Orphan definitions (token deleted from the SQL) are
/// ignored.
#[must_use]
pub fn manage_access_param(sql: &str, definitions: &[QueryParameterDefinition]) -> Option<String> {
    let live = referenced_names(sql);
    definitions
        .iter()
        .find(|d| {
            live.contains(&d.name)
                && matches!(
                    d.param_type,
                    ParameterType::Text | ParameterType::QueryDropdown
                )
        })
        .map(|d| d.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_param(name: &str) -> QueryParameterDefinition {
        param_of(name, ParameterType::Text)
    }

    fn param_of(name: &str, t: ParameterType) -> QueryParameterDefinition {
        QueryParameterDefinition {
            name: name.to_string(),
            title: name.to_string(),
            param_type: t,
            value: None,
            enum_options: None,
            query_id: None,
            multi_values_options: None,
        }
    }

    #[test]
    fn text_param_substitutes_raw_unescaped() {
        let defs = vec![text_param("name")];
        let values = json!({"name": "O'Brien"});
        let rendered =
            render_query("SELECT * FROM t WHERE n = '{{ name }}'", &defs, &values).unwrap();
        assert_eq!(rendered, "SELECT * FROM t WHERE n = 'O'Brien'");
    }

    #[test]
    fn number_param_substitutes_plain_numeric_literal() {
        let defs = vec![param_of("limit", ParameterType::Number)];
        let values = json!({"limit": 42});
        let rendered = render_query("SELECT * FROM t LIMIT {{ limit }}", &defs, &values).unwrap();
        assert_eq!(rendered, "SELECT * FROM t LIMIT 42");
    }

    #[test]
    fn date_param_substitutes_iso_date_string() {
        let defs = vec![param_of("d", ParameterType::Date)];
        let values = json!({"d": "2026-06-24"});
        let rendered = render_query("WHERE created = '{{ d }}'", &defs, &values).unwrap();
        assert_eq!(rendered, "WHERE created = '2026-06-24'");
    }

    #[test]
    fn date_range_substitutes_start_and_end() {
        let defs = vec![param_of("range", ParameterType::DateRange)];
        let values = json!({"range": {"start": "2026-01-01", "end": "2026-01-31"}});
        let rendered = render_query(
            "WHERE d BETWEEN '{{ range.start }}' AND '{{ range.end }}'",
            &defs,
            &values,
        )
        .unwrap();
        assert_eq!(rendered, "WHERE d BETWEEN '2026-01-01' AND '2026-01-31'");
    }

    #[test]
    fn datetime_range_substitutes_start_and_end() {
        let defs = vec![param_of("range", ParameterType::DateTimeRange)];
        let values =
            json!({"range": {"start": "2026-01-01T00:00:00Z", "end": "2026-01-31T23:59:59Z"}});
        let rendered = render_query(
            "WHERE ts BETWEEN '{{ range.start }}' AND '{{ range.end }}'",
            &defs,
            &values,
        )
        .unwrap();
        assert_eq!(
            rendered,
            "WHERE ts BETWEEN '2026-01-01T00:00:00+00:00' AND '2026-01-31T23:59:59+00:00'"
        );
    }

    #[test]
    fn date_range_injected_start_is_rejected() {
        let defs = vec![param_of("range", ParameterType::DateRange)];
        let values = json!({"range": {"start": "2026-01-01' OR '1'='1", "end": "2026-01-31"}});
        let err = render_query(
            "WHERE d BETWEEN '{{ range.start }}' AND '{{ range.end }}'",
            &defs,
            &values,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a valid"), "got: {err}");
    }

    #[test]
    fn datetime_range_injected_end_is_rejected() {
        let defs = vec![param_of("range", ParameterType::DateTimeRange)];
        let values = json!({"range": {"start": "2026-01-01T00:00:00Z", "end": "x' OR 1=1--"}});
        let err = render_query(
            "WHERE ts BETWEEN '{{ range.start }}' AND '{{ range.end }}'",
            &defs,
            &values,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a valid"), "got: {err}");
    }

    #[test]
    fn bare_range_token_without_accessor_is_a_render_error() {
        let defs = vec![param_of("range", ParameterType::DateRange)];
        let values = json!({"range": {"start": "2026-01-01", "end": "2026-01-31"}});
        let err = render_query("WHERE d = '{{ range }}'", &defs, &values).unwrap_err();
        assert!(err.to_string().contains("range parameter"));
    }

    #[test]
    fn accessor_on_non_range_param_is_a_render_error() {
        let defs = vec![text_param("name")];
        let values = json!({"name": "x"});
        let err = render_query("'{{ name.start }}'", &defs, &values).unwrap_err();
        assert!(err.to_string().contains("not a range parameter"));
    }

    #[test]
    fn dropdown_single_value_substitutes_selection() {
        let defs = vec![QueryParameterDefinition {
            enum_options: Some("a\nb\nc".to_string()),
            ..param_of("choice", ParameterType::Dropdown)
        }];
        let values = json!({"choice": "b"});
        let rendered = render_query("WHERE c = '{{ choice }}'", &defs, &values).unwrap();
        assert_eq!(rendered, "WHERE c = 'b'");
    }

    #[test]
    fn dropdown_value_not_in_enum_options_is_rejected() {
        let defs = vec![QueryParameterDefinition {
            enum_options: Some("a\nb\nc".to_string()),
            ..param_of("choice", ParameterType::Dropdown)
        }];
        let values = json!({"choice": "x' OR 1=1"});
        let err = render_query("WHERE c = '{{ choice }}'", &defs, &values).unwrap_err();
        assert!(err.to_string().contains("not a valid option"), "got: {err}");
    }

    #[test]
    fn query_dropdown_single_value_substitutes_selection() {
        let defs = vec![QueryParameterDefinition {
            query_id: Some(7),
            ..param_of("choice", ParameterType::QueryDropdown)
        }];
        let values = json!({"choice": "42"});
        let rendered = render_query("WHERE id = {{ choice }}", &defs, &values).unwrap();
        assert_eq!(rendered, "WHERE id = 42");
    }

    #[test]
    fn multi_select_joins_with_default_prefix_suffix_separator() {
        let defs = vec![QueryParameterDefinition {
            enum_options: Some("a\nb\nc".to_string()),
            multi_values_options: Some(arcodash_core::models::MultiValuesOptions::default()),
            ..param_of("choices", ParameterType::Dropdown)
        }];
        let values = json!({"choices": ["a", "b", "c"]});
        let rendered = render_query("WHERE c IN ({{ choices }})", &defs, &values).unwrap();
        assert_eq!(rendered, "WHERE c IN ('a','b','c')");
    }

    #[test]
    fn multi_select_honors_custom_prefix_suffix_separator() {
        let defs = vec![QueryParameterDefinition {
            enum_options: Some("1\n2\n3".to_string()),
            multi_values_options: Some(arcodash_core::models::MultiValuesOptions {
                prefix: "".to_string(),
                suffix: "".to_string(),
                separator: " | ".to_string(),
            }),
            ..param_of("choices", ParameterType::Dropdown)
        }];
        let values = json!({"choices": ["1", "2", "3"]});
        let rendered = render_query("WHERE c IN ({{ choices }})", &defs, &values).unwrap();
        assert_eq!(rendered, "WHERE c IN (1 | 2 | 3)");
    }

    #[test]
    fn multi_select_dropdown_non_member_element_is_rejected() {
        let defs = vec![QueryParameterDefinition {
            enum_options: Some("a\nb\nc".to_string()),
            multi_values_options: Some(arcodash_core::models::MultiValuesOptions::default()),
            ..param_of("choices", ParameterType::Dropdown)
        }];
        let values = json!({"choices": ["a", "x' OR 1=1"]});
        let err = render_query("WHERE c IN ({{ choices }})", &defs, &values).unwrap_err();
        assert!(err.to_string().contains("not a valid option"), "got: {err}");
    }

    #[test]
    fn missing_definition_is_a_render_error_not_empty_substitution() {
        let defs: Vec<QueryParameterDefinition> = vec![];
        let values = json!({});
        let err = render_query("WHERE n = '{{ undefined_param }}'", &defs, &values).unwrap_err();
        assert!(err.to_string().contains("undefined_param"));
        assert!(err.to_string().contains("no parameter named"));
    }

    #[test]
    fn missing_value_and_no_default_is_a_render_error() {
        let defs = vec![text_param("name")];
        let values = json!({});
        let err = render_query("'{{ name }}'", &defs, &values).unwrap_err();
        assert!(err.to_string().contains("no value supplied"));
    }

    #[test]
    fn falls_back_to_definition_default_value_when_request_omits_it() {
        let defs = vec![QueryParameterDefinition {
            value: Some(json!("default-value")),
            ..text_param("name")
        }];
        let values = json!({});
        let rendered = render_query("'{{ name }}'", &defs, &values).unwrap();
        assert_eq!(rendered, "'default-value'");
    }

    #[test]
    fn repeated_token_substituted_at_every_occurrence() {
        let defs = vec![param_of("x", ParameterType::Number)];
        let values = json!({"x": 5});
        let rendered = render_query("{{ x }} + {{ x }} = {{ x }}", &defs, &values).unwrap();
        assert_eq!(rendered, "5 + 5 = 5");
    }

    #[test]
    fn whitespace_variants_in_token_are_equivalent() {
        let defs = vec![text_param("x")];
        let values = json!({"x": "hello"});
        let r1 = render_query("{{x}}", &defs, &values).unwrap();
        let r2 = render_query("{{ x }}", &defs, &values).unwrap();
        let r3 = render_query("{{  x  }}", &defs, &values).unwrap();
        assert_eq!(r1, "hello");
        assert_eq!(r1, r2);
        assert_eq!(r1, r3);
    }

    #[test]
    fn hyphen_and_underscore_in_name_are_accepted() {
        let defs = vec![text_param("my-param_1")];
        let values = json!({"my-param_1": "val"});
        let rendered = render_query("{{ my-param_1 }}", &defs, &values).unwrap();
        assert_eq!(rendered, "val");
    }

    // A name with an invalid char (e.g. space) is not matched by the regex and stays untouched.
    #[test]
    fn invalid_char_in_token_name_is_left_untouched() {
        let defs: Vec<QueryParameterDefinition> = vec![];
        let values = json!({});
        let rendered = render_query("{{ bad name }}", &defs, &values).unwrap();
        assert_eq!(rendered, "{{ bad name }}");
    }

    #[test]
    fn adjacent_tokens_both_substitute() {
        let defs = vec![
            param_of("a", ParameterType::Number),
            param_of("b", ParameterType::Number),
        ];
        let values = json!({"a": 1, "b": 2});
        let rendered = render_query("{{a}}{{b}}", &defs, &values).unwrap();
        assert_eq!(rendered, "12");
    }

    #[test]
    fn query_dropdown_multi_select_joins_correctly() {
        let defs = vec![QueryParameterDefinition {
            query_id: Some(7),
            multi_values_options: Some(arcodash_core::models::MultiValuesOptions::default()),
            ..param_of("ids", ParameterType::QueryDropdown)
        }];
        let values = json!({"ids": ["10", "20", "30"]});
        let rendered = render_query("IN ({{ ids }})", &defs, &values).unwrap();
        assert_eq!(rendered, "IN ('10','20','30')");
    }

    #[test]
    fn multi_select_empty_array_renders_empty() {
        let defs = vec![QueryParameterDefinition {
            enum_options: Some("a\nb".to_string()),
            multi_values_options: Some(arcodash_core::models::MultiValuesOptions::default()),
            ..param_of("choices", ParameterType::Dropdown)
        }];
        let values = json!({"choices": []});
        let rendered = render_query("IN ({{ choices }})", &defs, &values).unwrap();
        assert_eq!(rendered, "IN ()");
    }

    #[test]
    fn multi_select_non_array_value_errors() {
        let defs = vec![QueryParameterDefinition {
            enum_options: Some("a\nb".to_string()),
            multi_values_options: Some(arcodash_core::models::MultiValuesOptions::default()),
            ..param_of("choices", ParameterType::Dropdown)
        }];
        let values = json!({"choices": "not-an-array"});
        let err = render_query("{{ choices }}", &defs, &values).unwrap_err();
        assert!(
            err.to_string().contains("must be a JSON array"),
            "got: {err}"
        );
    }

    #[test]
    fn null_value_errors_with_clear_message() {
        let defs = vec![text_param("x")];
        let values = json!({"x": null});
        let err = render_query("{{ x }}", &defs, &values).unwrap_err();
        assert!(err.to_string().contains("null"), "got: {err}");
    }

    // A boolean is not a valid Number value — rejected, not stringified.
    #[test]
    fn boolean_value_for_number_param_is_rejected() {
        let defs = vec![param_of("flag", ParameterType::Number)];
        let values = json!({"flag": true});
        let err = render_query("{{ flag }}", &defs, &values).unwrap_err();
        assert!(err.to_string().contains("must be a number"), "got: {err}");
    }

    #[test]
    fn number_param_sent_as_json_string_renders() {
        let defs = vec![param_of("n", ParameterType::Number)];
        let values = json!({"n": "42"});
        let rendered = render_query("{{ n }}", &defs, &values).unwrap();
        assert_eq!(rendered, "42");
    }

    #[test]
    fn number_param_sent_as_json_string_float_renders() {
        let defs = vec![param_of("n", ParameterType::Number)];
        let values = json!({"n": "1.5"});
        let rendered = render_query("{{ n }}", &defs, &values).unwrap();
        assert_eq!(rendered, "1.5");
    }

    #[test]
    fn number_param_union_injection_payload_is_rejected() {
        let defs = vec![param_of("n", ParameterType::Number)];
        let values = json!({"n": "999 UNION SELECT username, password FROM users"});
        let err = render_query("{{ n }}", &defs, &values).unwrap_err();
        assert!(err.to_string().contains("not a valid number"), "got: {err}");
    }

    #[test]
    fn number_param_non_numeric_string_is_rejected() {
        let defs = vec![param_of("n", ParameterType::Number)];
        let values = json!({"n": "abc"});
        let err = render_query("{{ n }}", &defs, &values).unwrap_err();
        assert!(err.to_string().contains("not a valid number"), "got: {err}");
    }

    #[test]
    fn date_param_injection_payload_is_rejected() {
        let defs = vec![param_of("d", ParameterType::Date)];
        let values = json!({"d": "2026-01-01' OR '1'='1"});
        let err = render_query("WHERE created = '{{ d }}'", &defs, &values).unwrap_err();
        assert!(err.to_string().contains("not a valid"), "got: {err}");
    }

    #[test]
    fn date_param_non_date_string_is_rejected() {
        let defs = vec![param_of("d", ParameterType::Date)];
        let values = json!({"d": "not-a-date"});
        let err = render_query("WHERE created = '{{ d }}'", &defs, &values).unwrap_err();
        assert!(err.to_string().contains("not a valid"), "got: {err}");
    }

    #[test]
    fn date_range_missing_start_errors() {
        let defs = vec![param_of("r", ParameterType::DateRange)];
        let values = json!({"r": {"end": "2026-01-31"}});
        let err = render_query("{{ r.start }}", &defs, &values).unwrap_err();
        assert!(err.to_string().contains("start"), "got: {err}");
    }

    #[test]
    fn date_range_missing_end_errors() {
        let defs = vec![param_of("r", ParameterType::DateRange)];
        let values = json!({"r": {"start": "2026-01-01"}});
        let err = render_query("{{ r.end }}", &defs, &values).unwrap_err();
        assert!(err.to_string().contains("end"), "got: {err}");
    }

    #[test]
    fn date_range_non_object_value_errors() {
        let defs = vec![param_of("r", ParameterType::DateRange)];
        let values = json!({"r": "2026-01-01"});
        let err = render_query("{{ r.start }}", &defs, &values).unwrap_err();
        assert!(err.to_string().contains("must be an object"), "got: {err}");
    }

    #[test]
    fn same_range_param_substitutes_start_and_end() {
        let defs = vec![param_of("r", ParameterType::DateRange)];
        let values = json!({"r": {"start": "2026-01-01", "end": "2026-01-31"}});
        let rendered = render_query("{{ r.start }} to {{ r.end }}", &defs, &values).unwrap();
        assert_eq!(rendered, "2026-01-01 to 2026-01-31");
    }

    #[test]
    fn request_value_overrides_definition_default() {
        let defs = vec![QueryParameterDefinition {
            value: Some(json!("default")),
            ..text_param("x")
        }];
        let values = json!({"x": "override"});
        let rendered = render_query("{{ x }}", &defs, &values).unwrap();
        assert_eq!(rendered, "override");
    }

    #[test]
    fn referenced_names_extracts_base_names_including_range_tokens() {
        let names = referenced_names(
            "WHERE d BETWEEN '{{ r.start }}' AND '{{ r.end }}' AND n = '{{ name }}'",
        );
        assert_eq!(names, ["r", "name"].into_iter().map(String::from).collect());
    }

    #[test]
    fn referenced_names_counts_a_range_param_once_even_with_both_accessors() {
        let names = referenced_names("{{ r.start }} .. {{ r.end }}");
        assert_eq!(names.len(), 1);
        assert!(names.contains("r"));
    }

    #[test]
    fn manage_access_param_returns_the_live_text_param_name() {
        let defs = vec![text_param("t")];
        assert_eq!(
            manage_access_param("SELECT '{{ t }}'", &defs),
            Some("t".to_string())
        );
    }

    #[test]
    fn manage_access_param_returns_the_live_query_dropdown_param_name() {
        let defs = vec![QueryParameterDefinition {
            query_id: Some(7),
            ..param_of("choice", ParameterType::QueryDropdown)
        }];
        assert_eq!(
            manage_access_param("SELECT id = {{ choice }}", &defs),
            Some("choice".to_string())
        );
    }

    #[test]
    fn manage_access_param_ignores_an_orphaned_text_definition() {
        // The Text definition still exists, but its token was removed from the SQL, so it must no
        // longer gate the query.
        let defs = vec![text_param("t")];
        assert_eq!(manage_access_param("SELECT 1", &defs), None);
    }

    #[test]
    fn manage_access_param_ignores_a_live_non_text_param() {
        let defs = vec![param_of("n", ParameterType::Number)];
        assert_eq!(manage_access_param("SELECT {{ n }}", &defs), None);
    }

    #[test]
    fn manage_access_param_none_when_only_validated_types_are_live() {
        let defs = vec![
            param_of("n", ParameterType::Number),
            param_of("d", ParameterType::Date),
            QueryParameterDefinition {
                enum_options: Some("a\nb".to_string()),
                ..param_of("choice", ParameterType::Dropdown)
            },
        ];
        assert_eq!(
            manage_access_param("SELECT {{ n }}, {{ d }}, {{ choice }}", &defs),
            None
        );
    }
}

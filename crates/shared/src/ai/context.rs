//! Schema context for AI prompts: the schema block (optionally annotated with the data source's
//! sampled profile), relationships, and the caller's recent saved queries as few-shot examples.
//!
//! Sample values are customer data — nothing here logs them, and `include_samples = false` drops
//! every sample/min/max from the output.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use crate::connectors::TableSchema;
use crate::entities::data_source;
use crate::models::{
    prune_to_schema, AiProfileColumn, AiProfileTable, DataSourceAiProfile, DataSourceSchema,
};

use super::tokenize;

const MAX_ALL_TABLES: usize = 80;
const TOP_TABLES: usize = 25;
pub const RECENT_LIMIT: u64 = 5;
const RECENT_MAX_BYTES: usize = 2048;
const RECENT_HEADER: &str = "Recent queries by this user on this source:\n";
const MAX_RELATIONSHIPS: usize = 100;

pub struct RecentQuery {
    pub name: String,
    pub sql: String,
}

fn cached_schema(ds: &data_source::Model) -> Option<DataSourceSchema> {
    serde_json::from_value(ds.schema.clone()?).ok()
}

/// The data source's profile restricted to the currently cached schema, or `None` when absent or
/// unparsable. Schema tables without a profile entry simply format bare.
#[must_use]
pub fn usable_profile(ds: &data_source::Model) -> Option<DataSourceAiProfile> {
    pruned_profile(ds.ai_profile.as_ref()?, cached_schema(ds).as_ref())
}

fn pruned_profile(
    raw: &serde_json::Value,
    schema: Option<&DataSourceSchema>,
) -> Option<DataSourceAiProfile> {
    let mut profile: DataSourceAiProfile = serde_json::from_value(raw.clone()).ok()?;
    if let Some(schema) = schema {
        prune_to_schema(&mut profile, schema);
    }
    Some(profile)
}

/// Whether the profile has an entry for every cached-schema table and none for a column the schema
/// lacks.
#[must_use]
pub fn profile_is_complete(ds: &data_source::Model) -> bool {
    let Some(profile) = ds
        .ai_profile
        .clone()
        .and_then(|v| serde_json::from_value::<DataSourceAiProfile>(v).ok())
    else {
        return false;
    };
    profile_covers_schema(&profile, cached_schema(ds).as_ref())
}

fn profile_covers_schema(profile: &DataSourceAiProfile, schema: Option<&DataSourceSchema>) -> bool {
    let Some(schema) = schema else { return true };
    schema.tables.iter().all(|st| {
        profile
            .tables
            .iter()
            .find(|t| t.name == st.name)
            .is_some_and(|pt| {
                pt.columns
                    .iter()
                    .all(|c| st.columns.iter().any(|sc| sc.name == c.name))
            })
    })
}

fn stem_tokens(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| {
            let l = s.to_lowercase();
            l.strip_suffix('s')
                .filter(|r| !r.is_empty())
                .unwrap_or(&l)
                .to_string()
        })
        .collect()
}

fn profile_table<'a>(
    profile: Option<&'a DataSourceAiProfile>,
    name: &str,
) -> Option<&'a AiProfileTable> {
    profile?.tables.iter().find(|t| t.name == name)
}

fn profile_column<'a>(
    table: Option<&'a AiProfileTable>,
    name: &str,
) -> Option<&'a AiProfileColumn> {
    table?.columns.iter().find(|c| c.name == name)
}

fn select_tables(
    tables: &[TableSchema],
    profile: Option<&DataSourceAiProfile>,
    question: &str,
    existing_sql: Option<&str>,
    recent: &[RecentQuery],
    with_descriptions: bool,
) -> HashSet<usize> {
    if tables.len() <= MAX_ALL_TABLES {
        return (0..tables.len()).collect();
    }
    let q = stem_tokens(question);
    let mut scored: Vec<(usize, usize)> = tables
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let ptable = profile_table(profile, &t.name);
            let mut text = t.name.clone();
            for c in &t.columns {
                text.push(' ');
                text.push_str(&c.name);
                if let Some(d) = profile_column(ptable, &c.name)
                    .and_then(|p| p.description.as_ref())
                    .filter(|_| with_descriptions)
                {
                    text.push(' ');
                    text.push_str(d);
                }
            }
            (i, stem_tokens(&text).intersection(&q).count())
        })
        .filter(|(_, s)| *s > 0)
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut picked: HashSet<usize> = scored.iter().take(TOP_TABLES).map(|(i, _)| *i).collect();
    if picked.is_empty() {
        picked.extend(0..TOP_TABLES);
    }

    if let Some(p) = profile {
        let by_name: HashMap<&str, usize> = tables
            .iter()
            .enumerate()
            .map(|(i, t)| (t.name.as_str(), i))
            .collect();
        let seed = picked.clone();
        for r in &p.relationships {
            let (from, to) = (
                by_name.get(r.from_table.as_str()),
                by_name.get(r.to_table.as_str()),
            );
            if let (Some(&f), Some(&t)) = (from, to) {
                if seed.contains(&f) {
                    picked.insert(t);
                }
                if seed.contains(&t) {
                    picked.insert(f);
                }
            }
        }
    }

    let mut referenced = existing_sql.map(tokenize).unwrap_or_default();
    for r in recent {
        referenced.extend(tokenize(&r.sql));
    }
    for (i, t) in tables.iter().enumerate() {
        let lower = t.name.to_lowercase();
        let bare = lower.rsplit('.').next().unwrap_or(&lower);
        if referenced.contains(&lower) || referenced.contains(bare) {
            picked.insert(i);
        }
    }
    picked
}

/// Customer data in a one-line-per-column format: control characters (incl. newlines) become
/// spaces and whitespace runs collapse, so a value cannot start a new line or fake a section.
#[must_use]
pub fn sanitize_inline(v: &str) -> String {
    v.split(|c: char| c.is_control() || c.is_whitespace())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[must_use]
pub fn column_annotation(col: &AiProfileColumn, include_samples: bool) -> Option<String> {
    let mut parts = Vec::new();
    if include_samples {
        if !col.samples.is_empty() {
            parts.push(format!(
                "samples: {}",
                col.samples
                    .iter()
                    .map(|v| sanitize_inline(v))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let (Some(min), Some(max)) = (&col.min, &col.max) {
            parts.push(format!(
                "range: {}..{}",
                sanitize_inline(min),
                sanitize_inline(max)
            ));
        }
    }
    let null_pct = (col.null_frac * 100.0).round();
    if null_pct >= 1.0 {
        parts.push(format!("null {null_pct}%"));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

fn format_table(
    t: &TableSchema,
    ptable: Option<&AiProfileTable>,
    include_samples: bool,
    with_descriptions: bool,
) -> String {
    let ptable = ptable.filter(|p| {
        p.sampled_rows > 0
            || (with_descriptions && p.columns.iter().any(|c| c.description.is_some()))
    });
    let mut annotated = false;
    let cols: Vec<String> = t
        .columns
        .iter()
        .map(|c| {
            let mut s = c
                .type_name
                .as_ref()
                .map_or_else(|| c.name.clone(), |ty| format!("{} {}", c.name, ty));
            if let Some(p) = profile_column(ptable, &c.name) {
                if let Some(d) = p.description.as_ref().filter(|_| with_descriptions) {
                    let _ = write!(s, " — {}", sanitize_inline(d));
                    annotated = true;
                }
                if ptable.is_some_and(|t| t.sampled_rows > 0) {
                    if let Some(a) = column_annotation(p, include_samples) {
                        let _ = write!(s, " ({a})");
                        annotated = true;
                    }
                }
            }
            s
        })
        .collect();
    if annotated {
        format!("{}(\n  {}\n)", t.name, cols.join(",\n  "))
    } else {
        format!("{}({})", t.name, cols.join(", "))
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "RECENT_LIMIT is a small constant that fits usize"
)]
fn format_recent(recent: &[RecentQuery]) -> Option<String> {
    let mut out = String::new();
    for r in recent.iter().take(RECENT_LIMIT as usize) {
        let entry = format!("-- {}\n{}\n", r.name.replace('\n', " "), r.sql.trim());
        if RECENT_HEADER.len() + out.len() + entry.len() > RECENT_MAX_BYTES {
            continue;
        }
        out.push_str(&entry);
    }
    (!out.is_empty()).then(|| format!("{RECENT_HEADER}{out}"))
}

/// Full schema block for a prompt. `profile` must already be validated with [`usable_profile`].
#[must_use]
pub fn build_context(
    tables: &[TableSchema],
    profile: Option<&DataSourceAiProfile>,
    question: &str,
    existing_sql: Option<&str>,
    recent: &[RecentQuery],
    include_samples: bool,
) -> String {
    // Descriptions derived from sampled values are withheld once samples are switched off.
    let with_descriptions =
        include_samples || !profile.is_some_and(|p| p.descriptions_used_samples);
    let picked = select_tables(
        tables,
        profile,
        question,
        existing_sql,
        recent,
        with_descriptions,
    );
    let included: Vec<&TableSchema> = tables
        .iter()
        .enumerate()
        .filter(|(i, _)| picked.contains(i))
        .map(|(_, t)| t)
        .collect();

    let mut out = included
        .iter()
        .map(|t| {
            format_table(
                t,
                profile_table(profile, &t.name),
                include_samples,
                with_descriptions,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    if let Some(p) = profile {
        let names: HashSet<&str> = included.iter().map(|t| t.name.as_str()).collect();
        let rels: Vec<String> = p
            .relationships
            .iter()
            .filter(|r| {
                names.contains(r.from_table.as_str()) && names.contains(r.to_table.as_str())
            })
            .take(MAX_RELATIONSHIPS)
            .map(|r| {
                format!(
                    "{}.{} → {}.{}",
                    r.from_table, r.from_column, r.to_table, r.to_column
                )
            })
            .collect();
        if !rels.is_empty() {
            out.push_str("\n\nRelationships:\n");
            out.push_str(&rels.join("\n"));
        }
    }
    if let Some(section) = format_recent(recent) {
        out.push_str("\n\n");
        out.push_str(&section);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::ColumnSchema;
    use crate::models::{AiRelationship, RelationshipSource};

    fn table(name: &str, cols: &[&str]) -> TableSchema {
        TableSchema {
            name: name.to_string(),
            columns: cols
                .iter()
                .map(|c| ColumnSchema::typed(*c, "text"))
                .collect(),
        }
    }

    fn profile_col(name: &str, samples: &[&str]) -> AiProfileColumn {
        AiProfileColumn {
            name: name.to_string(),
            null_frac: 0.03,
            distinct_in_sample: 3,
            samples: samples.iter().map(|s| s.to_string()).collect(),
            min: None,
            max: None,
            description: None,
        }
    }

    #[test]
    fn column_annotation_flattens_control_chars_and_newlines() {
        let mut c = profile_col("c", &["a\nRelationships:\r\n  x", "b\u{0007}\tc"]);
        c.min = Some("m\nn".into());
        c.max = Some("z\n\nq".into());
        let a = column_annotation(&c, true).unwrap();
        assert!(!a.contains(['\n', '\r', '\t', '\u{0007}']), "{a}");
        assert!(a.contains("samples: a Relationships: x, b c"), "{a}");
        assert!(a.contains("range: m n..z q"), "{a}");
    }

    fn profile(tables: Vec<AiProfileTable>, rels: Vec<AiRelationship>) -> DataSourceAiProfile {
        DataSourceAiProfile {
            tables,
            relationships: rels,
            schema_refreshed_at_seen: chrono::Utc::now(),
            descriptions_used_samples: false,
        }
    }

    fn ptable(name: &str, cols: &[&str], error: Option<&str>) -> AiProfileTable {
        AiProfileTable {
            name: name.into(),
            sampled_rows: 1,
            data_profiled_at: None,
            error: error.map(String::from),
            columns: cols.iter().map(|c| profile_col(c, &[])).collect(),
        }
    }

    #[test]
    fn coverage_counts_columnless_and_errored_entries_as_complete() {
        let s = schema_of(&[("orders", &["id"]), ("v_orders", &["id"]), ("bad", &["id"])]);
        let p = profile(
            vec![
                ptable("orders", &["id"], None),
                ptable("v_orders", &[], None),
                ptable("bad", &[], Some("timeout")),
            ],
            vec![],
        );
        assert!(profile_covers_schema(&p, Some(&s)));
        assert!(profile_covers_schema(&p, None));
    }

    #[test]
    fn coverage_fails_on_missing_table_or_unknown_column() {
        let s = schema_of(&[("orders", &["id"]), ("users", &["id"])]);
        let missing = profile(vec![ptable("orders", &["id"], None)], vec![]);
        assert!(!profile_covers_schema(&missing, Some(&s)));
        let extra = profile(
            vec![
                ptable("orders", &["id", "gone"], None),
                ptable("users", &[], None),
            ],
            vec![],
        );
        assert!(!profile_covers_schema(&extra, Some(&s)));
    }

    fn big_schema() -> Vec<TableSchema> {
        (0..200)
            .map(|i| table(&format!("t{i}"), &["id", "v"]))
            .collect()
    }

    fn rel(from: &str, to: &str) -> AiRelationship {
        AiRelationship {
            from_table: from.into(),
            from_column: "x_id".into(),
            to_table: to.into(),
            to_column: "id".into(),
            source: RelationshipSource::Fk,
        }
    }

    #[test]
    fn large_schema_selects_matching_table_within_cap() {
        let mut s = big_schema();
        s[150] = table("invoices", &["id", "amount"]);
        let ctx = build_context(&s, None, "total invoice amount", None, &[], true);
        assert!(ctx.contains("invoices("));
        assert!(ctx.lines().count() <= TOP_TABLES);
    }

    #[test]
    fn small_schema_includes_every_table() {
        let s: Vec<_> = (0..80).map(|i| table(&format!("t{i}"), &["id"])).collect();
        let ctx = build_context(&s, None, "unrelated", None, &[], true);
        assert_eq!(ctx.lines().count(), 80);
    }

    #[test]
    fn hop_expansion_adds_related_table() {
        let mut s = big_schema();
        s[150] = table("invoices", &["id", "customer_ref"]);
        s[151] = table("clients", &["id"]);
        let p = profile(vec![], vec![rel("invoices", "clients")]);
        let ctx = build_context(&s, Some(&p), "invoices", None, &[], true);
        assert!(ctx.contains("clients("));
        assert!(ctx.contains("invoices.x_id → clients.id"));
        assert!(!ctx.contains("t7("));
    }

    #[test]
    fn existing_sql_table_is_kept() {
        let s = big_schema();
        let ctx = build_context(&s, None, "zzz", Some("select * from t199"), &[], true);
        assert!(ctx.contains("t199("));
    }

    #[test]
    fn recent_query_table_is_kept() {
        let s = big_schema();
        let recent = [RecentQuery {
            name: "n".into(),
            sql: "select 1 from t198".into(),
        }];
        let ctx = build_context(&s, None, "zzz", None, &recent, true);
        assert!(ctx.contains("t198("));
    }

    fn orders_with_samples() -> (Vec<TableSchema>, DataSourceAiProfile) {
        let s = vec![table("orders", &["status"])];
        let p = profile(
            vec![AiProfileTable {
                name: "orders".into(),
                sampled_rows: 10,
                data_profiled_at: None,
                error: None,
                columns: vec![profile_col("status", &["shipped", "pending"])],
            }],
            vec![],
        );
        (s, p)
    }

    #[test]
    fn samples_included_then_dropped_when_toggle_off() {
        let (s, p) = orders_with_samples();
        let on = build_context(&s, Some(&p), "", None, &[], true);
        assert!(on.contains("samples: shipped, pending"));
        assert!(on.contains("null 3%"));
        let off = build_context(&s, Some(&p), "", None, &[], false);
        assert!(!off.contains("samples:"));
        assert!(!off.contains("shipped"));
        assert!(off.contains("null 3%"));
    }

    #[test]
    fn descriptions_withheld_when_samples_off_and_derived_from_samples() {
        let (s, mut p) = orders_with_samples();
        p.tables[0].columns[0].description = Some("order lifecycle state".into());
        let on = build_context(&s, Some(&p), "", None, &[], true);
        assert!(on.contains("order lifecycle state"));
        let off = build_context(&s, Some(&p), "", None, &[], false);
        assert!(off.contains("order lifecycle state"));
        p.descriptions_used_samples = true;
        let off = build_context(&s, Some(&p), "", None, &[], false);
        assert!(!off.contains("order lifecycle state"));
        let on = build_context(&s, Some(&p), "", None, &[], true);
        assert!(on.contains("order lifecycle state"));
    }

    #[test]
    fn withheld_descriptions_do_not_steer_table_selection() {
        let mut s = big_schema();
        s[150] = table("t_x", &["id", "v"]);
        let mut p = profile(
            vec![AiProfileTable {
                name: "t_x".into(),
                sampled_rows: 10,
                data_profiled_at: None,
                error: None,
                columns: vec![profile_col("v", &["a"])],
            }],
            vec![],
        );
        p.tables[0].columns[0].description = Some("zebra stripes".into());
        p.descriptions_used_samples = true;
        let on = build_context(&s, Some(&p), "zebra", None, &[], true);
        assert!(on.contains("t_x("));
        let off = build_context(&s, Some(&p), "zebra", None, &[], false);
        assert!(!off.contains("t_x("));
    }

    fn schema_of(tables: &[(&str, &[&str])]) -> DataSourceSchema {
        serde_json::from_value(serde_json::json!({
            "tables": tables.iter().map(|(t, cols)| serde_json::json!({
                "name": t,
                "columns": cols.iter().map(|c| serde_json::json!({"name": c, "type": "text"})).collect::<Vec<_>>(),
            })).collect::<Vec<_>>()
        }))
        .unwrap()
    }

    #[test]
    fn profile_with_older_schema_timestamp_is_still_used() {
        let (_, mut p) = orders_with_samples();
        p.schema_refreshed_at_seen = chrono::Utc::now() - chrono::Duration::hours(1);
        let raw = serde_json::to_value(&p).unwrap();
        let got = pruned_profile(&raw, Some(&schema_of(&[("orders", &["status"])]))).unwrap();
        assert_eq!(got.tables[0].columns[0].samples, ["shipped", "pending"]);
    }

    #[test]
    fn dropped_table_and_column_leave_context_and_new_table_is_bare() {
        let (_, mut p) = orders_with_samples();
        p.tables[0].columns.push(profile_col("legacy", &["zzz"]));
        p.tables.push(AiProfileTable {
            name: "gone".into(),
            sampled_rows: 1,
            data_profiled_at: None,
            error: None,
            columns: vec![profile_col("x", &["qqq"])],
        });
        let raw = serde_json::to_value(&p).unwrap();
        let schema = schema_of(&[("orders", &["status"]), ("fresh", &["id"])]);
        let got = pruned_profile(&raw, Some(&schema)).unwrap();
        let s = vec![table("orders", &["status"]), table("fresh", &["id"])];
        let ctx = build_context(&s, Some(&got), "", None, &[], true);
        assert!(ctx.contains("samples: shipped, pending"), "{ctx}");
        assert!(
            !ctx.contains("zzz") && !ctx.contains("gone") && !ctx.contains("qqq"),
            "{ctx}"
        );
        assert!(ctx.contains("fresh(id"), "{ctx}");
        assert!(!ctx
            .lines()
            .find(|l| l.starts_with("fresh"))
            .unwrap()
            .contains("samples"));
    }

    #[test]
    fn recent_section_capped_at_2kb() {
        let recent: Vec<_> = (0..5)
            .map(|i| RecentQuery {
                name: format!("q{i}"),
                sql: "x".repeat(900),
            })
            .collect();
        let section = format_recent(&recent).unwrap();
        assert!(section.len() <= RECENT_MAX_BYTES);
        assert!(section.starts_with("Recent queries by this user on this source:"));
    }
}

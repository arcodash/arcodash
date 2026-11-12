//! `query_results` fetch: `GET /api/query_results/<id>` and `GET /api/queries/<id>/results`
//! (latest, no execution). No HTTP types here.
//!
//! Results are stored as gzip JSON objects via `result_store`; rows written by older versions may
//! still hold inline `data`.

use csv::WriterBuilder;
use rust_xlsxwriter::Workbook;
use sea_orm::{ConnectionTrait, DbBackend, EntityTrait, FromQueryResult, Statement};
use serde_json::Value;

use arcodash_core::entities::{group, query_result, user};

use crate::authz::authorize::{self, Action, QueryVerb};
use crate::authz::permissions::load_effective_groups;
use crate::error::ApiError;
use crate::services::query_service;

pub enum ResultFormat {
    Json,
    Csv,
    Tsv,
    Xlsx,
}

/// Convert `data` (jsonb `{columns:[{name,friendly_name}], rows:[{col:val}]}`) into the
/// requested format. Returns `(bytes, content-type)`.
#[allow(
    clippy::needless_pass_by_value,
    reason = "public signature; ResultFormat is a small fieldless enum"
)]
pub fn format_result(
    data: &Value,
    format: ResultFormat,
) -> Result<(Vec<u8>, &'static str), ApiError> {
    let columns = data["columns"].as_array().map_or(&[][..], Vec::as_slice);
    let rows = data["rows"].as_array().map_or(&[][..], Vec::as_slice);
    let col_keys: Vec<&str> = columns
        .iter()
        .map(|c| c["name"].as_str().unwrap_or(""))
        .collect();
    let col_names: Vec<&str> = columns
        .iter()
        .zip(&col_keys)
        .map(|(c, key)| {
            c["friendly_name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(key)
        })
        .collect();

    match format {
        ResultFormat::Json => {
            let bytes = serde_json::to_vec(data)
                .map_err(|e| ApiError::internal(format!("json serialize: {e}")))?;
            Ok((bytes, "application/json"))
        }

        ResultFormat::Csv | ResultFormat::Tsv => {
            let (delimiter, content_type) = match format {
                ResultFormat::Csv => (b',', "text/csv"),
                _ => (b'\t', "text/tab-separated-values"),
            };
            let mut buf = Vec::new();
            let mut wtr = WriterBuilder::new()
                .delimiter(delimiter)
                .from_writer(&mut buf);

            wtr.write_record(&col_names)
                .map_err(|e| ApiError::internal(format!("csv write: {e}")))?;

            for row in rows {
                wtr.write_record(col_keys.iter().map(|k| csv_safe_cell(&row[*k])))
                    .map_err(|e| ApiError::internal(format!("csv write: {e}")))?;
            }

            wtr.flush()
                .map_err(|e| ApiError::internal(format!("csv flush: {e}")))?;
            drop(wtr);
            Ok((buf, content_type))
        }

        #[allow(
            clippy::cast_possible_truncation,
            reason = "xlsx sheets top out at 16,384 columns and 1,048,576 rows; the writer rejects anything past that"
        )]
        ResultFormat::Xlsx => {
            let mut wb = Workbook::new();
            let ws = wb.add_worksheet();
            ws.set_name("Query Results")
                .map_err(|e| ApiError::internal(format!("xlsx: {e}")))?;

            for (col_idx, &name) in col_names.iter().enumerate() {
                ws.write_string(0, col_idx as u16, name)
                    .map_err(|e| ApiError::internal(format!("xlsx header: {e}")))?;
            }

            for (row_idx, row) in rows.iter().enumerate() {
                let excel_row = (row_idx + 1) as u32;
                for (col_idx, key) in col_keys.iter().enumerate() {
                    let col = col_idx as u16;
                    let written = match &row[*key] {
                        Value::Null => continue,
                        Value::Number(n) => {
                            ws.write_number(excel_row, col, n.as_f64().unwrap_or(0.0))
                        }
                        Value::Bool(b) => ws.write_boolean(excel_row, col, *b),
                        other => ws.write_string(excel_row, col, value_to_string(other)),
                    };
                    written.map_err(|e| ApiError::internal(format!("xlsx cell: {e}")))?;
                }
            }

            let bytes = wb
                .save_to_buffer()
                .map_err(|e| ApiError::internal(format!("xlsx save: {e}")))?;
            Ok((
                bytes,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ))
        }
    }
}

/// [`format_result`] on a blocking-pool thread: CSV/TSV/XLSX serialization of up to 10k rows is CPU
/// work reachable by authenticated requests. Takes `data` by value to move into the closure.
pub async fn format_result_blocking(
    data: Value,
    format: ResultFormat,
) -> Result<(Vec<u8>, &'static str), ApiError> {
    tokio::task::spawn_blocking(move || format_result(&data, format))
        .await
        .map_err(|e| ApiError::internal(format!("result formatting task panicked: {e}")))?
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// CSV/TSV-only: a string cell starting with `=`, `+`, `-`, `@`, tab or CR is evaluated as a
/// formula by Excel/Sheets, so prefix it with `'`. Numbers and bools are never touched; XLSX string
/// cells are already text.
fn csv_safe_cell(v: &Value) -> String {
    let s = value_to_string(v);
    if matches!(v, Value::String(_)) && s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{s}")
    } else {
        s
    }
}

/// `GET /api/query_results/<id>` — fetch a result row, gated on execute-tier data-source reach AND
/// reach on at least one query the result could belong to.
///
/// The check lives here so no caller can reintroduce the gap where any org member reads any result
/// by id; it returns the same [`ApiError::not_found`] for a missing row, another org, or a failed
/// check.
///
/// A `query_results` row has no `query_id` (it's keyed by `(data_source_id, query_hash)`, shared by
/// several queries or none for ad-hoc executes), so [`reaches_a_parent_query`] resolves the
/// candidates and allows access if none exist or the caller can view one.
pub async fn get(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    id: i64,
) -> Result<query_result::Model, ApiError> {
    let result = query_result::Entity::find_by_id(id)
        .one(db)
        .await?
        .filter(|r| r.org_id == org_id)
        .ok_or_else(|| ApiError::not_found("query result not found"))?;

    let groups = load_effective_groups(db, user).await?;
    if crate::authz::permissions::is_admin(&groups) {
        return Ok(result);
    }

    query_service::require_data_source_execute_access(db, user, &groups, result.data_source_id)
        .await
        .map_err(|_| ApiError::not_found("query result not found"))?;

    if reaches_a_parent_query(db, org_id, user, &groups, &result).await? {
        Ok(result)
    } else {
        Err(ApiError::not_found("query result not found"))
    }
}

#[derive(Debug, FromQueryResult)]
struct CandidateQueryId {
    id: i64,
}

/// `true` when `result` has no identifiable parent query (a result keyed by `(data_source_id,
/// query_hash)` may belong to several, or none for ad-hoc) or the caller can view at least one that
/// could have produced it.
async fn reaches_a_parent_query(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    groups: &[group::Model],
    result: &query_result::Model,
) -> Result<bool, ApiError> {
    let candidates = CandidateQueryId::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT id FROM queries \
          WHERE org_id = $1 \
            AND (latest_query_data_id = $2 \
              OR published_latest_query_data_id = $2 \
              OR (data_source_id = $3 AND query_hash = $4)) \
         UNION \
         SELECT entity_id FROM runs \
          WHERE org_id = $1 AND entity_type = 'query' AND query_result_id = $2 \
         LIMIT 50",
        [
            org_id.into(),
            result.id.into(),
            result.data_source_id.into(),
            result.query_hash.clone().into(),
        ],
    ))
    .all(db)
    .await?;

    if candidates.is_empty() {
        return Ok(true);
    }

    let is_admin = crate::authz::permissions::is_admin(groups);
    let ids: Vec<i64> = candidates.iter().map(|c| c.id).collect();
    let roles = authorize::prefetch_roles(db, user, groups, is_admin, "query", &ids).await?;
    for candidate in candidates {
        let can_view = authorize::evaluate_with_roles(
            db,
            user,
            groups,
            is_admin,
            &roles,
            Action::Query {
                verb: QueryVerb::View,
                id: candidate.id,
            },
        )
        .await?
        .can_edit;
        if can_view {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data(columns: serde_json::Value, rows: serde_json::Value) -> serde_json::Value {
        json!({ "columns": columns, "rows": rows })
    }

    fn simple_data() -> serde_json::Value {
        data(
            json!([
                { "name": "id",    "friendly_name": "ID"   },
                { "name": "label", "friendly_name": "Label" }
            ]),
            json!([
                { "id": 1, "label": "alpha" },
                { "id": 2, "label": "beta"  }
            ]),
        )
    }

    #[test]
    fn value_to_string_null_is_empty() {
        assert_eq!(value_to_string(&Value::Null), "");
    }

    #[test]
    fn value_to_string_string_unquoted() {
        assert_eq!(value_to_string(&json!("hello")), "hello");
    }

    #[test]
    fn value_to_string_number() {
        assert_eq!(value_to_string(&json!(42)), "42");
        // NOTE: not 3.14 — clippy's approx_constant lint flags that as "looks like PI".
        assert_eq!(value_to_string(&json!(4.25)), "4.25");
    }

    #[test]
    fn value_to_string_bool() {
        assert_eq!(value_to_string(&json!(true)), "true");
        assert_eq!(value_to_string(&json!(false)), "false");
    }

    #[test]
    fn value_to_string_object_stringified() {
        let obj = json!({ "a": 1 });
        let s = value_to_string(&obj);
        assert!(s.contains("\"a\"") && s.contains('1'), "got: {s}");
    }

    #[test]
    fn csv_safe_cell_escapes_leading_equals() {
        assert_eq!(csv_safe_cell(&json!("=SUM(A1)")), "'=SUM(A1)");
    }

    #[test]
    fn csv_safe_cell_leaves_a_numeric_leading_minus_untouched() {
        // A JSON *number* must never gain a leading quote — it would stop being numeric.
        assert_eq!(csv_safe_cell(&json!(-5)), "-5");
    }

    #[test]
    fn csv_safe_cell_escapes_a_string_leading_minus() {
        assert_eq!(csv_safe_cell(&json!("-5")), "'-5");
    }

    #[test]
    fn csv_safe_cell_escapes_plus_at_and_tab() {
        assert_eq!(csv_safe_cell(&json!("+x")), "'+x");
        assert_eq!(csv_safe_cell(&json!("@x")), "'@x");
        assert_eq!(csv_safe_cell(&json!("\tx")), "'\tx");
    }

    #[test]
    fn csv_safe_cell_leaves_ordinary_text_untouched() {
        assert_eq!(csv_safe_cell(&json!("hello")), "hello");
        assert_eq!(csv_safe_cell(&json!(true)), "true");
        assert_eq!(csv_safe_cell(&Value::Null), "");
    }

    #[test]
    fn csv_header_uses_friendly_name() {
        let (bytes, ct) = format_result(&simple_data(), ResultFormat::Csv).unwrap();
        assert_eq!(ct, "text/csv");
        let text = String::from_utf8(bytes).unwrap();
        let first_line = text.lines().next().unwrap();
        assert_eq!(first_line, "ID,Label");
    }

    #[test]
    fn csv_header_falls_back_to_name_when_friendly_name_absent() {
        let d = data(
            json!([
                { "name": "col_a" },
                { "name": "col_b", "friendly_name": "" }
            ]),
            json!([{ "col_a": "x", "col_b": "y" }]),
        );
        let (bytes, _) = format_result(&d, ResultFormat::Csv).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.lines().next().unwrap(), "col_a,col_b");
    }

    #[test]
    fn csv_rows_mapped_by_internal_key_order() {
        let (bytes, _) = format_result(&simple_data(), ResultFormat::Csv).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[1], "1,alpha");
        assert_eq!(lines[2], "2,beta");
    }

    #[test]
    fn csv_missing_key_produces_empty_cell() {
        // row is missing "label" — should produce an empty cell, not panic
        let d = data(
            json!([
                { "name": "id",    "friendly_name": "ID"    },
                { "name": "label", "friendly_name": "Label" }
            ]),
            json!([{ "id": 99 }]),
        );
        let (bytes, _) = format_result(&d, ResultFormat::Csv).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let data_line = text.lines().nth(1).unwrap();
        assert_eq!(data_line, "99,");
    }

    #[test]
    fn tsv_uses_tab_delimiter_and_correct_content_type() {
        let (bytes, ct) = format_result(&simple_data(), ResultFormat::Tsv).unwrap();
        assert_eq!(ct, "text/tab-separated-values");
        let text = String::from_utf8(bytes).unwrap();
        let first_line = text.lines().next().unwrap();
        assert!(first_line.contains('\t'), "expected tab in: {first_line}");
        assert_eq!(first_line, "ID\tLabel");
        let data_line = text.lines().nth(1).unwrap();
        assert_eq!(data_line, "1\talpha");
    }

    #[test]
    fn json_returns_data_verbatim_with_correct_content_type() {
        let d = simple_data();
        let (bytes, ct) = format_result(&d, ResultFormat::Json).unwrap();
        assert_eq!(ct, "application/json");
        let roundtrip: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(roundtrip, d);
    }

    #[test]
    fn xlsx_produces_valid_buffer_with_correct_content_type() {
        let d = data(
            json!([
                { "name": "n",   "friendly_name": "Num"  },
                { "name": "b",   "friendly_name": "Bool" },
                { "name": "nil", "friendly_name": "Nil"  },
                { "name": "s",   "friendly_name": "Str"  },
                { "name": "obj", "friendly_name": "Obj"  }
            ]),
            json!([{
                "n":   42,
                "b":   true,
                "nil": null,
                "s":   "hello",
                "obj": { "x": 1 }
            }]),
        );
        let (bytes, ct) = format_result(&d, ResultFormat::Xlsx).unwrap();
        assert_eq!(
            ct,
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        );
        assert!(bytes.len() > 4);
        assert_eq!(&bytes[0..2], b"PK");
    }

    #[tokio::test]
    async fn format_result_blocking_matches_the_sync_path_byte_for_byte() {
        // CSV rather than XLSX: the xlsx zip embeds a timestamp that could differ by a second and
        // flake an exact comparison.
        let d = data(
            json!([{ "name": "n", "friendly_name": "Num" }]),
            json!([{ "n": 42 }]),
        );
        let (sync_bytes, sync_ct) = format_result(&d, ResultFormat::Csv).unwrap();
        let (async_bytes, async_ct) = format_result_blocking(d, ResultFormat::Csv).await.unwrap();
        assert_eq!(sync_ct, async_ct);
        assert_eq!(sync_bytes, async_bytes);
    }

    #[test]
    fn csv_empty_data_no_columns_no_rows() {
        let d = data(json!([]), json!([]));
        let (bytes, ct) = format_result(&d, ResultFormat::Csv).unwrap();
        assert_eq!(ct, "text/csv");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.lines().count() <= 1);
    }

    #[test]
    fn xlsx_empty_data_produces_valid_buffer() {
        let d = data(json!([]), json!([]));
        let (bytes, ct) = format_result(&d, ResultFormat::Xlsx).unwrap();
        assert_eq!(
            ct,
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        );
        assert_eq!(&bytes[0..2], b"PK");
    }

    #[test]
    fn json_empty_data_roundtrips() {
        let d = data(json!([]), json!([]));
        let (bytes, _) = format_result(&d, ResultFormat::Json).unwrap();
        let roundtrip: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(roundtrip, d);
    }

    #[test]
    fn csv_columns_present_no_rows() {
        let d = data(json!([{ "name": "x", "friendly_name": "X" }]), json!([]));
        let (bytes, _) = format_result(&d, ResultFormat::Csv).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "X");
    }
}

//! `ProfileDataSource` handler: samples each table of a data source and stores a per-column
//! digest in `data_sources.ai_profile` for AI prompts. Sample values are customer data — only
//! table names and errors are ever logged.

use std::collections::HashMap;

use arcodash_core::ai::describe::needs_descriptions;
use arcodash_core::ai::narrative::report_unreachable;
use arcodash_core::ai::profile_values::{load_ai_profile, save_profile_values, split_profile};
use arcodash_core::ai_relationships::{merge_relationships, mine_join_pairs, name_heuristic};
use arcodash_core::config::env_parsed;
use arcodash_core::connectors::query_results::inject_caller;
use arcodash_core::connectors::sample_table_sql;
use arcodash_core::entities::{data_source, query};
use arcodash_core::models::{
    ai_profile_pending_key, is_metered, merge_sampled, prune_to_schema, tables_needing_data,
    AiProfileColumn, AiProfileTable, AiRelationship, DataSourceAiProfile, DataSourceSchema,
    ProfileData, RelationshipSource, SchemaTable, AI_PROFILE_PENDING_TTL_SECS,
};
use arcodash_core::org_storage::{OrgStorage, StorageError};
use arcodash_core::queue::{JobMessage, QueueName};
use redis::AsyncCommands;
use sea_orm::prelude::DateTimeWithTimeZone;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseBackend,
    DatabaseTransaction, EntityTrait, QueryFilter, QuerySelect, Statement, TransactionTrait,
};
use serde_json::Value;

use crate::handlers::execute_query::{decrypt_options, run_with_timeout, ExecuteQueryContext};

const MAX_TABLES: usize = 300;
const MAX_SAVED_QUERIES: u64 = 2000;
const SAMPLE_ROWS: u32 = 200;
const TOP_VALUES: usize = 5;
const MAX_VALUE_CHARS: usize = 40;
const MAX_ERROR_CHARS: usize = 300;

pub async fn handle(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    data_source_id: i64,
    data: ProfileData,
) {
    match find_owned(ctx, org_id, data_source_id).await {
        Ok(Some(ds)) => {
            if let Err(e) = run(ctx, org_id, ds, data).await {
                tracing::error!(org_id, data_source_id, error = %e, "profile_data_source failed");
                set_last_error(
                    ctx,
                    data_source_id,
                    Some(format!("profiling failed: {e:#}")),
                )
                .await;
            }
        }
        Ok(None) => {
            tracing::warn!(
                org_id,
                data_source_id,
                "profile_data_source: data source not found for org"
            );
        }
        Err(e) => {
            tracing::warn!(org_id, data_source_id, error = %e, "profile_data_source: data source lookup failed");
        }
    }
    // The producer's dedupe key; cleared when the job ends so a rebuild can be queued again.
    let mut conn = ctx.redis.clone();
    let _: Result<(), _> = conn.del(ai_profile_pending_key(data_source_id)).await;
}

/// The worker's DB role bypasses RLS: this org check is the only tenant boundary.
async fn find_owned(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    data_source_id: i64,
) -> Result<Option<data_source::Model>, sea_orm::DbErr> {
    Ok(data_source::Entity::find_by_id(data_source_id)
        .one(&ctx.db)
        .await?
        .filter(|ds| ds.org_id == org_id))
}

/// Connector errors can be arbitrarily long; bound anything stored or shown.
fn cap_error(msg: &str) -> String {
    arcodash_core::scrub::scrub_error_values(msg)
        .chars()
        .take(MAX_ERROR_CHARS)
        .collect()
}

/// The profile entry for a table whose sample failed; its error text carries no quoted values.
fn failed_table(t: &SchemaTable, e: &anyhow::Error) -> AiProfileTable {
    AiProfileTable {
        name: t.name.clone(),
        sampled_rows: 0,
        columns: t
            .columns
            .iter()
            .map(|c| column_stats(&[], &c.name, "string"))
            .collect(),
        data_profiled_at: None,
        error: Some(cap_error(&format!("{e:#}"))),
    }
}

async fn set_last_error(ctx: &ExecuteQueryContext, data_source_id: i64, err: Option<String>) {
    let res = data_source::ActiveModel {
        id: Set(data_source_id),
        ai_profile_last_error: Set(err.map(|e| cap_error(&e))),
        ..Default::default()
    }
    .update(&ctx.db)
    .await;
    if let Err(e) = res {
        tracing::warn!(data_source_id, error = %e, "recording profile error failed");
    }
}

async fn run(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    ds: data_source::Model,
    data: ProfileData,
) -> anyhow::Result<()> {
    let data_source_id = ds.id;
    // A query_results schema is per-caller and must never reach the shared cache.
    if ds.r#type == "query_results" {
        return Ok(());
    }
    // A row whose type no longer has a connector has nothing to profile.
    let Some(connector) = ctx.connectors.get(&ds.r#type) else {
        return Ok(());
    };
    // Only org_id matters here: the user_id is read solely by query_results, returned above.
    let options = inject_caller(
        decrypt_options(&ctx.encryption_key, ds.encrypted_options.as_deref())?,
        &ds.r#type,
        org_id,
        0,
    );

    let fetched = tokio::time::timeout(
        std::time::Duration::from_secs(ctx.query_timeout_secs),
        connector.get_schema(&options),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out")));
    let tables = match fetched {
        Ok(t) => t,
        Err(e) => {
            let msg = format!("schema refresh failed: {e:#}");
            set_last_error(ctx, data_source_id, Some(msg)).await;
            return Ok(());
        }
    };
    let schema = DataSourceSchema {
        tables: tables
            .into_iter()
            .map(|t| SchemaTable {
                name: t.name,
                columns: t.columns,
            })
            .collect(),
    };
    let seen = chrono::Utc::now();

    let storage = ctx.storage.resolve(&ctx.db, org_id).await?;
    let stored = match load_ai_profile(&storage, &ds).await {
        Ok(p) => p,
        Err(e) => return Err(storage_failure(ctx, org_id, e).await.into()),
    };
    let mut profile = stored.unwrap_or_else(|| DataSourceAiProfile {
        tables: vec![],
        relationships: vec![],
        schema_refreshed_at_seen: seen,
        descriptions_used_samples: false,
    });
    prune_to_schema(&mut profile, &schema);
    // Taken before bare entries are added so a table with no columns still counts as new.
    let profile_before_bare = profile.clone();
    add_bare_entries(&mut profile, &schema);
    profile.schema_refreshed_at_seen = seen;

    // Declared FKs are best-effort: failing to read them must not lose the rest of the profile.
    let fk: Vec<AiRelationship> = match connector.get_foreign_keys(&options).await {
        Ok(fks) => fks
            .into_iter()
            .map(|f| AiRelationship {
                from_table: f.from_table,
                from_column: f.from_column,
                to_table: f.to_table,
                to_column: f.to_column,
                source: RelationshipSource::Fk,
            })
            .collect(),
        Err(e) => {
            tracing::warn!(data_source_id, error = %e, "profile foreign key lookup failed");
            vec![]
        }
    };
    let saved_sql: Vec<String> = query::Entity::find()
        .select_only()
        .column(query::Column::Query)
        .filter(query::Column::OrgId.eq(org_id))
        .filter(query::Column::DataSourceId.eq(data_source_id))
        .filter(query::Column::IsArchived.eq(false))
        .limit(MAX_SAVED_QUERIES)
        .into_tuple()
        .all(&ctx.db)
        .await?;
    let mined = saved_sql.iter().flat_map(|q| mine_join_pairs(q)).collect();
    profile.relationships =
        merge_relationships(fk, mined, name_heuristic(&schema.tables), &schema.tables);

    let (stripped, location, txn) = store_values(ctx, org_id, &storage, ds.id, &profile).await?;
    data_source::ActiveModel {
        id: Set(ds.id),
        schema: Set(Some(serde_json::to_value(&schema)?)),
        schema_refreshed_at: Set(Some(DateTimeWithTimeZone::from(seen))),
        ai_profile: Set(Some(stripped)),
        ai_profile_values_location: Set(Some(location)),
        ai_profile_built_at: Set(Some(DateTimeWithTimeZone::from(seen))),
        ..Default::default()
    }
    .update(&txn)
    .await?;
    txn.commit().await?;

    let mut conn = ctx.redis.clone();
    let _: Result<(), _> = redis::cmd("SET")
        .arg(ai_profile_pending_key(data_source_id))
        .arg("data")
        .arg("XX")
        .arg("EX")
        .arg(AI_PROFILE_PENDING_TTL_SECS)
        .query_async(&mut conn)
        .await;

    if sample_table_sql(&ds.r#type, "t", 1).is_some() {
        // The view lookup can itself be billed, so it only runs when something would be sampled.
        let mut selected = select_tables(data, &ds.r#type, &profile_before_bare, &schema, &[]);
        if !selected.is_empty() {
            let views = match connector.get_view_names(&options).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(data_source_id, error = %e, "profile view lookup failed");
                    vec![]
                }
            };
            selected = select_tables(data, &ds.r#type, &profile_before_bare, &schema, &views);
        }
        let sample_options = with_sample_cost_cap(&ds.r#type, options.clone());
        let mut fresh = Vec::new();
        for t in selected {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "the pending TTL is a small constant"
            )]
            let _: Result<(), _> = conn
                .expire(
                    ai_profile_pending_key(data_source_id),
                    AI_PROFILE_PENDING_TTL_SECS as i64,
                )
                .await;
            let sql = sample_table_sql(&ds.r#type, &t.name, SAMPLE_ROWS)
                .expect("type checked sampleable above");
            match run_with_timeout(&*connector, &sample_options, &sql, ctx.query_timeout_secs).await
            {
                Ok(res) => {
                    let types: HashMap<&str, &str> = res
                        .columns
                        .iter()
                        .map(|c| (c.name.as_str(), c.friendly_type.as_str()))
                        .collect();
                    let columns = t
                        .columns
                        .iter()
                        .map(|c| {
                            let ft = types.get(c.name.as_str()).copied().unwrap_or("string");
                            column_stats(&res.rows, &c.name, ft)
                        })
                        .collect();
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "sampled rows are capped at SAMPLE_ROWS"
                    )]
                    let sampled_rows = res.rows.len() as u32;
                    fresh.push(AiProfileTable {
                        name: t.name.clone(),
                        sampled_rows,
                        columns,
                        data_profiled_at: Some(chrono::Utc::now()),
                        error: None,
                    });
                }
                Err(e) => {
                    let failed = failed_table(t, &e);
                    tracing::warn!(data_source_id, table = %t.name, error = ?failed.error, "profile sample failed");
                    fresh.push(failed);
                }
            }
        }
        merge_sampled(&mut profile, fresh);
    }

    let (stripped, location, txn) = store_values(ctx, org_id, &storage, ds.id, &profile).await?;
    data_source::ActiveModel {
        id: Set(ds.id),
        ai_profile: Set(Some(stripped)),
        ai_profile_values_location: Set(Some(location)),
        ai_profile_built_at: Set(Some(DateTimeWithTimeZone::from(chrono::Utc::now()))),
        ai_profile_last_error: Set(None),
        ..Default::default()
    }
    .update(&txn)
    .await?;
    txn.commit().await?;
    if needs_descriptions(&profile) {
        let job = JobMessage::DescribeDataSource {
            org_id,
            data_source_id,
        };
        if let Err(e) = ctx.queue.enqueue(QueueName::Schemas, job).await {
            tracing::warn!(data_source_id, error = %e, "enqueueing describe failed");
        }
    }
    Ok(())
}

/// Reports an unreachable store to the resolver; the error carries provider detail only.
async fn storage_failure(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    err: StorageError,
) -> StorageError {
    report_unreachable(&*ctx.storage, &ctx.db, org_id, &err).await;
    err
}

/// Writes the profile's sampled values to storage and returns the value-free profile JSON, the
/// values' location and the open transaction holding the data source row lock. The caller updates
/// the row and commits, so a concurrent writer of the same object waits for the pointer.
async fn store_values(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    storage: &OrgStorage,
    data_source_id: i64,
    profile: &DataSourceAiProfile,
) -> anyhow::Result<(Value, String, DatabaseTransaction)> {
    let (stripped, values) = split_profile(profile);
    let txn = ctx.db.begin().await?;
    txn.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "SELECT 1 FROM data_sources WHERE id = $1 FOR UPDATE",
        [data_source_id.into()],
    ))
    .await?;
    let location = match save_profile_values(storage, data_source_id, &values).await {
        Ok(l) => l,
        Err(e) => {
            let _ = txn.rollback().await;
            return Err(storage_failure(ctx, org_id, e).await.into());
        }
    };
    Ok((serde_json::to_value(&stripped)?, location, txn))
}

/// One column-less entry per schema table the profile lacks; uncapped so profile coverage matches
/// the schema.
fn add_bare_entries(profile: &mut DataSourceAiProfile, schema: &DataSourceSchema) {
    for t in &schema.tables {
        if !profile.tables.iter().any(|p| p.name == t.name) {
            profile.tables.push(AiProfileTable {
                name: t.name.clone(),
                sampled_rows: 0,
                columns: vec![],
                data_profiled_at: None,
                error: None,
            });
        }
    }
}

/// Tables to sample, capped at `MAX_TABLES`. Metered sources only sample on an explicit `All`.
fn select_tables<'a>(
    data: ProfileData,
    source_type: &str,
    profile: &DataSourceAiProfile,
    schema: &'a DataSourceSchema,
    views: &[String],
) -> Vec<&'a SchemaTable> {
    let needing: Vec<String> = match data {
        ProfileData::None => return vec![],
        ProfileData::Changed if is_metered(source_type) => return vec![],
        ProfileData::Changed => tables_needing_data(Some(profile), schema),
        ProfileData::All => vec![],
    };
    schema
        .tables
        .iter()
        .filter(|t| data == ProfileData::All || needing.contains(&t.name))
        .filter(|t| !views.contains(&t.name))
        .take(MAX_TABLES)
        .collect()
}

/// `BigQuery` bills per bytes scanned; a capped sample fails free instead of scanning a huge table.
fn with_sample_cost_cap(source_type: &str, mut options: Value) -> Value {
    if source_type == "bigquery" {
        if let Some(o) = options.as_object_mut() {
            let cap: i64 = env_parsed("AI_PROFILE_MAX_BYTES_BILLED", 104_857_600);
            o.insert("maximum_bytes_billed".into(), cap.into());
        }
    }
    options
}

fn value_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Digest of one column over sampled rows (JSON objects keyed by column name).
/// min/max only for numeric and date-like columns.
pub fn column_stats(rows: &[Value], name: &str, friendly_type: &str) -> AiProfileColumn {
    let vals: Vec<&Value> = rows
        .iter()
        .map(|r| r.get(name).unwrap_or(&Value::Null))
        .collect();
    let non_null: Vec<&Value> = vals.iter().copied().filter(|v| !v.is_null()).collect();
    #[allow(
        clippy::cast_precision_loss,
        reason = "sample sizes are far below f32's exact-integer range"
    )]
    let null_frac = if vals.is_empty() {
        0.0
    } else {
        (vals.len() - non_null.len()) as f32 / vals.len() as f32
    };

    let mut counts: HashMap<String, u32> = HashMap::new();
    for v in &non_null {
        *counts.entry(value_str(v)).or_default() += 1;
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "distinct values in a sample fit in u32"
    )]
    let distinct = counts.len() as u32;
    let mut freq: Vec<(String, u32)> = counts.into_iter().collect();
    // Ties break alphabetically so the digest is deterministic.
    freq.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let samples = freq
        .into_iter()
        .take(TOP_VALUES)
        .map(|(s, _)| s.chars().take(MAX_VALUE_CHARS).collect())
        .collect();

    let (min, max) = match friendly_type {
        "integer" | "float" => {
            let nums: Vec<f64> = non_null.iter().filter_map(|v| v.as_f64()).collect();
            let pick =
                |f: fn(f64, f64) -> f64| nums.iter().copied().reduce(f).map(|n| n.to_string());
            (pick(f64::min), pick(f64::max))
        }
        "date" | "datetime" => {
            let strs: Vec<String> = non_null.iter().map(|v| value_str(v)).collect();
            (strs.iter().min().cloned(), strs.iter().max().cloned())
        }
        _ => (None, None),
    };

    AiProfileColumn {
        name: name.to_string(),
        null_frac,
        distinct_in_sample: distinct,
        samples,
        min: min.map(|s| s.chars().take(MAX_VALUE_CHARS).collect()),
        max: max.map(|s| s.chars().take(MAX_VALUE_CHARS).collect()),
        description: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_backfill::tests::{fixture, seed_profile, PROFILE_JSON};
    use arcodash_core::org_storage::StorageArea;
    use arcodash_core::test_support::blob::MemoryBlobStore;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn profile_rebuild_and_backfill_serialize_on_the_row_lock() {
        let Some(f) = fixture(Arc::new(MemoryBlobStore::default()), false).await else {
            return;
        };
        seed_profile(&f, f.ds_id).await;
        let storage = f.ctx.storage.resolve(&f.ctx.db, f.org_id).await.unwrap();
        let profile: DataSourceAiProfile = serde_json::from_str(PROFILE_JSON).unwrap();
        let rel = arcodash_core::ai::profile_values::values_rel_key(f.ds_id);

        let holder = f.ctx.db.begin().await.unwrap();
        exec_sql_on(&holder, f.ds_id).await;

        let writer = async {
            let (_, location, txn) = store_values(&f.ctx, f.org_id, &storage, f.ds_id, &profile)
                .await
                .unwrap();
            txn.commit().await.unwrap();
            location
        };
        let release = async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert!(
                storage.get(StorageArea::AiProfiles, &rel).await.is_err(),
                "put happened while the row lock was held"
            );
            holder.commit().await.unwrap();
        };
        let (location, ()) = tokio::join!(writer, release);

        assert_eq!(location, rel);
        assert!(storage.get(StorageArea::AiProfiles, &rel).await.is_ok());
    }

    async fn exec_sql_on(txn: &sea_orm::DatabaseTransaction, id: i64) {
        txn.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT 1 FROM data_sources WHERE id = $1 FOR UPDATE",
            [id.into()],
        ))
        .await
        .unwrap();
    }

    #[test]
    fn nulls_ties_and_top5() {
        let rows = vec![
            json!({"c": "b"}),
            json!({"c": "a"}),
            json!({"c": null}),
            json!({"c": "b"}),
            json!({"c": "a"}),
            json!({"c": "c"}),
            json!({"c": "d"}),
            json!({"c": "e"}),
            json!({"c": "f"}),
            json!({}),
        ];
        let s = column_stats(&rows, "c", "string");
        assert_eq!(s.null_frac, 0.2);
        assert_eq!(s.distinct_in_sample, 6);
        // a and b tie at 2 (alphabetical), then c, d, e
        assert_eq!(s.samples, vec!["a", "b", "c", "d", "e"]);
        assert!(s.min.is_none() && s.max.is_none());
    }

    #[test]
    fn truncates_long_values() {
        let rows = vec![json!({"c": "x".repeat(100)})];
        assert_eq!(
            column_stats(&rows, "c", "string").samples[0]
                .chars()
                .count(),
            40
        );
    }

    #[test]
    fn min_max_only_for_numeric_and_dates() {
        let rows = vec![json!({"n": 3}), json!({"n": -1.5}), json!({"n": null})];
        let s = column_stats(&rows, "n", "float");
        assert_eq!(
            (s.min.as_deref(), s.max.as_deref()),
            (Some("-1.5"), Some("3"))
        );
        let rows = vec![json!({"d": "2024-02-01"}), json!({"d": "2023-01-05"})];
        let s = column_stats(&rows, "d", "date");
        assert_eq!(
            (s.min.as_deref(), s.max.as_deref()),
            (Some("2023-01-05"), Some("2024-02-01"))
        );
        assert!(column_stats(&rows, "d", "string").min.is_none());
    }

    #[test]
    fn empty_rows_are_safe() {
        let s = column_stats(&[], "c", "integer");
        assert_eq!((s.null_frac, s.distinct_in_sample), (0.0, 0));
    }

    fn st(name: &str) -> SchemaTable {
        SchemaTable {
            name: name.into(),
            columns: vec![],
        }
    }

    fn names(v: Vec<&SchemaTable>) -> Vec<&str> {
        v.into_iter().map(|t| t.name.as_str()).collect()
    }

    fn empty_profile() -> DataSourceAiProfile {
        DataSourceAiProfile {
            tables: vec![],
            relationships: vec![],
            schema_refreshed_at_seen: chrono::Utc::now(),
            descriptions_used_samples: false,
        }
    }

    #[test]
    fn selection_by_mode_type_and_views() {
        let schema = DataSourceSchema {
            tables: vec![st("a"), st("v"), st("b")],
        };
        let p = empty_profile();
        let views = vec!["v".to_string()];
        let sel = |d, t| names(select_tables(d, t, &p, &schema, &views));
        assert_eq!(sel(ProfileData::None, "pg"), Vec::<&str>::new());
        assert_eq!(sel(ProfileData::Changed, "pg"), ["a", "b"]);
        assert_eq!(sel(ProfileData::All, "pg"), ["a", "b"]);
        assert!(sel(ProfileData::Changed, "bigquery").is_empty());
        assert_eq!(sel(ProfileData::All, "bigquery"), ["a", "b"]);
    }

    #[test]
    fn changed_skips_up_to_date_tables_and_caps() {
        let schema = DataSourceSchema {
            tables: vec![st("old"), st("new")],
        };
        let mut p = empty_profile();
        p.tables.push(AiProfileTable {
            name: "old".into(),
            sampled_rows: 1,
            columns: vec![],
            data_profiled_at: Some(chrono::Utc::now()),
            error: None,
        });
        assert_eq!(
            names(select_tables(ProfileData::Changed, "pg", &p, &schema, &[])),
            ["new"]
        );
        let big = DataSourceSchema {
            tables: (0..MAX_TABLES + 5).map(|i| st(&format!("t{i}"))).collect(),
        };
        let all = select_tables(ProfileData::All, "pg", &p, &big, &[]);
        assert_eq!(all.len(), MAX_TABLES);
    }

    #[test]
    fn bare_entries_cover_every_schema_table() {
        let big = DataSourceSchema {
            tables: (0..MAX_TABLES + 5).map(|i| st(&format!("t{i}"))).collect(),
        };
        let mut p = empty_profile();
        add_bare_entries(&mut p, &big);
        let got: Vec<&str> = p.tables.iter().map(|t| t.name.as_str()).collect();
        let want: Vec<String> = (0..MAX_TABLES + 5).map(|i| format!("t{i}")).collect();
        assert_eq!(got, want);
        add_bare_entries(&mut p, &big);
        assert_eq!(p.tables.len(), MAX_TABLES + 5);
    }

    #[test]
    fn nothing_sampled_means_no_view_lookup() {
        let schema = DataSourceSchema {
            tables: vec![st("a")],
        };
        let p = empty_profile();
        assert!(select_tables(ProfileData::None, "pg", &p, &schema, &[]).is_empty());
        assert!(select_tables(ProfileData::Changed, "bigquery", &p, &schema, &[]).is_empty());
        assert!(!select_tables(ProfileData::Changed, "pg", &p, &schema, &[]).is_empty());
    }

    #[test]
    fn profile_table_error_is_scrubbed() {
        let err = anyhow::anyhow!("Key (email)=(bob@x.com) already exists.");
        let table = failed_table(&st("users"), &err);
        let json = serde_json::to_string(&table).unwrap();
        assert!(
            json.contains("(…)") && !json.contains("bob@x.com"),
            "{json}"
        );
    }

    #[test]
    fn cap_error_is_char_safe() {
        let long = "é".repeat(MAX_ERROR_CHARS + 50);
        let capped = cap_error(&long);
        assert_eq!(capped.chars().count(), MAX_ERROR_CHARS);
        assert_eq!(cap_error("short"), "short");
    }

    #[test]
    fn cost_cap_only_for_bigquery() {
        let o = json!({"project_id": "p"});
        let capped = with_sample_cost_cap("bigquery", o.clone());
        assert!(capped["maximum_bytes_billed"].is_i64());
        assert_eq!(capped["project_id"], "p");
        assert_eq!(with_sample_cost_cap("pg", o.clone()), o);
    }

    struct Env {
        ctx: ExecuteQueryContext,
        db: sea_orm::DatabaseConnection,
        redis: redis::aio::ConnectionManager,
        ds: data_source::Model,
        options: Value,
        /// Backend suites TRUNCATE data_sources; the advisory lock keeps the seeded row alive.
        _lock: sea_orm::DatabaseConnection,
        /// The target DB is on loopback, which the SSRF guard blocks by default.
        _net: arcodash_core::test_support::PrivateNetEnv,
    }

    /// Real target Postgres. Skips without the testing stack.
    async fn env() -> Option<Env> {
        use crate::scheduler::test_support::*;
        let (Ok(db_url), Ok(redis_url)) = (
            std::env::var("TEST_DATABASE_URL"),
            std::env::var("TEST_REDIS_URL"),
        ) else {
            eprintln!("skipping: TEST_DATABASE_URL/TEST_REDIS_URL not set");
            return None;
        };
        let net = arcodash_core::test_support::private_net_env(Some("1"));
        let lock = acquire_shared_test_db_lock(&db_url).await;
        let db = arcodash_core::db::connect(&db_url).await.unwrap();
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();
        let ctx = test_ctx(db.clone(), redis.clone());
        let org = seed_org(&db, &format!("org-profile-{}", uuid::Uuid::new_v4())).await;
        let Some(ds) = seed_target_pg_data_source(&db, org.id, &ctx.encryption_key).await else {
            eprintln!("skipping: TEST_TARGET_DATABASE_URL not set");
            return None;
        };
        let options =
            decrypt_options(&ctx.encryption_key, ds.encrypted_options.as_deref()).unwrap();
        let mut conn = redis.clone();
        let _: () = conn.del(ai_profile_pending_key(ds.id)).await.unwrap();
        Some(Env {
            ctx,
            db,
            redis,
            ds,
            options,
            _lock: lock,
            _net: net,
        })
    }

    impl Env {
        async fn sql(&self, sql: &str) {
            let pg = self.ctx.connectors.get("pg").unwrap();
            pg.run_query(&self.options, sql).await.expect(sql);
        }

        async fn run(&self) {
            self.run_mode(ProfileData::Changed).await;
        }

        async fn run_mode(&self, data: ProfileData) {
            handle(&self.ctx, self.ds.org_id, self.ds.id, data).await;
        }

        async fn load(&self) -> data_source::Model {
            data_source::Entity::find_by_id(self.ds.id)
                .one(&self.db)
                .await
                .unwrap()
                .unwrap()
        }

        async fn profile(&self) -> DataSourceAiProfile {
            let ds = self.load().await;
            let v = ds
                .ai_profile
                .unwrap_or_else(|| panic!("no profile; last_error {:?}", ds.ai_profile_last_error));
            serde_json::from_value(v).unwrap()
        }
    }

    fn uniq(prefix: &str) -> String {
        format!(
            "{prefix}_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        )
    }

    #[tokio::test]
    async fn profile_records_declared_foreign_key() {
        let Some(e) = env().await else { return };
        let (parent, child) = (uniq("fk_probe_p"), uniq("fk_probe_c"));
        e.sql(&format!("CREATE TABLE {parent} (id int PRIMARY KEY)"))
            .await;
        e.sql(&format!(
            "CREATE TABLE {child} (id int PRIMARY KEY, owner int REFERENCES {parent} (id))"
        ))
        .await;
        e.run().await;
        e.sql(&format!("DROP TABLE {child}")).await;
        e.sql(&format!("DROP TABLE {parent}")).await;

        let p = e.profile().await;
        let rel = p
            .relationships
            .iter()
            .find(|r| r.from_table == child && r.from_column == "owner")
            .unwrap_or_else(|| panic!("no fk relationship in {:?}", p.relationships));
        assert_eq!(rel.source, RelationshipSource::Fk);
        assert_eq!(
            (rel.to_table.as_str(), rel.to_column.as_str()),
            (parent.as_str(), "id")
        );
    }

    #[tokio::test]
    async fn schema_pass_adds_samples_and_prunes() {
        let Some(e) = env().await else { return };
        let t = uniq("prof_tbl");
        e.sql(&format!("CREATE TABLE {t} (id bigint)")).await;
        e.sql(&format!("INSERT INTO {t} VALUES (1), (2)")).await;

        let mut conn = e.redis.clone();
        let _: () = conn
            .set(ai_profile_pending_key(e.ds.id), "schema")
            .await
            .unwrap();
        e.run().await;
        let exists: bool = conn.exists(ai_profile_pending_key(e.ds.id)).await.unwrap();
        assert!(!exists, "pending key must be cleared");

        let ds = e.load().await;
        assert!(ds.ai_profile_built_at.is_some() && ds.ai_profile_last_error.is_none());
        let schema: DataSourceSchema = serde_json::from_value(ds.schema.clone().unwrap()).unwrap();
        assert!(schema.tables.iter().any(|x| x.name == t));
        let p = e.profile().await;
        let tbl = p.tables.iter().find(|x| x.name == t).unwrap();
        assert_eq!(tbl.sampled_rows, 2);
        assert!(tbl.data_profiled_at.is_some() && tbl.error.is_none());

        e.sql(&format!("DROP TABLE {t}")).await;
        e.run().await;
        let schema: DataSourceSchema =
            serde_json::from_value(e.load().await.schema.unwrap()).unwrap();
        assert!(!schema.tables.iter().any(|x| x.name == t));
        assert!(!e.profile().await.tables.iter().any(|x| x.name == t));
    }

    async fn seed_values_table(e: &Env) -> String {
        let t = uniq("prof_vals");
        e.sql(&format!("CREATE TABLE {t} (label text, n int)"))
            .await;
        e.sql(&format!(
            "INSERT INTO {t} VALUES ('alpha_secret_91', 5), ('beta_secret_91', 40)"
        ))
        .await;
        t
    }

    #[tokio::test]
    async fn ai_profile_values_split_out_of_postgres() {
        use arcodash_core::ai::profile_values::ProfileValues;
        use arcodash_core::org_storage::StorageArea;
        let Some(e) = env().await else { return };
        let t = seed_values_table(&e).await;
        e.run().await;
        e.sql(&format!("DROP TABLE {t}")).await;

        let ds = e.load().await;
        let raw = ds.ai_profile.clone().unwrap();
        assert!(!raw.to_string().contains("secret_91"));
        let p: DataSourceAiProfile = serde_json::from_value(raw).unwrap();
        for c in p.tables.iter().flat_map(|t| &t.columns) {
            assert!(c.samples.is_empty() && c.min.is_none() && c.max.is_none());
        }
        let loc = format!("{}.json", e.ds.id);
        assert_eq!(ds.ai_profile_values_location.as_deref(), Some(loc.as_str()));

        let storage = e.ctx.storage.resolve(&e.db, e.ds.org_id).await.unwrap();
        let bytes = storage.get(StorageArea::AiProfiles, &loc).await.unwrap();
        let values: ProfileValues = serde_json::from_slice(&bytes).unwrap();
        let cols = &values.0[&t];
        assert!(cols["label"]
            .samples
            .contains(&"alpha_secret_91".to_string()));
        assert_eq!(cols["n"].min.as_deref(), Some("5"));
        assert_eq!(cols["n"].max.as_deref(), Some("40"));
    }

    #[tokio::test]
    async fn profile_job_storage_failure_keeps_previous_profile() {
        use arcodash_core::test_support::blob::{FailingBlobStore, StaticResolver};
        let Some(mut e) = env().await else { return };
        let t = seed_values_table(&e).await;
        let good = e.ctx.storage.clone();
        e.ctx.storage = std::sync::Arc::new(StaticResolver::with_store(
            arcodash_core::org_storage::StorageMode::ArcoDash,
            std::sync::Arc::new(FailingBlobStore::default()),
            "org-root",
        ));

        // No previous profile: the failed values write leaves the row untouched.
        e.run().await;
        let ds = e.load().await;
        assert!(ds.ai_profile.is_none() && ds.ai_profile_values_location.is_none());
        let err = ds.ai_profile_last_error.unwrap();
        assert!(err.starts_with("profiling failed"), "{err}");
        assert!(!err.contains("secret_91"));

        e.ctx.storage = good;
        e.run().await;
        let before = e.load().await;
        assert!(before.ai_profile.is_some());
        e.ctx.storage = std::sync::Arc::new(StaticResolver::with_store(
            arcodash_core::org_storage::StorageMode::ArcoDash,
            std::sync::Arc::new(FailingBlobStore::default()),
            "org-root",
        ));
        e.run().await;
        e.sql(&format!("DROP TABLE {t}")).await;
        let after = e.load().await;
        assert_eq!(after.ai_profile, before.ai_profile);
        assert_eq!(
            after.ai_profile_values_location,
            before.ai_profile_values_location
        );
        assert_eq!(after.ai_profile_built_at, before.ai_profile_built_at);
    }

    #[tokio::test]
    async fn descriptions_survive_a_rebuild() {
        let Some(e) = env().await else { return };
        let t = uniq("prof_desc");
        e.sql(&format!("CREATE TABLE {t} (id bigint)")).await;
        e.run().await;
        let mut p = e.profile().await;
        p.tables.iter_mut().find(|x| x.name == t).unwrap().columns[0].description =
            Some("the key".into());
        data_source::ActiveModel {
            id: Set(e.ds.id),
            ai_profile: Set(Some(serde_json::to_value(&p).unwrap())),
            ..Default::default()
        }
        .update(&e.db)
        .await
        .unwrap();

        e.run().await;
        e.sql(&format!("DROP TABLE {t}")).await;
        let p = e.profile().await;
        let col = &p.tables.iter().find(|x| x.name == t).unwrap().columns[0];
        assert_eq!(col.description.as_deref(), Some("the key"));
    }

    #[tokio::test]
    async fn connector_error_sets_last_error_and_clears_pending() {
        let Some(e) = env().await else { return };
        let opts = json!({
            "host": "127.0.0.1", "port": 1, "user": "x", "password": "x",
            "dbname": "x", "sslmode": "disable",
        });
        let enc = arcodash_core::secrets::encrypt(
            &e.ctx.encryption_key,
            serde_json::to_vec(&opts).unwrap().as_slice(),
        )
        .unwrap();
        data_source::ActiveModel {
            id: Set(e.ds.id),
            encrypted_options: Set(Some(enc)),
            ..Default::default()
        }
        .update(&e.db)
        .await
        .unwrap();

        let mut conn = e.redis.clone();
        let _: () = conn
            .set(ai_profile_pending_key(e.ds.id), "schema")
            .await
            .unwrap();
        e.run().await;
        let err = e.load().await.ai_profile_last_error.unwrap();
        assert!(err.starts_with("schema refresh failed: "), "{err}");
        let exists: bool = conn.exists(ai_profile_pending_key(e.ds.id)).await.unwrap();
        assert!(!exists, "pending key must be cleared");
    }

    #[tokio::test]
    async fn upload_backed_source_profiles_with_injected_org() {
        use crate::scheduler::test_support::*;
        let (Ok(db_url), Ok(redis_url)) = (
            std::env::var("TEST_DATABASE_URL"),
            std::env::var("TEST_REDIS_URL"),
        ) else {
            eprintln!("skipping: TEST_DATABASE_URL/TEST_REDIS_URL not set");
            return;
        };
        let db = arcodash_core::db::connect(&db_url).await.unwrap();
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();
        let mut ctx = test_ctx(db.clone(), redis.clone());
        ctx.storage = std::sync::Arc::new(arcodash_core::org_storage::DefaultStorageResolver::new(
            ctx.store.clone(),
        ));
        ctx.connectors = std::sync::Arc::new(
            arcodash_core::connectors::registry::ConnectorRegistry::with_db(
                db.clone(),
                ctx.storage.clone(),
            ),
        );
        let org = seed_org(&db, &format!("org-profile-{}", uuid::Uuid::new_v4())).await;

        let upload_id = uuid::Uuid::new_v4();
        let object_key = format!("orgs/{}/uploads/csv/{upload_id}/data.csv", org.id);
        // A fresh `-backend memory` emulator (CI) has no buckets.
        ctx.store.ensure_bucket().await.unwrap();
        ctx.store
            .put(
                &object_key,
                bytes::Bytes::from_static(b"a,b\n1,2\n3,4\n"),
                "text/csv",
            )
            .await
            .unwrap();
        arcodash_core::entities::data_source_upload::ActiveModel {
            id: Set(upload_id),
            org_id: Set(org.id),
            object_key: Set(object_key),
            filename: Set("data.csv".into()),
            byte_size: Set(14),
            content_type: Set("text/csv".into()),
            created_by: Set(None),
            data_source_id: Set(None),
            created_at: Set(chrono::Utc::now().into()),
        }
        .insert(&db)
        .await
        .unwrap();

        let enc = arcodash_core::secrets::encrypt(
            &ctx.encryption_key,
            serde_json::to_vec(&json!({ "upload_id": upload_id.to_string() }))
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        let now = DateTimeWithTimeZone::from(chrono::Utc::now());
        let ds = data_source::ActiveModel {
            org_id: Set(org.id),
            name: Set(uniq("profile-csv")),
            r#type: Set("csv".into()),
            encrypted_options: Set(Some(enc)),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        handle(&ctx, org.id, ds.id, ProfileData::Changed).await;
        let ds = data_source::Entity::find_by_id(ds.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            ds.ai_profile_last_error.is_none(),
            "{:?}",
            ds.ai_profile_last_error
        );
        assert!(ds.ai_profile_built_at.is_some());
    }

    #[tokio::test]
    async fn org_mismatch_writes_nothing() {
        let Some(e) = env().await else { return };
        let mut conn = e.redis.clone();
        let _: () = conn
            .set(ai_profile_pending_key(e.ds.id), "schema")
            .await
            .unwrap();
        handle(
            &e.ctx,
            e.ds.org_id + 1_000_000,
            e.ds.id,
            ProfileData::Changed,
        )
        .await;
        let ds = e.load().await;
        assert!(ds.ai_profile_last_error.is_none());
        assert!(ds.ai_profile_built_at.is_none());
        let exists: bool = conn.exists(ai_profile_pending_key(e.ds.id)).await.unwrap();
        assert!(!exists, "pending key must be cleared");
    }

    fn profiled_at(p: &DataSourceAiProfile, t: &str) -> Option<chrono::DateTime<chrono::Utc>> {
        p.tables
            .iter()
            .find(|x| x.name == t)
            .unwrap()
            .data_profiled_at
    }

    #[tokio::test]
    async fn changed_samples_only_new_tables_and_all_resamples() {
        let Some(e) = env().await else { return };
        let (old, new) = (uniq("prof_old"), uniq("prof_new"));
        e.sql(&format!("CREATE TABLE {old} (id bigint)")).await;
        e.run_mode(ProfileData::All).await;
        let first = profiled_at(&e.profile().await, &old).unwrap();

        e.sql(&format!("CREATE TABLE {new} (id bigint)")).await;
        e.run().await;
        let p = e.profile().await;
        assert_eq!(profiled_at(&p, &old), Some(first));
        assert!(profiled_at(&p, &new).is_some());

        e.run_mode(ProfileData::All).await;
        let again = profiled_at(&e.profile().await, &old).unwrap();
        e.sql(&format!("DROP TABLE {old}")).await;
        e.sql(&format!("DROP TABLE {new}")).await;
        assert!(again > first);
    }

    #[tokio::test]
    async fn none_samples_nothing() {
        let Some(e) = env().await else { return };
        let t = uniq("prof_none");
        e.sql(&format!("CREATE TABLE {t} (id bigint)")).await;
        e.run_mode(ProfileData::None).await;
        e.sql(&format!("DROP TABLE {t}")).await;
        let p = e.profile().await;
        let tbl = p.tables.iter().find(|x| x.name == t).unwrap();
        assert!(tbl.data_profiled_at.is_none() && tbl.error.is_none());
    }

    #[tokio::test]
    async fn views_are_never_sampled() {
        let Some(e) = env().await else { return };
        let (t, v) = (uniq("prof_vt"), uniq("prof_view"));
        e.sql(&format!("CREATE TABLE {t} (id bigint)")).await;
        e.sql(&format!("CREATE VIEW {v} AS SELECT id FROM {t}"))
            .await;
        e.run_mode(ProfileData::All).await;
        e.sql(&format!("DROP VIEW {v}")).await;
        e.sql(&format!("DROP TABLE {t}")).await;
        let p = e.profile().await;
        assert!(profiled_at(&p, &t).is_some());
        let view = p.tables.iter().find(|x| x.name == v).unwrap();
        assert!(view.data_profiled_at.is_none() && view.error.is_none());
    }
}

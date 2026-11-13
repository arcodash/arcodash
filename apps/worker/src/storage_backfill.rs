//! Copies customer data still held inline in Postgres into object storage. Inline columns are
//! never cleared here; readers prefer inline data when present, so a row holding both keeps
//! working.

use std::collections::{BTreeMap, HashMap};

use anyhow::Context;
use chrono::{DateTime, Utc};
use redis::AsyncCommands;
use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};

use arcodash_core::observation_store::{load_observations, upsert_observation, ObservationValues};
use arcodash_core::org_storage::{OrgStorage, StorageArea};
use arcodash_core::result_store::{decode_result, save_result};

use crate::handlers::execute_query::ExecuteQueryContext;

const OBSERVATION_CURSOR_KEY: &str = "arcodash:storage_backfill:observations:cursor";
/// Set when a pass skipped rows, so the cursor restarts from 0 once the pass completes.
const OBSERVATION_RETRY_KEY: &str = "arcodash:storage_backfill:observations:retry";
const RESULT_CURSOR_KEY: &str = "arcodash:storage_backfill:results:cursor";
const RESULT_RETRY_KEY: &str = "arcodash:storage_backfill:results:retry";

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct BackfillStats {
    pub results: usize,
    pub observations: usize,
}

impl BackfillStats {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.results == 0 && self.observations == 0
    }
}

/// Per-batch cache of each org's writable storage; `None` marks an org to skip.
struct OrgCache<'a> {
    ctx: &'a ExecuteQueryContext,
    map: HashMap<i64, Option<OrgStorage>>,
}

impl<'a> OrgCache<'a> {
    fn new(ctx: &'a ExecuteQueryContext) -> Self {
        Self {
            ctx,
            map: HashMap::new(),
        }
    }

    async fn writable(&mut self, org_id: i64) -> Option<&OrgStorage> {
        if !self.map.contains_key(&org_id) {
            let resolved = match self.ctx.storage.resolve(&self.ctx.db, org_id).await {
                Ok(s) if s.writes_paused() => None,
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(org_id, error = %e, "storage backfill: org storage unavailable, skipping");
                    None
                }
            };
            self.map.insert(org_id, resolved);
        }
        self.map.get(&org_id).and_then(Option::as_ref)
    }

    fn fail(&mut self, org_id: i64) {
        self.map.insert(org_id, None);
    }
}

/// Ends a pass that found no rows: a flagged pass restarts from 0 on the next batch.
async fn finish_pass(
    conn: &mut redis::aio::ConnectionManager,
    cursor_key: &str,
    retry_key: &str,
) -> anyhow::Result<()> {
    if conn
        .del::<_, i64>(retry_key)
        .await
        .context("clear retry flag")?
        > 0
    {
        let _: () = conn.del(cursor_key).await.context("reset cursor")?;
    }
    Ok(())
}

async fn save_cursor(
    conn: &mut redis::aio::ConnectionManager,
    cursor_key: &str,
    retry_key: &str,
    cursor: i64,
    skipped: bool,
) -> anyhow::Result<()> {
    let _: () = conn.set(cursor_key, cursor).await.context("save cursor")?;
    if skipped {
        let _: () = conn.set(retry_key, 1).await.context("set retry flag")?;
    }
    Ok(())
}

/// Runs one bounded pass over every area.
///
/// # Errors
///
/// Database or Redis failure while selecting work.
pub async fn backfill_batch(
    ctx: &ExecuteQueryContext,
    limit: i64,
) -> anyhow::Result<BackfillStats> {
    let mut orgs = OrgCache::new(ctx);
    let results = backfill_results(ctx, &mut orgs, limit).await?;
    let observations = backfill_observations(ctx, &mut orgs, limit).await?;
    Ok(BackfillStats {
        results,
        observations,
    })
}

struct ResultRow {
    id: i64,
    org_id: i64,
    data_source_id: i64,
    query_hash: String,
    has_sibling: bool,
}

/// Inline results that readers still reach: current or published results of a query, results of
/// recent runs, and each monitor's newest observation.
async fn select_referenced_results(
    ctx: &ExecuteQueryContext,
    limit: i64,
    cursor: i64,
) -> anyhow::Result<Vec<ResultRow>> {
    let rows = ctx
        .db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "WITH referenced AS ( \
               SELECT latest_query_data_id AS id FROM queries \
                 WHERE latest_query_data_id > $2 \
               UNION SELECT published_latest_query_data_id FROM queries \
                 WHERE published_latest_query_data_id > $2 \
               UNION SELECT query_result_id FROM runs \
                 WHERE query_result_id > $2 AND created_at > now() - interval '7 days' \
               UNION SELECT n.query_result_id FROM ( \
                 SELECT DISTINCT ON (monitor_id) query_result_id FROM monitor_observations \
                 ORDER BY monitor_id, id DESC) n \
                 WHERE n.query_result_id > $2) \
             SELECT qr.id, qr.org_id, qr.data_source_id, qr.query_hash, \
                    EXISTS (SELECT 1 FROM query_results o \
                            WHERE o.org_id = qr.org_id AND o.data_source_id = qr.data_source_id \
                              AND o.query_hash = qr.query_hash AND o.id <> qr.id) AS has_sibling \
             FROM query_results qr JOIN referenced r ON r.id = qr.id \
             WHERE qr.id > $2 AND qr.data IS NOT NULL AND qr.data_location IS NULL \
             ORDER BY qr.id LIMIT $1",
            [limit.into(), cursor.into()],
        ))
        .await
        .context("select referenced inline results")?;
    rows.into_iter()
        .map(|r| {
            Ok(ResultRow {
                id: r.try_get("", "id")?,
                org_id: r.try_get("", "org_id")?,
                data_source_id: r.try_get("", "data_source_id")?,
                query_hash: r.try_get("", "query_hash")?,
                has_sibling: r.try_get("", "has_sibling")?,
            })
        })
        .collect::<Result<_, sea_orm::DbErr>>()
        .context("read result row")
}

async fn backfill_results(
    ctx: &ExecuteQueryContext,
    orgs: &mut OrgCache<'_>,
    limit: i64,
) -> anyhow::Result<usize> {
    let mut conn = ctx.redis.clone();
    let cursor: i64 = conn
        .get::<_, Option<i64>>(RESULT_CURSOR_KEY)
        .await
        .context("read result cursor")?
        .unwrap_or(0);
    let rows = select_referenced_results(ctx, limit, cursor).await?;
    if rows.is_empty() {
        finish_pass(&mut conn, RESULT_CURSOR_KEY, RESULT_RETRY_KEY).await?;
        return Ok(0);
    }
    let mut copied = 0usize;
    let mut skipped = false;
    let mut max_id = cursor;
    for row in rows {
        max_id = max_id.max(row.id);
        if row.has_sibling {
            tracing::warn!(
                query_result_id = row.id,
                "storage backfill: result shares its key with another row, skipping"
            );
            continue;
        }
        let Some(storage) = orgs.writable(row.org_id).await else {
            skipped = true;
            continue;
        };
        match copy_result(ctx, storage, &row).await {
            Ok(true) => copied += 1,
            Ok(false) => {}
            Err(e) => {
                skipped = true;
                orgs.fail(row.org_id);
                tracing::warn!(query_result_id = row.id, error = %e, "storage backfill: result copy failed");
            }
        }
    }
    save_cursor(
        &mut conn,
        RESULT_CURSOR_KEY,
        RESULT_RETRY_KEY,
        max_id,
        skipped,
    )
    .await?;
    Ok(copied)
}

/// Copies one result under the writers' key lock, then re-reads the object and sets the pointer
/// only when it decodes to the inline JSON. `Ok(false)` when the row changed or the object did
/// not verify.
async fn copy_result(
    ctx: &ExecuteQueryContext,
    storage: &OrgStorage,
    row: &ResultRow,
) -> anyhow::Result<bool> {
    let txn = ctx.db.begin().await?;
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_advisory_xact_lock(hashtext($1))",
        [format!("{}:{}:{}", row.org_id, row.data_source_id, row.query_hash).into()],
    ))
    .await?;
    let inline: Option<serde_json::Value> = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT data FROM query_results WHERE id = $1 AND data_location IS NULL",
            [row.id.into()],
        ))
        .await?
        .map(|r| r.try_get("", "data"))
        .transpose()?
        .flatten();
    let Some(inline) = inline else {
        return Ok(false);
    };
    let stored = save_result(storage, row.data_source_id, &row.query_hash, &inline).await?;
    let bytes = storage
        .get(StorageArea::Results, &stored.location)
        .await
        .context("re-read stored result")?;
    let size_matches = i64::try_from(bytes.len()).is_ok_and(|n| n == stored.size_bytes);
    if !size_matches || decode_result(&bytes).ok().as_ref() != Some(&inline) {
        tracing::warn!(
            query_result_id = row.id,
            "storage backfill: stored object does not match the inline data, leaving the row"
        );
        txn.rollback().await?;
        return Ok(false);
    }
    let updated = txn
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE query_results SET data_location = $2, data_size_bytes = $3 \
             WHERE id = $1 AND data_location IS NULL",
            [
                row.id.into(),
                stored.location.into(),
                stored.size_bytes.into(),
            ],
        ))
        .await?;
    if updated.rows_affected() != 1 {
        txn.rollback().await?;
        return Ok(false);
    }
    txn.commit().await?;
    Ok(true)
}

struct ObservationRow {
    id: i64,
    org_id: i64,
    monitor_id: i64,
    observed_at: DateTime<Utc>,
    value: Option<f64>,
    explanation: Option<String>,
}

async fn backfill_observations(
    ctx: &ExecuteQueryContext,
    orgs: &mut OrgCache<'_>,
    limit: i64,
) -> anyhow::Result<usize> {
    let mut conn = ctx.redis.clone();
    let cursor: i64 = conn
        .get::<_, Option<i64>>(OBSERVATION_CURSOR_KEY)
        .await
        .context("read observation cursor")?
        .unwrap_or(0);
    let rows = ctx
        .db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id, org_id, monitor_id, observed_at, value, explanation \
             FROM monitor_observations \
             WHERE id > $1 AND (value IS NOT NULL OR explanation IS NOT NULL) \
             ORDER BY id LIMIT $2",
            [cursor.into(), limit.into()],
        ))
        .await
        .context("select inline observations")?;
    if rows.is_empty() {
        finish_pass(&mut conn, OBSERVATION_CURSOR_KEY, OBSERVATION_RETRY_KEY).await?;
        return Ok(0);
    }
    let mut by_monitor: BTreeMap<(i64, i64), Vec<ObservationRow>> = BTreeMap::new();
    let mut max_id = cursor;
    for r in rows {
        let row = ObservationRow {
            id: r.try_get("", "id")?,
            org_id: r.try_get("", "org_id")?,
            monitor_id: r.try_get("", "monitor_id")?,
            observed_at: r.try_get("", "observed_at")?,
            value: r.try_get("", "value")?,
            explanation: r.try_get("", "explanation")?,
        };
        max_id = max_id.max(row.id);
        by_monitor
            .entry((row.org_id, row.monitor_id))
            .or_default()
            .push(row);
    }

    let mut written = 0usize;
    let mut skipped = false;
    for ((org_id, monitor_id), group) in by_monitor {
        let Some(storage) = orgs.writable(org_id).await else {
            skipped = true;
            continue;
        };
        match copy_observations(ctx, storage, monitor_id, &group).await {
            Ok(n) => written += n,
            Err(e) => {
                skipped = true;
                tracing::warn!(monitor_id, error = %e, "storage backfill: observation copy failed");
            }
        }
    }
    save_cursor(
        &mut conn,
        OBSERVATION_CURSOR_KEY,
        OBSERVATION_RETRY_KEY,
        max_id,
        skipped,
    )
    .await?;
    Ok(written)
}

fn line_matches(stored: Option<&ObservationValues>, row: &ObservationRow) -> bool {
    stored.is_some_and(|s| {
        (row.value.is_none() || s.value == row.value)
            && (row.explanation.is_none() || s.explanation == row.explanation)
    })
}

/// Writes the missing lines of one monitor (oldest first), then re-loads to verify each.
async fn copy_observations(
    ctx: &ExecuteQueryContext,
    storage: &OrgStorage,
    monitor_id: i64,
    group: &[ObservationRow],
) -> anyhow::Result<usize> {
    let mut sorted: Vec<&ObservationRow> = group.iter().collect();
    sorted.sort_by_key(|r| (r.observed_at, r.id));
    let (Some(first), Some(last)) = (sorted.first(), sorted.last()) else {
        return Ok(0);
    };
    let (from, to) = (first.observed_at, last.observed_at);
    let existing = load_observations(storage, monitor_id, from, to).await?;
    let mut written = 0usize;
    for row in &sorted {
        let stored = existing.get(&row.id);
        if line_matches(stored, row) {
            continue;
        }
        let values = ObservationValues {
            id: row.id,
            observed_at: row.observed_at,
            value: row.value,
            explanation: row.explanation.clone(),
        };
        upsert_observation(&ctx.db, storage, monitor_id, &values).await?;
        written += 1;
    }
    if written > 0 {
        let after = load_observations(storage, monitor_id, from, to).await?;
        for row in &sorted {
            anyhow::ensure!(
                line_matches(after.get(&row.id), row),
                "observation {} did not verify after writing",
                row.id
            );
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arcodash_core::observation_store::set_explanation;
    use async_trait::async_trait;
    use bytes::Bytes;
    use sea_orm::DatabaseConnection;
    use serde_json::json;

    use arcodash_core::org_storage::{BlobStore, OrgStorageResolver, StorageError, StorageMode};
    use arcodash_core::test_support::blob::MemoryBlobStore;

    use super::*;
    use crate::scheduler::test_support::*;

    /// Resolves only `org_id`; every other org is unreachable so a whole-DB pass skips it.
    struct ScopedResolver {
        org_id: i64,
        store: Arc<dyn BlobStore>,
        paused: bool,
    }

    #[async_trait]
    impl OrgStorageResolver for ScopedResolver {
        async fn resolve(
            &self,
            _db: &DatabaseConnection,
            org_id: i64,
        ) -> Result<OrgStorage, StorageError> {
            if org_id != self.org_id {
                return Err(StorageError::Unreachable {
                    detail: "out of scope".into(),
                });
            }
            let s = OrgStorage::new(org_id, StorageMode::ArcoDash, self.store.clone(), "orgs/t/");
            Ok(if self.paused {
                s.with_writes_paused()
            } else {
                s
            })
        }
    }

    /// Serves different bytes than were written.
    struct TamperedStore(MemoryBlobStore);

    #[async_trait]
    impl BlobStore for TamperedStore {
        async fn put(&self, key: &str, bytes: Bytes, ct: &str) -> Result<(), StorageError> {
            self.0.put(key, bytes, ct).await
        }
        async fn get(&self, key: &str) -> Result<Bytes, StorageError> {
            self.0.get(key).await?;
            Ok(Bytes::from_static(b"not the stored object"))
        }
        async fn delete(&self, key: &str) -> Result<(), StorageError> {
            self.0.delete(key).await
        }
        async fn list_prefix(&self, p: &str) -> Result<Vec<(String, u64)>, StorageError> {
            self.0.list_prefix(p).await
        }
    }

    struct Fixture {
        ctx: ExecuteQueryContext,
        org_id: i64,
        ds_id: i64,
        query_id: i64,
        user_id: i64,
    }

    async fn fixture(store: Arc<dyn BlobStore>, paused: bool) -> Option<Fixture> {
        let (db, redis) = setup().await?;
        let mut conn = redis.clone();
        let _: () = conn
            .del(&[
                OBSERVATION_CURSOR_KEY,
                OBSERVATION_RETRY_KEY,
                RESULT_CURSOR_KEY,
                RESULT_RETRY_KEY,
            ])
            .await
            .unwrap();
        let org = seed_org(&db, &format!("backfill-{}", uuid::Uuid::new_v4())).await;
        let ds = seed_data_source(&db, org.id).await;
        let q = seed_query(&db, org.id, ds.id).await;
        let ctx = ExecuteQueryContext {
            storage: Arc::new(ScopedResolver {
                org_id: org.id,
                store,
                paused,
            }),
            ..test_ctx(db, redis)
        };
        Some(Fixture {
            ctx,
            org_id: org.id,
            ds_id: ds.id,
            query_id: q.id,
            user_id: q.user_id,
        })
    }

    async fn pointer(db: &DatabaseConnection, id: i64) -> (Option<String>, serde_json::Value) {
        let r = query_row(
            db,
            "SELECT data_location, data FROM query_results WHERE id = $1",
            [id.into()],
        )
        .await;
        (
            r.try_get("", "data_location").unwrap(),
            r.try_get("", "data").unwrap(),
        )
    }

    async fn seed_observation(
        f: &Fixture,
        monitor_id: i64,
        value: Option<f64>,
        explanation: Option<&str>,
    ) -> i64 {
        let r = query_row(
            &f.ctx.db,
            "INSERT INTO monitor_observations \
             (org_id, monitor_id, observed_at, value, state, explanation) \
             VALUES ($1, $2, now(), $3, 'ok', $4) RETURNING id",
            [
                f.org_id.into(),
                monitor_id.into(),
                value.into(),
                explanation.map(str::to_string).into(),
            ],
        )
        .await;
        r.try_get("", "id").unwrap()
    }

    async fn seed_referenced_and_unreferenced(f: &Fixture) -> (i64, i64) {
        let now = Utc::now();
        let referenced =
            seed_query_result(&f.ctx.db, f.org_id, f.ds_id, now, json!({"a": 1})).await;
        set_latest_query_data(&f.ctx.db, f.query_id, referenced.id).await;
        let orphan = seed_query_result(&f.ctx.db, f.org_id, f.ds_id, now, json!({"b": 2})).await;
        (referenced.id, orphan.id)
    }

    #[tokio::test]
    async fn backfill_copies_referenced_results_only() {
        let store = Arc::new(MemoryBlobStore::default());
        let Some(f) = fixture(store.clone(), false).await else {
            return;
        };
        let (referenced, orphan) = seed_referenced_and_unreferenced(&f).await;

        let stats = backfill_batch(&f.ctx, 10_000).await.unwrap();
        assert_eq!(stats.results, 1);

        let (loc, data) = pointer(&f.ctx.db, referenced).await;
        let loc = loc.expect("referenced result gets a pointer");
        assert_eq!(data, json!({"rows": [{"a": 1}]}));
        let storage = f.ctx.storage.resolve(&f.ctx.db, f.org_id).await.unwrap();
        let bytes = storage.get(StorageArea::Results, &loc).await.unwrap();
        assert_eq!(decode_result(&bytes).unwrap(), data);

        let (loc, data) = pointer(&f.ctx.db, orphan).await;
        assert!(loc.is_none(), "unreferenced result stays pointer-less");
        assert_eq!(data, json!({"rows": [{"b": 2}]}));
    }

    #[tokio::test]
    async fn backfill_is_idempotent() {
        let store = Arc::new(MemoryBlobStore::default());
        let Some(f) = fixture(store.clone(), false).await else {
            return;
        };
        let (referenced, _) = seed_referenced_and_unreferenced(&f).await;
        let monitor = seed_monitor(&f.ctx.db, f.org_id, f.query_id, f.user_id, json!({})).await;
        let obs = seed_observation(&f, monitor.id, Some(1.5), Some("why")).await;

        backfill_batch(&f.ctx, 10_000).await.unwrap();
        let puts = store.puts();
        let keys = store.keys();
        assert!(pointer(&f.ctx.db, referenced).await.0.is_some());

        let second = backfill_batch(&f.ctx, 10_000).await.unwrap();
        assert_eq!(second, BackfillStats::default());
        assert_eq!(store.puts(), puts, "no new objects on the second batch");
        assert_eq!(store.keys(), keys);

        let storage = f.ctx.storage.resolve(&f.ctx.db, f.org_id).await.unwrap();
        let lines = load_observations(
            &storage,
            monitor.id,
            Utc::now() - chrono::Duration::days(1),
            Utc::now() + chrono::Duration::days(1),
        )
        .await
        .unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[&obs].value, Some(1.5));
    }

    #[tokio::test]
    async fn backfill_never_nulls_inline_columns() {
        let store = Arc::new(MemoryBlobStore::default());
        let Some(f) = fixture(store, false).await else {
            return;
        };
        let (referenced, _) = seed_referenced_and_unreferenced(&f).await;
        let monitor = seed_monitor(&f.ctx.db, f.org_id, f.query_id, f.user_id, json!({})).await;
        let obs = seed_observation(&f, monitor.id, Some(2.25), Some("because")).await;

        backfill_batch(&f.ctx, 10_000).await.unwrap();

        let (loc, data) = pointer(&f.ctx.db, referenced).await;
        assert!(loc.is_some());
        assert_eq!(data, json!({"rows": [{"a": 1}]}));
        let r = query_row(
            &f.ctx.db,
            "SELECT value, explanation FROM monitor_observations WHERE id = $1",
            [obs.into()],
        )
        .await;
        assert_eq!(r.try_get::<Option<f64>>("", "value").unwrap(), Some(2.25));
        assert_eq!(
            r.try_get::<Option<String>>("", "explanation")
                .unwrap()
                .as_deref(),
            Some("because")
        );
    }

    #[tokio::test]
    async fn backfill_skips_mismatched_object() {
        let store = Arc::new(TamperedStore(MemoryBlobStore::default()));
        let Some(f) = fixture(store, false).await else {
            return;
        };
        let (referenced, _) = seed_referenced_and_unreferenced(&f).await;

        backfill_batch(&f.ctx, 10_000).await.unwrap();

        let (loc, data) = pointer(&f.ctx.db, referenced).await;
        assert!(
            loc.is_none(),
            "a pointer must not be set for an unverified object"
        );
        assert_eq!(data, json!({"rows": [{"a": 1}]}));
    }

    #[tokio::test]
    async fn backfill_copies_observation_values_and_explanations() {
        let store = Arc::new(MemoryBlobStore::default());
        let Some(f) = fixture(store, false).await else {
            return;
        };
        let monitor = seed_monitor(&f.ctx.db, f.org_id, f.query_id, f.user_id, json!({})).await;
        let a = seed_observation(&f, monitor.id, Some(10.0), None).await;
        let b = seed_observation(&f, monitor.id, Some(11.5), Some("spike")).await;
        let c = seed_observation(&f, monitor.id, None, Some("note only")).await;

        let stats = backfill_batch(&f.ctx, 10_000).await.unwrap();
        assert_eq!(stats.observations, 3);

        let storage = f.ctx.storage.resolve(&f.ctx.db, f.org_id).await.unwrap();
        let lines = load_observations(
            &storage,
            monitor.id,
            Utc::now() - chrono::Duration::days(1),
            Utc::now() + chrono::Duration::days(1),
        )
        .await
        .unwrap();
        assert_eq!(
            (lines[&a].value, lines[&a].explanation.clone()),
            (Some(10.0), None)
        );
        assert_eq!(
            (lines[&b].value, lines[&b].explanation.as_deref()),
            (Some(11.5), Some("spike"))
        );
        assert_eq!(
            (lines[&c].value, lines[&c].explanation.as_deref()),
            (None, Some("note only"))
        );
    }

    #[tokio::test]
    async fn backfill_skips_paused_orgs() {
        let store = Arc::new(MemoryBlobStore::default());
        let Some(f) = fixture(store.clone(), true).await else {
            return;
        };
        let (referenced, _) = seed_referenced_and_unreferenced(&f).await;
        let monitor = seed_monitor(&f.ctx.db, f.org_id, f.query_id, f.user_id, json!({})).await;
        seed_observation(&f, monitor.id, Some(3.0), None).await;

        let stats = backfill_batch(&f.ctx, 10_000).await.unwrap();

        assert_eq!(stats, BackfillStats::default());
        assert_eq!(store.puts(), 0);
        assert!(pointer(&f.ctx.db, referenced).await.0.is_none());
    }

    async fn raw_month(store: &MemoryBlobStore, monitor_id: i64) -> Vec<serde_json::Value> {
        let key = store
            .keys()
            .into_iter()
            .find(|k| k.contains(&format!("/{monitor_id}/observations/")))
            .expect("month file");
        let bytes = store.get(&key).await.unwrap();
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn backfill_does_not_duplicate_observation_lines() {
        let store = Arc::new(MemoryBlobStore::default());
        let Some(f) = fixture(store.clone(), false).await else {
            return;
        };
        let monitor = seed_monitor(&f.ctx.db, f.org_id, f.query_id, f.user_id, json!({})).await;
        let obs = seed_observation(&f, monitor.id, Some(4.0), None).await;
        let storage = f.ctx.storage.resolve(&f.ctx.db, f.org_id).await.unwrap();
        let at: DateTime<Utc> = query_row(
            &f.ctx.db,
            "SELECT observed_at FROM monitor_observations WHERE id = $1",
            [obs.into()],
        )
        .await
        .try_get("", "observed_at")
        .unwrap();
        set_explanation(&f.ctx.db, &storage, monitor.id, obs, at, "stored why")
            .await
            .unwrap();

        backfill_batch(&f.ctx, 10_000).await.unwrap();

        let lines = raw_month(&store, monitor.id).await;
        assert_eq!(lines.iter().filter(|l| l["id"] == obs).count(), 1);
        assert_eq!(lines.len(), 1);
        let day = chrono::Duration::days(1);
        let loaded = load_observations(&storage, monitor.id, at - day, at + day)
            .await
            .unwrap();
        assert_eq!(loaded[&obs].value, Some(4.0));
        assert_eq!(loaded[&obs].explanation.as_deref(), Some("stored why"));

        set_explanation(&f.ctx.db, &storage, monitor.id, obs, at, "newer")
            .await
            .unwrap();
        let loaded = load_observations(&storage, monitor.id, at - day, at + day)
            .await
            .unwrap();
        assert_eq!(loaded[&obs].explanation.as_deref(), Some("newer"));
        assert_eq!(raw_month(&store, monitor.id).await.len(), 1);
    }

    #[tokio::test]
    async fn backfill_waits_for_the_result_write_lock() {
        let store = Arc::new(MemoryBlobStore::default());
        let Some(f) = fixture(store.clone(), false).await else {
            return;
        };
        let (referenced, _) = seed_referenced_and_unreferenced(&f).await;
        let hash: String = query_row(
            &f.ctx.db,
            "SELECT query_hash FROM query_results WHERE id = $1",
            [referenced.into()],
        )
        .await
        .try_get("", "query_hash")
        .unwrap();

        let holder = f.ctx.db.begin().await.unwrap();
        holder
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT pg_advisory_xact_lock(hashtext($1))",
                [format!("{}:{}:{}", f.org_id, f.ds_id, hash).into()],
            ))
            .await
            .unwrap();
        let blocked = tokio::time::timeout(
            std::time::Duration::from_millis(1500),
            backfill_batch(&f.ctx, 10_000),
        )
        .await;
        assert!(blocked.is_err(), "backfill must wait for the lock");
        assert_eq!(store.puts(), 0);
        assert!(pointer(&f.ctx.db, referenced).await.0.is_none());

        holder.rollback().await.unwrap();
        let stats = backfill_batch(&f.ctx, 10_000).await.unwrap();
        assert_eq!(stats.results, 1);
        assert!(pointer(&f.ctx.db, referenced).await.0.is_some());
    }

    #[tokio::test]
    async fn backfill_reference_rules() {
        let store = Arc::new(MemoryBlobStore::default());
        let Some(f) = fixture(store, false).await else {
            return;
        };
        let db = &f.ctx.db;
        let now = Utc::now();
        let mk = || seed_query_result(db, f.org_id, f.ds_id, now, json!({"x": 1}));
        let (published, recent_run, old_run, latest_obs, old_obs, unreferenced) = (
            mk().await.id,
            mk().await.id,
            mk().await.id,
            mk().await.id,
            mk().await.id,
            mk().await.id,
        );
        set_published_latest_query_data(db, f.query_id, published).await;

        for (qr, age) in [(recent_run, "0 days"), (old_run, "8 days")] {
            let run = seed_run(
                db,
                f.org_id,
                "query",
                f.query_id,
                None,
                Some(f.ds_id),
                "success",
                None,
            )
            .await;
            exec_sql(
                db,
                "UPDATE runs SET query_result_id = $1, \
                 created_at = now() - ($3 || '')::interval WHERE id = $2",
                [qr.into(), run.into(), age.into()],
            )
            .await;
        }

        let monitor = seed_monitor(db, f.org_id, f.query_id, f.user_id, json!({})).await;
        for qr in [old_obs, latest_obs] {
            exec_sql(
                db,
                "INSERT INTO monitor_observations \
                 (org_id, monitor_id, observed_at, state, query_result_id) \
                 VALUES ($1, $2, now(), 'ok', $3)",
                [f.org_id.into(), monitor.id.into(), qr.into()],
            )
            .await;
        }

        let stats = backfill_batch(&f.ctx, 10_000).await.unwrap();
        assert_eq!(stats.results, 3);

        for (id, copied) in [
            (published, true),
            (recent_run, true),
            (old_run, false),
            (latest_obs, true),
            (old_obs, false),
            (unreferenced, false),
        ] {
            assert_eq!(pointer(db, id).await.0.is_some(), copied, "result {id}");
        }
    }

    #[tokio::test]
    async fn backfill_skips_results_with_same_key_siblings() {
        let store = Arc::new(MemoryBlobStore::default());
        let Some(f) = fixture(store.clone(), false).await else {
            return;
        };
        let db = &f.ctx.db;
        let now = Utc::now();
        let a = seed_query_result(db, f.org_id, f.ds_id, now, json!({"a": 1})).await;
        let b = seed_query_result(db, f.org_id, f.ds_id, now, json!({"b": 1})).await;
        let c = seed_query_result(db, f.org_id, f.ds_id, now, json!({"c": 1})).await;
        exec_sql(
            db,
            "UPDATE query_results SET query_hash = (SELECT query_hash FROM query_results \
             WHERE id = $1) WHERE id = $2",
            [a.id.into(), b.id.into()],
        )
        .await;
        set_latest_query_data(db, f.query_id, a.id).await;
        set_published_latest_query_data(db, f.query_id, b.id).await;
        let other = seed_query(db, f.org_id, f.ds_id).await;
        set_latest_query_data(db, other.id, c.id).await;

        let stats = backfill_batch(&f.ctx, 10_000).await.unwrap();

        assert_eq!(stats.results, 1);
        assert!(pointer(db, a.id).await.0.is_none());
        assert!(pointer(db, b.id).await.0.is_none());
        assert!(pointer(db, c.id).await.0.is_some());
    }
}

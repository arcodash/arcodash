//! Monitor observation values and explanations live in the organization's `Monitors` area as
//! JSON Lines, one file per monitor and month. The database row keeps only the metadata.

use std::collections::HashMap;

use anyhow::Context;
use bytes::Bytes;
use chrono::{DateTime, Datelike, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};

use crate::org_storage::{OrgStorage, StorageArea, StorageError};

const CONTENT_TYPE: &str = "application/x-ndjson";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ObservationValues {
    pub id: i64,
    pub observed_at: DateTime<Utc>,
    pub value: Option<f64>,
    pub explanation: Option<String>,
}

#[must_use]
pub fn observation_rel_key(monitor_id: i64, at: DateTime<Utc>) -> String {
    format!(
        "{monitor_id}/observations/{:04}-{:02}.jsonl",
        at.year(),
        at.month()
    )
}

fn parse_lines(bytes: &[u8]) -> Vec<ObservationValues> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            serde_json::from_str::<ObservationValues>(l)
                .map_err(|_| tracing::warn!("observation_store: skipping malformed line"))
                .ok()
        })
        .collect()
}

fn encode_lines(items: &[ObservationValues]) -> Result<Bytes, StorageError> {
    let mut out = Vec::new();
    for item in items {
        serde_json::to_writer(&mut out, item).context("serialize observation")?;
        out.push(b'\n');
    }
    Ok(Bytes::from(out))
}

async fn read_month(
    storage: &OrgStorage,
    rel: &str,
) -> Result<Vec<ObservationValues>, StorageError> {
    match storage.get(StorageArea::Monitors, rel).await {
        Ok(bytes) => Ok(parse_lines(&bytes)),
        Err(StorageError::NotFound) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn db_err(e: sea_orm::DbErr) -> StorageError {
    StorageError::Other(anyhow::Error::new(e))
}

/// Read-modify-write of one month file under a per-monitor advisory lock.
async fn modify_month(
    db: &DatabaseConnection,
    storage: &OrgStorage,
    monitor_id: i64,
    at: DateTime<Utc>,
    f: impl FnOnce(&mut Vec<ObservationValues>),
) -> Result<(), StorageError> {
    let rel = observation_rel_key(monitor_id, at);
    let txn = db.begin().await.map_err(db_err)?;
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_advisory_xact_lock(hashtext($1))",
        [format!("obs:{monitor_id}").into()],
    ))
    .await
    .map_err(db_err)?;
    let mut items = read_month(storage, &rel).await?;
    f(&mut items);
    storage
        .put(
            StorageArea::Monitors,
            &rel,
            encode_lines(&items)?,
            CONTENT_TYPE,
        )
        .await?;
    txn.commit().await.map_err(db_err)
}

/// Appends one line; with `keep_from`, lines observed before it are dropped from the month file.
///
/// # Errors
///
/// Database or storage failure.
pub async fn append_observation(
    db: &DatabaseConnection,
    storage: &OrgStorage,
    monitor_id: i64,
    v: &ObservationValues,
    keep_from: Option<DateTime<Utc>>,
) -> Result<(), StorageError> {
    modify_month(db, storage, monitor_id, v.observed_at, |items| {
        items.push(v.clone());
        if let Some(k) = keep_from {
            items.retain(|i| i.observed_at >= k);
        }
    })
    .await
}

/// Every month file overlapping `[from, to]`; a missing file is a gap, not an error.
///
/// # Errors
///
/// Storage failure other than a missing object.
pub async fn load_observations(
    storage: &OrgStorage,
    monitor_id: i64,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<HashMap<i64, ObservationValues>, StorageError> {
    let mut out = HashMap::new();
    let (mut year, mut month) = (from.year(), from.month());
    while (year, month) <= (to.year(), to.month()) {
        let rel = format!("{monitor_id}/observations/{year:04}-{month:02}.jsonl");
        for item in read_month(storage, &rel).await? {
            out.insert(item.id, item);
        }
        if month == 12 {
            year += 1;
            month = 1;
        } else {
            month += 1;
        }
    }
    Ok(out)
}

/// Sets the explanation of one observation, adding a value-less line when it is absent.
///
/// # Errors
///
/// Database or storage failure.
pub async fn set_explanation(
    db: &DatabaseConnection,
    storage: &OrgStorage,
    monitor_id: i64,
    observation_id: i64,
    observed_at: DateTime<Utc>,
    explanation: &str,
) -> Result<(), StorageError> {
    modify_month(db, storage, monitor_id, observed_at, |items| {
        if let Some(item) = items.iter_mut().find(|i| i.id == observation_id) {
            item.explanation = Some(explanation.to_string());
        } else {
            items.push(ObservationValues {
                id: observation_id,
                observed_at,
                value: None,
                explanation: Some(explanation.to_string()),
            });
        }
    })
    .await
}

/// Deletes month files strictly older than the month of `oldest_kept`.
///
/// # Errors
///
/// Storage failure.
pub async fn prune_months_before(
    storage: &OrgStorage,
    monitor_id: i64,
    oldest_kept: DateTime<Utc>,
) -> Result<usize, StorageError> {
    let prefix = format!("{monitor_id}/observations/");
    let cutoff = format!("{:04}-{:02}", oldest_kept.year(), oldest_kept.month());
    let mut deleted = 0;
    for (rel, _) in storage.list(StorageArea::Monitors, &prefix).await? {
        let Some(month) = rel
            .strip_prefix(&prefix)
            .and_then(|n| n.strip_suffix(".jsonl"))
        else {
            continue;
        };
        if month < cutoff.as_str() {
            storage.delete(StorageArea::Monitors, &rel).await?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// The row's own value wins (rows written before values moved to storage); else the stored one.
#[must_use]
pub fn merged_value(row_value: Option<f64>, stored: Option<&ObservationValues>) -> Option<f64> {
    row_value.or_else(|| stored.and_then(|v| v.value))
}

/// The row's own explanation wins; else the stored one.
#[must_use]
pub fn merged_explanation(
    row_explanation: Option<String>,
    stored: Option<&ObservationValues>,
) -> Option<String> {
    row_explanation.or_else(|| stored.and_then(|v| v.explanation.clone()))
}

/// Best-effort lookup of stored values for metadata rows; an unreadable store yields an empty map.
pub async fn load_for_rows(
    storage: &OrgStorage,
    monitor_id: i64,
    rows: &[(i64, DateTime<Utc>)],
) -> HashMap<i64, ObservationValues> {
    let (Some(from), Some(to)) = (
        rows.iter().map(|r| r.1).min(),
        rows.iter().map(|r| r.1).max(),
    ) else {
        return HashMap::new();
    };
    load_observations(storage, monitor_id, from, to)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(monitor_id, error = %e, "observation_store: load failed");
            HashMap::new()
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::TimeZone;

    use super::*;
    use crate::org_storage::StorageMode;
    use crate::test_support::blob::MemoryBlobStore;

    fn storage() -> (OrgStorage, Arc<MemoryBlobStore>) {
        let mem = Arc::new(MemoryBlobStore::default());
        (
            OrgStorage::new(1, StorageMode::ArcoDash, mem.clone(), "orgs/1/"),
            mem,
        )
    }

    fn obs(id: i64, at: DateTime<Utc>, value: f64) -> ObservationValues {
        ObservationValues {
            id,
            observed_at: at,
            value: Some(value),
            explanation: None,
        }
    }

    fn ts(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
    }

    async fn db() -> Option<DatabaseConnection> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        Some(crate::db::connect(&url).await.unwrap())
    }

    async fn month_lines(mem: &MemoryBlobStore, key: &str) -> Vec<ObservationValues> {
        use crate::org_storage::BlobStore;
        parse_lines(&mem.get(key).await.unwrap())
    }

    #[tokio::test]
    async fn append_then_load_round_trips() {
        let Some(db) = db().await else { return };
        let (s, mem) = storage();
        let a = obs(1, ts(2026, 3, 5), 1.5);
        let b = ObservationValues {
            explanation: Some("why".into()),
            ..obs(2, ts(2026, 3, 6), 2.5)
        };
        append_observation(&db, &s, 901, &a, None).await.unwrap();
        append_observation(&db, &s, 901, &b, None).await.unwrap();
        assert_eq!(
            mem.keys(),
            ["orgs/1/monitors/901/observations/2026-03.jsonl"]
        );
        assert_eq!(
            mem.content_type("orgs/1/monitors/901/observations/2026-03.jsonl")
                .as_deref(),
            Some("application/x-ndjson")
        );
        let got = load_observations(&s, 901, ts(2026, 3, 1), ts(2026, 3, 31))
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[&1], a);
        assert_eq!(got[&2], b);
    }

    #[tokio::test]
    async fn concurrent_appends_keep_every_observation() {
        let Some(db) = db().await else { return };
        let (s, mem) = storage();
        let s = Arc::new(s);
        let handles: Vec<_> = (1..=20)
            .map(|i| {
                let (db, s) = (db.clone(), s.clone());
                tokio::spawn(async move {
                    append_observation(&db, &s, 902, &obs(i, ts(2026, 4, 2), 1.0), None)
                        .await
                        .unwrap();
                })
            })
            .collect();
        futures::future::join_all(handles)
            .await
            .into_iter()
            .for_each(|r| r.unwrap());
        let lines = month_lines(&mem, "orgs/1/monitors/902/observations/2026-04.jsonl").await;
        assert_eq!(lines.len(), 20);
        let mut ids: Vec<i64> = lines.iter().map(|l| l.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, (1..=20).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn missing_month_file_yields_gap_not_error() {
        let (s, _) = storage();
        let got = load_observations(&s, 903, ts(2026, 1, 1), ts(2026, 5, 1))
            .await
            .unwrap();
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn load_spans_month_boundaries() {
        let Some(db) = db().await else { return };
        let (s, _) = storage();
        append_observation(&db, &s, 904, &obs(1, ts(2025, 12, 31), 1.0), None)
            .await
            .unwrap();
        append_observation(&db, &s, 904, &obs(2, ts(2026, 1, 1), 2.0), None)
            .await
            .unwrap();
        append_observation(&db, &s, 904, &obs(3, ts(2026, 3, 1), 3.0), None)
            .await
            .unwrap();
        let got = load_observations(&s, 904, ts(2025, 12, 31), ts(2026, 1, 2))
            .await
            .unwrap();
        let mut ids: Vec<i64> = got.keys().copied().collect();
        ids.sort_unstable();
        assert_eq!(ids, [1, 2]);
    }

    #[tokio::test]
    async fn set_explanation_rewrites_only_that_line() {
        let Some(db) = db().await else { return };
        let (s, mem) = storage();
        for i in 1..=3 {
            append_observation(&db, &s, 905, &obs(i, ts(2026, 5, 1 + i as u32), 1.0), None)
                .await
                .unwrap();
        }
        set_explanation(&db, &s, 905, 2, ts(2026, 5, 3), "because")
            .await
            .unwrap();
        let lines = month_lines(&mem, "orgs/1/monitors/905/observations/2026-05.jsonl").await;
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].explanation, None);
        assert_eq!(lines[1].explanation.as_deref(), Some("because"));
        assert_eq!(lines[2].explanation, None);
        assert_eq!(lines[1].value, Some(1.0));
    }

    #[tokio::test]
    async fn append_trims_lines_older_than_keep_from() {
        let Some(db) = db().await else { return };
        let (s, mem) = storage();
        for i in 1..=5 {
            append_observation(&db, &s, 907, &obs(i, ts(2026, 6, i as u32), 1.0), None)
                .await
                .unwrap();
        }
        append_observation(
            &db,
            &s,
            907,
            &obs(6, ts(2026, 6, 6), 1.0),
            Some(ts(2026, 6, 3)),
        )
        .await
        .unwrap();
        let lines = month_lines(&mem, "orgs/1/monitors/907/observations/2026-06.jsonl").await;
        let ids: Vec<_> = lines.iter().map(|l| l.id).collect();
        assert_eq!(ids, [3, 4, 5, 6]);
    }

    #[tokio::test]
    async fn set_explanation_adds_missing_line() {
        let Some(db) = db().await else { return };
        let (s, mem) = storage();
        set_explanation(&db, &s, 908, 7, ts(2026, 5, 3), "why")
            .await
            .unwrap();
        let lines = month_lines(&mem, "orgs/1/monitors/908/observations/2026-05.jsonl").await;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].id, 7);
        assert_eq!(lines[0].value, None);
        assert_eq!(lines[0].explanation.as_deref(), Some("why"));
    }

    #[tokio::test]
    async fn prune_months_before_keeps_current_and_newer_months() {
        let (s, mem) = storage();
        for (m, d) in [(1, 1), (2, 1), (3, 1), (4, 1)] {
            let rel = observation_rel_key(906, ts(2026, m, d));
            s.put(
                StorageArea::Monitors,
                &rel,
                Bytes::from_static(b"{}\n"),
                CONTENT_TYPE,
            )
            .await
            .unwrap();
        }
        let n = prune_months_before(&s, 906, ts(2026, 3, 15)).await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            mem.keys(),
            [
                "orgs/1/monitors/906/observations/2026-03.jsonl",
                "orgs/1/monitors/906/observations/2026-04.jsonl"
            ]
        );
    }

    #[test]
    fn malformed_line_is_skipped() {
        let good = serde_json::to_string(&obs(1, ts(2026, 1, 1), 1.0)).unwrap();
        let data = format!("garbage\n{good}\n");
        assert_eq!(parse_lines(data.as_bytes()).len(), 1);
    }
}

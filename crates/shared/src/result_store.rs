//! Query results are stored as gzip-compressed JSON objects in the organization's `Results` area.

use std::collections::{hash_map::Entry, HashMap};
use std::sync::OnceLock;

use anyhow::Context;
use bytes::Bytes;
use flate2::{write::GzEncoder, Compression};
use futures::future::{BoxFuture, FutureExt};
use futures::stream::{self, StreamExt};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

use crate::entities::query_result;
use crate::gzip::gunzip_bounded;
use crate::org_storage::{OrgStorage, OrgStorageResolver, StorageArea, StorageError};
use crate::secrets::hex_encode;

/// `last_used_at` is only rewritten when older than this, so hot results don't write on every read.
pub const LAST_USED_TOUCH_INTERVAL_SECS: i64 = 3600;

type FetchOutcome = (i64, Result<Option<serde_json::Value>, StorageError>);

const MAX_CONCURRENT_FETCHES: usize = 16;

pub struct StoredResult {
    /// Key relative to the `Results` area; the org root differs per backend, so relative pointers
    /// survive moving an org's objects to another bucket with the same layout.
    pub location: String,
    /// Compressed object size.
    pub size_bytes: i64,
    /// Hex SHA-256 of the exact stored bytes.
    pub sha256: String,
}

const DEFAULT_MAX_DECODED_BYTES: usize = 64 * 1024 * 1024;

/// Largest result JSON that is written or decoded; `RESULT_MAX_DECODED_BYTES`, read once.
fn max_decoded_bytes() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("RESULT_MAX_DECODED_BYTES")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_DECODED_BYTES)
    })
}

/// Fetched object bytes did not match the digest recorded when they were written.
#[derive(Debug, thiserror::Error)]
#[error("stored result failed its integrity check")]
pub struct IntegrityMismatch;

#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex_encode(&Sha256::digest(bytes))
}

/// Rows without a digest (written before digests existed) pass.
///
/// # Errors
///
/// [`IntegrityMismatch`] when a digest is recorded and differs from the bytes.
pub fn verify_digest(bytes: &[u8], expected: Option<&str>) -> Result<(), IntegrityMismatch> {
    match expected {
        Some(want) if !want.eq_ignore_ascii_case(&sha256_hex(bytes)) => Err(IntegrityMismatch),
        _ => Ok(()),
    }
}

#[must_use]
pub fn result_rel_key(data_source_id: i64, query_hash: &str) -> String {
    format!("{data_source_id}/{query_hash}.json.gz")
}

/// # Errors
///
/// Serialization or compression failure, or JSON larger than `RESULT_MAX_DECODED_BYTES`.
pub fn encode_result(data: &serde_json::Value) -> anyhow::Result<Bytes> {
    encode_result_limited(data, max_decoded_bytes())
}

fn encode_result_limited(data: &serde_json::Value, limit: usize) -> anyhow::Result<Bytes> {
    let json = serde_json::to_vec(data).context("serialize result")?;
    anyhow::ensure!(
        json.len() <= limit,
        "result is larger than the {limit}-byte limit (RESULT_MAX_DECODED_BYTES)"
    );
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    std::io::Write::write_all(&mut enc, &json).context("compress result")?;
    Ok(Bytes::from(enc.finish().context("compress result")?))
}

/// # Errors
///
/// Invalid gzip or JSON, or output larger than `RESULT_MAX_DECODED_BYTES`.
pub fn decode_result(bytes: &[u8]) -> anyhow::Result<serde_json::Value> {
    decode_result_limited(bytes, max_decoded_bytes())
}

fn decode_result_limited(bytes: &[u8], limit: usize) -> anyhow::Result<serde_json::Value> {
    let json = gunzip_bounded(bytes, limit).context("decompress result")?;
    serde_json::from_slice(&json).context("parse result")
}

/// # Errors
///
/// Encoding failure, or the storage error from the write.
pub async fn save_result(
    storage: &OrgStorage,
    data_source_id: i64,
    query_hash: &str,
    data: &serde_json::Value,
) -> Result<StoredResult, StorageError> {
    let bytes = encode_result(data)?;
    let size_bytes = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    let sha256 = sha256_hex(&bytes);
    let location = result_rel_key(data_source_id, query_hash);
    storage
        .put(StorageArea::Results, &location, bytes, "application/gzip")
        .await?;
    Ok(StoredResult {
        location,
        size_bytes,
        sha256,
    })
}

/// One batched statement that skips rows locked by other transactions, so touches never queue.
async fn touch_last_used(db: &impl ConnectionTrait, ids: &[i64]) {
    if ids.is_empty() {
        return;
    }
    let stmt = Statement::from_sql_and_values(
        db.get_database_backend(),
        "UPDATE query_results SET last_used_at = now() \
         WHERE id IN (SELECT id FROM query_results \
                      WHERE id = ANY($1) AND last_used_at < now() - $2 * interval '1 second' \
                      FOR UPDATE SKIP LOCKED)",
        [ids.to_vec().into(), LAST_USED_TOUCH_INTERVAL_SECS.into()],
    );
    if let Err(e) = db.execute_raw(stmt).await {
        tracing::warn!(count = ids.len(), error = %e, "failed to touch last_used_at");
    }
}

/// `resolver.resolve` that reports an unreachable bucket to the resolver.
async fn resolve_reporting(
    resolver_db: &DatabaseConnection,
    resolver: &dyn OrgStorageResolver,
    org_id: i64,
) -> Result<OrgStorage, StorageError> {
    let result = resolver.resolve(resolver_db, org_id).await;
    if let Err(e @ StorageError::Unreachable { .. }) = &result {
        resolver.report_failure(resolver_db, org_id, e).await;
    }
    result
}

async fn fetch_object(
    resolver_db: &DatabaseConnection,
    resolver: &dyn OrgStorageResolver,
    storage: &OrgStorage,
    org_id: i64,
    location: &str,
    expected_sha256: Option<&str>,
) -> Result<Option<serde_json::Value>, StorageError> {
    match storage.get(StorageArea::Results, location).await {
        Ok(bytes) => {
            verify_digest(&bytes, expected_sha256).map_err(anyhow::Error::from)?;
            Ok(Some(decode_result(&bytes)?))
        }
        Err(StorageError::NotFound) => {
            resolver.report_missing(resolver_db, org_id).await;
            Ok(None)
        }
        Err(e) => {
            if matches!(e, StorageError::Unreachable { .. }) {
                resolver.report_failure(resolver_db, org_id, &e).await;
            }
            Err(e)
        }
    }
}

/// Inline `data` (legacy rows) wins if present; otherwise loads `data_location`. A missing object
/// is `Ok(None)`. Bumps `last_used_at` after a successful load when it is older than
/// [`LAST_USED_TOUCH_INTERVAL_SECS`].
///
/// # Errors
///
/// Storage unreachable/switching, or an undecodable object.
pub async fn load_result(
    db: &impl ConnectionTrait,
    resolver_db: &DatabaseConnection,
    resolver: &dyn OrgStorageResolver,
    qr: &query_result::Model,
) -> Result<Option<serde_json::Value>, StorageError> {
    let data = if let Some(inline) = &qr.data {
        inline.clone()
    } else if let Some(location) = &qr.data_location {
        let storage = resolve_reporting(resolver_db, resolver, qr.org_id).await?;
        match fetch_object(
            resolver_db,
            resolver,
            &storage,
            qr.org_id,
            location,
            qr.data_sha256.as_deref(),
        )
        .await?
        {
            Some(v) => v,
            None => return Ok(None),
        }
    } else {
        return Ok(None);
    };
    touch_last_used(db, &[qr.id]).await;
    Ok(Some(data))
}

/// Loads many results with bounded parallelism; missing objects are absent from the map.
///
/// # Errors
///
/// The first storage unreachable/switching/decode error.
pub async fn load_results(
    db: &impl ConnectionTrait,
    resolver_db: &DatabaseConnection,
    resolver: &dyn OrgStorageResolver,
    qrs: &[query_result::Model],
) -> Result<HashMap<i64, serde_json::Value>, StorageError> {
    let mut out = HashMap::new();
    let mut storages: HashMap<i64, OrgStorage> = HashMap::new();
    let mut pending = Vec::new();
    for qr in qrs {
        if let Some(inline) = &qr.data {
            out.insert(qr.id, inline.clone());
        } else if let Some(location) = &qr.data_location {
            if let Entry::Vacant(e) = storages.entry(qr.org_id) {
                e.insert(resolve_reporting(resolver_db, resolver, qr.org_id).await?);
            }
            pending.push((
                qr.id,
                qr.org_id,
                location.as_str(),
                qr.data_sha256.as_deref(),
            ));
        }
    }
    let storages = &storages;
    let jobs: Vec<BoxFuture<'_, FetchOutcome>> = pending
        .into_iter()
        .map(|(id, org_id, location, sha)| {
            async move {
                let r = fetch_object(
                    resolver_db,
                    resolver,
                    &storages[&org_id],
                    org_id,
                    location,
                    sha,
                )
                .await;
                (id, r)
            }
            .boxed()
        })
        .collect();
    let fetched: Vec<_> = stream::iter(jobs)
        .buffer_unordered(MAX_CONCURRENT_FETCHES)
        .collect()
        .await;
    for (id, r) in fetched {
        if let Some(v) = r? {
            out.insert(id, v);
        }
    }
    touch_last_used(db, &out.keys().copied().collect::<Vec<_>>()).await;
    Ok(out)
}

/// The model with `data` filled from storage, or `None` when the object is gone.
///
/// # Errors
///
/// As [`load_result`].
pub async fn hydrate_result(
    db: &impl ConnectionTrait,
    resolver_db: &DatabaseConnection,
    resolver: &dyn OrgStorageResolver,
    mut qr: query_result::Model,
) -> Result<Option<query_result::Model>, StorageError> {
    Ok(load_result(db, resolver_db, resolver, &qr).await?.map(|d| {
        qr.data = Some(d);
        qr
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::org_storage::{BlobStore, StorageMode};
    use crate::test_support::blob::MemoryBlobStore;

    #[tokio::test]
    async fn save_result_writes_gzip_under_results() {
        let mem = Arc::new(MemoryBlobStore::default());
        let storage = OrgStorage::new(1, StorageMode::ArcoDash, mem.clone(), "orgs/1/");
        let data = serde_json::json!({"rows": [{"a": 1}, {"a": 2}]});
        let stored = save_result(&storage, 5, "h", &data).await.unwrap();
        assert_eq!(stored.location, "5/h.json.gz");
        let bytes = storage
            .get(StorageArea::Results, &stored.location)
            .await
            .unwrap();
        assert_eq!(mem.keys(), ["orgs/1/results/5/h.json.gz"]);
        assert_eq!(stored.size_bytes, i64::try_from(bytes.len()).unwrap());
        assert_eq!(decode_result(&bytes).unwrap(), data);
        assert_eq!(stored.sha256, sha256_hex(&bytes));
        assert_eq!(stored.sha256.len(), 64);
    }

    #[test]
    fn digest_mismatch_is_refused_and_legacy_none_passes() {
        let bytes = encode_result(&serde_json::json!({"a": 1})).unwrap();
        let good = sha256_hex(&bytes);
        assert!(verify_digest(&bytes, Some(&good)).is_ok());
        assert!(verify_digest(&bytes, None).is_ok());
        let tampered = encode_result(&serde_json::json!({"a": 2})).unwrap();
        assert!(verify_digest(&tampered, Some(&good)).is_err());
    }

    #[test]
    fn decode_refuses_gzip_bomb_over_limit() {
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&vec![b' '; 1 << 20]).unwrap();
        let bomb = enc.finish().unwrap();
        let err = decode_result_limited(&bomb, 64 * 1024).unwrap_err();
        assert!(format!("{err:#}").contains("exceeds"), "{err:#}");
    }

    #[test]
    fn encode_refuses_result_over_limit() {
        let data = serde_json::json!({"rows": ["x".repeat(1000)]});
        assert!(encode_result_limited(&data, 100).is_err());
        let ok = encode_result_limited(&data, 2000).unwrap();
        assert_eq!(decode_result_limited(&ok, 2000).unwrap(), data);
        assert!(decode_result_limited(&ok, 100).is_err());
    }

    #[test]
    fn decode_result_round_trips_encode_result() {
        let data = serde_json::json!({"x": [1, "two", null], "y": {"z": true}});
        assert_eq!(decode_result(&encode_result(&data).unwrap()).unwrap(), data);
        assert!(decode_result(b"not gzip").is_err());
    }

    mod db {
        use sea_orm::{ActiveModelTrait, ActiveValue::NotSet, ActiveValue::Set, EntityTrait};

        use super::*;
        use crate::entities::{data_source, organization};
        use crate::test_support::blob::{FailingBlobStore, StaticResolver};
        use crate::test_support::{new_data_source, new_organization, test_db};

        async fn seed(
            db: &DatabaseConnection,
            data: Option<serde_json::Value>,
            location: Option<&str>,
            last_used_ago_secs: i64,
        ) -> query_result::Model {
            let mut org = new_organization();
            org.slug = Set(format!("rs-{}", uuid::Uuid::new_v4()));
            let org: organization::Model = org.insert(db).await.unwrap();
            let ds: data_source::Model = new_data_source(org.id).insert(db).await.unwrap();
            let now = chrono::Utc::now().fixed_offset();
            query_result::ActiveModel {
                id: NotSet,
                org_id: Set(org.id),
                data_source_id: Set(ds.id),
                query_hash: Set(uuid::Uuid::new_v4().to_string()),
                query: Set("select 1".into()),
                data: Set(data),
                data_location: Set(location.map(str::to_string)),
                data_size_bytes: Set(None),
                data_sha256: Set(None),
                runtime: Set(Some(0.1)),
                retrieved_at: Set(now),
                last_used_at: Set(now - chrono::Duration::seconds(last_used_ago_secs)),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(db)
            .await
            .unwrap()
        }

        fn resolver(store: Arc<dyn crate::org_storage::BlobStore>) -> StaticResolver {
            StaticResolver::with_store(StorageMode::ArcoDash, store, "orgs/1/")
        }

        async fn last_used(
            db: &DatabaseConnection,
            id: i64,
        ) -> chrono::DateTime<chrono::FixedOffset> {
            query_result::Entity::find_by_id(id)
                .one(db)
                .await
                .unwrap()
                .unwrap()
                .last_used_at
        }

        #[tokio::test]
        async fn load_result_prefers_inline_legacy_data() {
            let Some(db) = test_db().await else { return };
            let mem = Arc::new(MemoryBlobStore::default());
            let r = resolver(mem.clone());
            let inline = serde_json::json!({"rows": [{"a": 1}]});
            let qr = seed(&db, Some(inline.clone()), Some("1/x.json.gz"), 0).await;
            assert_eq!(load_result(&db, &db, &r, &qr).await.unwrap(), Some(inline));
            assert_eq!(mem.gets(), 0);
        }

        #[tokio::test]
        async fn load_result_refuses_tampered_object_and_tolerates_missing_digest() {
            let Some(db) = test_db().await else { return };
            let mem = Arc::new(MemoryBlobStore::default());
            let r = resolver(mem.clone());
            let loc = format!("1/{}.json.gz", uuid::Uuid::new_v4());
            let original = encode_result(&serde_json::json!({"rows": [{"ssn": "111"}]})).unwrap();
            let digest = sha256_hex(&original);
            let forged = serde_json::json!({"rows": [{"ssn": "999"}]});
            mem.put(
                &format!("orgs/1/results/{loc}"),
                encode_result(&forged).unwrap(),
                "application/gzip",
            )
            .await
            .unwrap();
            let mut qr = seed(&db, None, Some(&loc), 0).await;
            qr.data_sha256 = Some(digest);
            let err = load_result(&db, &db, &r, &qr).await.unwrap_err();
            let StorageError::Other(e) = err else {
                panic!("expected Other")
            };
            assert!(e.downcast_ref::<IntegrityMismatch>().is_some());
            let msg = format!("{e:#}");
            assert!(!msg.contains("999") && !msg.contains("111"), "{msg}");
            let many = load_results(&db, &db, &r, std::slice::from_ref(&qr)).await;
            assert!(many.is_err());

            qr.data_sha256 = None;
            assert_eq!(load_result(&db, &db, &r, &qr).await.unwrap(), Some(forged));
        }

        #[tokio::test]
        async fn load_result_missing_object_is_none() {
            let Some(db) = test_db().await else { return };
            let r = resolver(Arc::new(MemoryBlobStore::default()));
            let qr = seed(&db, None, Some("1/gone.json.gz"), 0).await;
            assert_eq!(load_result(&db, &db, &r, &qr).await.unwrap(), None);
            assert_eq!(r.missing(), 1);
            let none = seed(&db, None, None, 0).await;
            assert_eq!(load_result(&db, &db, &r, &none).await.unwrap(), None);
        }

        #[tokio::test]
        async fn touch_is_throttled_to_one_write_per_interval() {
            let Some(db) = test_db().await else { return };
            let mem = Arc::new(MemoryBlobStore::default());
            let r = resolver(mem.clone());
            let data = serde_json::json!({"rows": []});
            let loc = format!("1/{}.json.gz", uuid::Uuid::new_v4());
            mem.put(
                &format!("orgs/1/results/{loc}"),
                encode_result(&data).unwrap(),
                "application/gzip",
            )
            .await
            .unwrap();
            let fresh = seed(&db, None, Some(&loc), 0).await;
            load_result(&db, &db, &r, &fresh).await.unwrap();
            load_result(&db, &db, &r, &fresh).await.unwrap();
            assert_eq!(last_used(&db, fresh.id).await, fresh.last_used_at);

            let stale = seed(&db, None, Some(&loc), 7200).await;
            load_result(&db, &db, &r, &stale).await.unwrap();
            let first = last_used(&db, stale.id).await;
            assert!(first > stale.last_used_at + chrono::Duration::seconds(7000));
            load_result(&db, &db, &r, &stale).await.unwrap();
            assert_eq!(last_used(&db, stale.id).await, first);
        }

        #[tokio::test]
        async fn touch_skips_rows_locked_by_another_transaction() {
            use sea_orm::TransactionTrait;
            let Some(db) = test_db().await else { return };
            let mem = Arc::new(MemoryBlobStore::default());
            let r = resolver(mem.clone());
            let loc = format!("1/{}.json.gz", uuid::Uuid::new_v4());
            mem.put(
                &format!("orgs/1/results/{loc}"),
                encode_result(&serde_json::json!({"rows": []})).unwrap(),
                "application/gzip",
            )
            .await
            .unwrap();
            let qr = seed(&db, None, Some(&loc), 7200).await;

            let txn = db.begin().await.unwrap();
            txn.execute_raw(Statement::from_sql_and_values(
                txn.get_database_backend(),
                "SELECT id FROM query_results WHERE id = $1 FOR UPDATE",
                [qr.id.into()],
            ))
            .await
            .unwrap();

            let loaded = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                load_result(&db, &db, &r, &qr),
            )
            .await
            .expect("touch must not wait on the row lock");
            assert!(matches!(loaded, Ok(Some(_))));
            txn.rollback().await.unwrap();
            assert_eq!(last_used(&db, qr.id).await, qr.last_used_at);
        }

        #[tokio::test]
        async fn load_results_skips_missing_and_returns_present() {
            let Some(db) = test_db().await else { return };
            let mem = Arc::new(MemoryBlobStore::default());
            let r = resolver(mem.clone());
            let data = serde_json::json!({"rows": [{"k": 1}]});
            let loc = format!("1/{}.json.gz", uuid::Uuid::new_v4());
            mem.put(
                &format!("orgs/1/results/{loc}"),
                encode_result(&data).unwrap(),
                "application/gzip",
            )
            .await
            .unwrap();
            let present = seed(&db, None, Some(&loc), 0).await;
            let missing = seed(&db, None, Some("1/none.json.gz"), 0).await;
            let legacy = seed(&db, Some(serde_json::json!({"rows": []})), None, 0).await;
            let map = load_results(
                &db,
                &db,
                &r,
                &[present.clone(), missing.clone(), legacy.clone()],
            )
            .await
            .unwrap();
            assert_eq!(map.len(), 2);
            assert_eq!(map[&present.id], data);
            assert!(map.contains_key(&legacy.id));
            assert!(!map.contains_key(&missing.id));
            assert_eq!(r.missing(), 1);
        }

        #[tokio::test]
        async fn unreachable_storage_reports_failure() {
            let Some(db) = test_db().await else { return };
            let r = resolver(Arc::new(FailingBlobStore::default()));
            let qr = seed(&db, None, Some("1/x.json.gz"), 0).await;
            let err = load_result(&db, &db, &r, &qr).await.unwrap_err();
            assert!(matches!(err, StorageError::Unreachable { .. }));
            assert_eq!(r.failures(), 1);
        }
    }
}

//! Query results are stored as gzip-compressed JSON objects in the organization's `Results` area.

use std::io::Read;

use anyhow::Context;
use bytes::Bytes;
use flate2::{read::GzDecoder, write::GzEncoder, Compression};

use crate::org_storage::{OrgStorage, StorageArea, StorageError};

pub struct StoredResult {
    /// Key relative to the `Results` area; the org root differs per backend, so relative pointers
    /// survive moving an org's objects to another bucket with the same layout.
    pub location: String,
    /// Compressed object size.
    pub size_bytes: i64,
}

#[must_use]
pub fn result_rel_key(data_source_id: i64, query_hash: &str) -> String {
    format!("{data_source_id}/{query_hash}.json.gz")
}

/// # Errors
///
/// Serialization or compression failure.
pub fn encode_result(data: &serde_json::Value) -> anyhow::Result<Bytes> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    serde_json::to_writer(&mut enc, data).context("serialize result")?;
    Ok(Bytes::from(enc.finish().context("compress result")?))
}

/// # Errors
///
/// Invalid gzip or JSON.
pub fn decode_result(bytes: &[u8]) -> anyhow::Result<serde_json::Value> {
    let mut json = Vec::new();
    GzDecoder::new(bytes)
        .read_to_end(&mut json)
        .context("decompress result")?;
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
    let location = result_rel_key(data_source_id, query_hash);
    storage
        .put(StorageArea::Results, &location, bytes, "application/gzip")
        .await?;
    Ok(StoredResult {
        location,
        size_bytes,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::org_storage::StorageMode;
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
    }

    #[test]
    fn decode_result_round_trips_encode_result() {
        let data = serde_json::json!({"x": [1, "two", null], "y": {"z": true}});
        assert_eq!(decode_result(&encode_result(&data).unwrap()).unwrap(), data);
        assert!(decode_result(b"not gzip").is_err());
    }
}

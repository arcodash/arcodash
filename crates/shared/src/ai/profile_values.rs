//! Sampled column values of an AI profile live in object storage; Postgres keeps only the
//! value-free profile. Values are customer data and are never logged.

use std::collections::BTreeMap;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::entities::data_source;
use crate::models::DataSourceAiProfile;
use crate::org_storage::{OrgStorage, StorageArea, StorageError};

/// Sampled values of one column.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnValues {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<String>,
}

impl ColumnValues {
    const fn is_empty(&self) -> bool {
        self.samples.is_empty() && self.min.is_none() && self.max.is_none()
    }
}

/// `table -> column -> values`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileValues(pub BTreeMap<String, BTreeMap<String, ColumnValues>>);

/// The profile with every sampled value removed, and the removed values.
#[must_use]
pub fn split_profile(p: &DataSourceAiProfile) -> (DataSourceAiProfile, ProfileValues) {
    let mut stripped = p.clone();
    let mut values = ProfileValues::default();
    for t in &mut stripped.tables {
        for c in &mut t.columns {
            let v = ColumnValues {
                samples: std::mem::take(&mut c.samples),
                min: c.min.take(),
                max: c.max.take(),
            };
            if !v.is_empty() {
                values
                    .0
                    .entry(t.name.clone())
                    .or_default()
                    .insert(c.name.clone(), v);
            }
        }
    }
    (stripped, values)
}

/// Fills only the empty sample/min/max fields, so inline values of a legacy row win.
pub fn merge_values(p: &mut DataSourceAiProfile, v: &ProfileValues) {
    for t in &mut p.tables {
        let Some(cols) = v.0.get(&t.name) else {
            continue;
        };
        for c in &mut t.columns {
            let Some(cv) = cols.get(&c.name) else {
                continue;
            };
            if c.samples.is_empty() {
                c.samples.clone_from(&cv.samples);
            }
            if c.min.is_none() {
                c.min.clone_from(&cv.min);
            }
            if c.max.is_none() {
                c.max.clone_from(&cv.max);
            }
        }
    }
}

/// Object key of a data source's values, relative to the AI-profile area; also the stored location.
#[must_use]
pub fn values_rel_key(data_source_id: i64) -> String {
    format!("{data_source_id}.json")
}

/// Writes the values object and returns its location.
///
/// # Errors
///
/// The storage error when the write fails.
pub async fn save_profile_values(
    storage: &OrgStorage,
    data_source_id: i64,
    values: &ProfileValues,
) -> Result<String, StorageError> {
    let rel = values_rel_key(data_source_id);
    let body = serde_json::to_vec(values).map_err(|e| StorageError::Other(e.into()))?;
    storage
        .put(
            StorageArea::AiProfiles,
            &rel,
            Bytes::from(body),
            "application/json",
        )
        .await?;
    Ok(rel)
}

/// The stored profile with its sampled values restored. A missing or unreadable values object
/// yields the profile without samples.
///
/// # Errors
///
/// The storage error when the values object cannot be read for a reason other than absence.
pub async fn load_ai_profile(
    storage: &OrgStorage,
    ds: &data_source::Model,
) -> Result<Option<DataSourceAiProfile>, StorageError> {
    let Some(raw) = &ds.ai_profile else {
        return Ok(None);
    };
    let Ok(mut profile) = serde_json::from_value::<DataSourceAiProfile>(raw.clone()) else {
        return Ok(None);
    };
    let Some(location) = &ds.ai_profile_values_location else {
        return Ok(Some(profile));
    };
    match storage.get(StorageArea::AiProfiles, location).await {
        Ok(bytes) => {
            if let Ok(values) = serde_json::from_slice::<ProfileValues>(&bytes) {
                merge_values(&mut profile, &values);
            } else {
                tracing::warn!(
                    data_source_id = ds.id,
                    "stored profile values failed to parse"
                );
            }
        }
        Err(StorageError::NotFound) => {
            tracing::warn!(data_source_id = ds.id, "profile values object missing");
        }
        Err(e) => return Err(e),
    }
    Ok(Some(profile))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AiProfileColumn, AiProfileTable};
    use crate::org_storage::StorageMode;
    use crate::test_support::blob::{FailingBlobStore, MemoryBlobStore};
    use std::sync::Arc;

    fn column(name: &str, samples: &[&str], min: Option<&str>) -> AiProfileColumn {
        AiProfileColumn {
            name: name.into(),
            null_frac: 0.1,
            distinct_in_sample: 3,
            samples: samples.iter().map(|s| (*s).to_string()).collect(),
            min: min.map(str::to_string),
            max: min.map(|_| "zz".to_string()),
            description: Some("d".into()),
        }
    }

    fn profile() -> DataSourceAiProfile {
        DataSourceAiProfile {
            tables: vec![AiProfileTable {
                name: "orders".into(),
                sampled_rows: 5,
                columns: vec![
                    column("status", &["shipped", "open"], None),
                    column("created", &[], Some("2020")),
                    column("empty", &[], None),
                ],
                data_profiled_at: None,
                error: None,
            }],
            relationships: vec![],
            schema_refreshed_at_seen: chrono::Utc::now(),
            descriptions_used_samples: false,
        }
    }

    fn storage(store: Arc<dyn crate::org_storage::BlobStore>) -> OrgStorage {
        OrgStorage::new(1, StorageMode::ArcoDash, store, "org-root")
    }

    #[test]
    fn split_then_merge_round_trips() {
        let original = profile();
        let (stripped, values) = split_profile(&original);
        for c in &stripped.tables[0].columns {
            assert!(c.samples.is_empty() && c.min.is_none() && c.max.is_none());
        }
        assert_eq!(
            stripped.tables[0].columns[0].description.as_deref(),
            Some("d")
        );
        assert!(!values.0["orders"].contains_key("empty"));
        let mut merged = stripped;
        merge_values(&mut merged, &values);
        assert_eq!(merged, original);
    }

    #[test]
    fn merge_keeps_inline_values() {
        let (_, values) = split_profile(&profile());
        let mut legacy = profile();
        legacy.tables[0].columns[0].samples = vec!["inline".into()];
        merge_values(&mut legacy, &values);
        assert_eq!(legacy.tables[0].columns[0].samples, vec!["inline"]);
    }

    #[tokio::test]
    async fn save_failure_surfaces() {
        let s = storage(Arc::new(FailingBlobStore::default()));
        let (_, values) = split_profile(&profile());
        assert!(save_profile_values(&s, 7, &values).await.is_err());
    }

    #[tokio::test]
    async fn save_returns_relative_location() {
        let s = storage(Arc::new(MemoryBlobStore::default()));
        let (_, values) = split_profile(&profile());
        let loc = save_profile_values(&s, 7, &values).await.unwrap();
        assert_eq!(loc, "7.json");
        let got = s.get(StorageArea::AiProfiles, &loc).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<ProfileValues>(&got).unwrap(),
            values
        );
    }

    fn ds(profile: Option<&DataSourceAiProfile>, location: Option<&str>) -> data_source::Model {
        let now = chrono::Utc::now().into();
        data_source::Model {
            id: 7,
            org_id: 1,
            name: "t".into(),
            r#type: "pg".into(),
            encrypted_options: None,
            scheduled_queue_name: None,
            schema: None,
            schema_refreshed_at: None,
            paused_at: None,
            pause_reason: None,
            health_state: "unknown".into(),
            health_error: None,
            health_checked_at: None,
            ai_profile: profile.map(|p| serde_json::to_value(p).unwrap()),
            ai_profile_built_at: None,
            ai_profile_last_error: None,
            ai_profile_values_location: location.map(str::to_string),
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn ai_profile_merge_restores_samples() {
        let s = storage(Arc::new(MemoryBlobStore::default()));
        let original = profile();
        let (stripped, values) = split_profile(&original);
        let loc = save_profile_values(&s, 7, &values).await.unwrap();
        let loaded = load_ai_profile(&s, &ds(Some(&stripped), Some(&loc)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded, original);

        let legacy = load_ai_profile(&s, &ds(Some(&original), None))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(legacy.tables[0].columns[0].samples, vec!["shipped", "open"]);
        assert!(load_ai_profile(&s, &ds(None, None))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn ai_profile_missing_values_object_degrades_to_no_samples() {
        let s = storage(Arc::new(MemoryBlobStore::default()));
        let (stripped, _) = split_profile(&profile());
        let loaded = load_ai_profile(&s, &ds(Some(&stripped), Some("7.json")))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded, stripped);
    }

    #[tokio::test]
    async fn unreachable_values_object_is_an_error() {
        let s = storage(Arc::new(FailingBlobStore::default()));
        let (stripped, _) = split_profile(&profile());
        assert!(load_ai_profile(&s, &ds(Some(&stripped), Some("7.json")))
            .await
            .is_err());
    }
}

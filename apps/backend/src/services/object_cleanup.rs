//! Removes stored objects once their owning row is deleted. Best-effort: a failure is logged with
//! ids only and never fails the delete; a bucket the organization supplies is never touched.

use std::sync::Arc;

use arcodash_core::org_storage::{OrgStorage, OrgStorageResolver, StorageArea};
use arcodash_core::storage::delete_upload;
use sea_orm::{ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter};

use arcodash_core::entities::data_source_upload;

use crate::error::ApiError;

pub enum DeletedObject {
    Monitors(Vec<i64>),
    DataSource { id: i64, upload_keys: Vec<String> },
}

/// Upload object keys of a data source; select before deleting its row, which unlinks them.
///
/// # Errors
/// Database failure.
pub async fn upload_keys(
    db: &impl ConnectionTrait,
    data_source_id: i64,
) -> Result<Vec<String>, ApiError> {
    Ok(data_source_upload::Entity::find()
        .filter(data_source_upload::Column::DataSourceId.eq(data_source_id))
        .all(db)
        .await?
        .into_iter()
        .map(|u| u.object_key)
        .collect())
}

/// Deletes the stored objects of `deleted`, only from storage the operator manages.
pub async fn run(
    resolver: Arc<dyn OrgStorageResolver>,
    pool: DatabaseConnection,
    org_id: i64,
    deleted: DeletedObject,
) {
    let Ok(storage) = resolver.resolve(&pool, org_id).await else {
        tracing::warn!(org_id, "object cleanup skipped: storage unavailable");
        return;
    };
    if !storage.manages_retention() {
        return;
    }
    match deleted {
        DeletedObject::Monitors(ids) => {
            for id in ids {
                if storage
                    .delete_prefix(StorageArea::Monitors, &format!("{id}/"))
                    .await
                    .is_err()
                {
                    tracing::warn!(org_id, monitor_id = id, "monitor object cleanup failed");
                }
            }
        }
        DeletedObject::DataSource { id, upload_keys } => {
            data_source_objects(&*resolver, &pool, &storage, org_id, id, &upload_keys).await;
        }
    }
}

async fn data_source_objects(
    resolver: &dyn OrgStorageResolver,
    pool: &DatabaseConnection,
    storage: &OrgStorage,
    org_id: i64,
    id: i64,
    upload_keys: &[String],
) {
    let mut failed = storage
        .delete_prefix(StorageArea::Results, &format!("{id}/"))
        .await
        .is_err();
    failed |= storage
        .delete(StorageArea::AiProfiles, &format!("{id}.json"))
        .await
        .is_err();
    for key in upload_keys {
        failed |= delete_upload(resolver, pool, org_id, key).await.is_err();
    }
    if failed {
        tracing::warn!(
            org_id,
            data_source_id = id,
            "data source object cleanup failed"
        );
    }
}

//! Per-organization object-storage seam. Every customer-data area is read and written through an
//! `OrgStorage`, which an embedding deployment can resolve to its own bucket and credentials.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use sea_orm::DatabaseConnection;

use crate::storage::ObjectStore;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StorageArea {
    Results,
    Monitors,
    Emails,
    Narratives,
    AiProfiles,
    Uploads,
}

impl StorageArea {
    pub const ALL: [Self; 6] = [
        Self::Results,
        Self::Monitors,
        Self::Emails,
        Self::Narratives,
        Self::AiProfiles,
        Self::Uploads,
    ];

    #[must_use]
    pub const fn folder(self) -> &'static str {
        match self {
            Self::Results => "results",
            Self::Monitors => "monitors",
            Self::Emails => "emails",
            Self::Narratives => "narratives",
            Self::AiProfiles => "ai-profiles",
            Self::Uploads => "uploads",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("object not found")]
    NotFound,
    #[error("storage switch in progress")]
    Switching,
    /// `detail` carries the provider's code/message only, never customer values.
    #[error("storage unreachable: {detail}")]
    Unreachable { detail: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put(&self, key: &str, bytes: Bytes, content_type: &str) -> Result<(), StorageError>;
    /// A missing object is `Err(NotFound)`.
    async fn get(&self, key: &str) -> Result<Bytes, StorageError>;
    /// A missing object is `Ok`.
    async fn delete(&self, key: &str) -> Result<(), StorageError>;
    /// `(full key, size in bytes)` for every object under `prefix`.
    async fn list_prefix(&self, prefix: &str) -> Result<Vec<(String, u64)>, StorageError>;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StorageMode {
    /// The operator's own bucket.
    ArcoDash,
    /// A bucket supplied by the organization.
    Customer,
}

#[derive(Clone)]
pub struct OrgStorage {
    pub org_id: i64,
    pub mode: StorageMode,
    store: Arc<dyn BlobStore>,
    root: String,
    writes_paused: bool,
}

/// Drops empty, `.`, and `..` segments so a relative path can never leave its area folder.
fn sanitize_rel(rel: &str) -> String {
    rel.split('/')
        .filter(|seg| !seg.is_empty() && *seg != "." && *seg != "..")
        .collect::<Vec<_>>()
        .join("/")
}

impl OrgStorage {
    pub fn new(
        org_id: i64,
        mode: StorageMode,
        store: Arc<dyn BlobStore>,
        root: impl Into<String>,
    ) -> Self {
        let root: String = root.into();
        let trimmed = root.trim_start_matches('/').trim_end_matches('/');
        let root = if trimmed.is_empty() {
            String::new()
        } else {
            format!("{trimmed}/")
        };
        Self {
            org_id,
            mode,
            store,
            root,
            writes_paused: false,
        }
    }

    /// Rejects writes while an organization's storage is being moved.
    #[must_use]
    pub const fn with_writes_paused(mut self) -> Self {
        self.writes_paused = true;
        self
    }

    #[must_use]
    pub const fn writes_paused(&self) -> bool {
        self.writes_paused
    }

    #[must_use]
    pub fn root(&self) -> &str {
        &self.root
    }

    #[must_use]
    pub const fn store(&self) -> &Arc<dyn BlobStore> {
        &self.store
    }

    #[must_use]
    pub fn key(&self, area: StorageArea, rel: &str) -> String {
        let rel = sanitize_rel(rel);
        let slash = if rel.is_empty() { "" } else { "/" };
        format!("{}{}{slash}{rel}", self.root, area.folder())
    }

    #[must_use]
    pub const fn manages_retention(&self) -> bool {
        matches!(self.mode, StorageMode::ArcoDash)
    }

    const fn check_writable(&self) -> Result<(), StorageError> {
        if self.writes_paused {
            Err(StorageError::Switching)
        } else {
            Ok(())
        }
    }

    /// # Errors
    ///
    /// `Switching` while writes are paused; otherwise the store's error.
    pub async fn put(
        &self,
        area: StorageArea,
        rel: &str,
        bytes: Bytes,
        content_type: &str,
    ) -> Result<(), StorageError> {
        self.check_writable()?;
        self.store
            .put(&self.key(area, rel), bytes, content_type)
            .await
    }

    /// # Errors
    ///
    /// `NotFound` when the object does not exist; otherwise the store's error.
    pub async fn get(&self, area: StorageArea, rel: &str) -> Result<Bytes, StorageError> {
        self.store.get(&self.key(area, rel)).await
    }

    /// # Errors
    ///
    /// `Switching` while writes are paused; otherwise the store's error.
    pub async fn delete(&self, area: StorageArea, rel: &str) -> Result<(), StorageError> {
        self.check_writable()?;
        self.store.delete(&self.key(area, rel)).await
    }

    /// Returns the number of objects deleted.
    ///
    /// # Errors
    ///
    /// `Switching` while writes are paused; otherwise the store's error.
    pub async fn delete_prefix(
        &self,
        area: StorageArea,
        rel_prefix: &str,
    ) -> Result<usize, StorageError> {
        self.check_writable()?;
        let objects = self
            .store
            .list_prefix(&self.prefix_key(area, rel_prefix))
            .await?;
        let count = objects.len();
        for (key, _) in objects {
            self.store.delete(&key).await?;
        }
        Ok(count)
    }

    /// Keys come back relative to the area folder.
    ///
    /// # Errors
    ///
    /// The store's error.
    pub async fn list(
        &self,
        area: StorageArea,
        rel_prefix: &str,
    ) -> Result<Vec<(String, u64)>, StorageError> {
        let folder = format!("{}{}/", self.root, area.folder());
        let objects = self
            .store
            .list_prefix(&self.prefix_key(area, rel_prefix))
            .await?;
        Ok(objects
            .into_iter()
            .map(|(key, size)| {
                let rel = key.strip_prefix(&folder).unwrap_or(&key).to_string();
                (rel, size)
            })
            .collect())
    }

    /// Like `key`, but preserves a trailing `/` on the caller's prefix.
    fn prefix_key(&self, area: StorageArea, rel_prefix: &str) -> String {
        let mut key = self.key(area, rel_prefix);
        let sanitized = sanitize_rel(rel_prefix);
        if (sanitized.is_empty()
            || sanitized != rel_prefix.trim_end_matches('/')
            || rel_prefix.ends_with('/'))
            && !key.ends_with('/')
        {
            key.push('/');
        }
        key
    }
}

/// Deletes every area folder under the organization's root. Called by deployments that delete a
/// whole organization; nothing in this repository does.
///
/// A bucket the organization supplies is left untouched (`Ok(0)`); otherwise returns the number of
/// objects deleted.
///
/// # Errors
///
/// `Switching` while writes are paused; otherwise the store's error.
pub async fn delete_org_objects(storage: &OrgStorage) -> Result<usize, StorageError> {
    if !storage.manages_retention() {
        return Ok(0);
    }
    let mut total = 0;
    for area in StorageArea::ALL {
        total += storage.delete_prefix(area, "").await?;
    }
    Ok(total)
}

#[async_trait]
pub trait OrgStorageResolver: Send + Sync {
    async fn resolve(
        &self,
        db: &DatabaseConnection,
        org_id: i64,
    ) -> Result<OrgStorage, StorageError>;

    /// Called after a failed storage operation.
    async fn report_failure(&self, _db: &DatabaseConnection, _org_id: i64, _err: &StorageError) {}

    /// Called when a metadata row points at an object that no longer exists.
    async fn report_missing(&self, _db: &DatabaseConnection, _org_id: i64) {}
}

/// Every organization shares one bucket, partitioned by an `orgs/<id>/` root.
pub struct DefaultStorageResolver {
    store: Arc<ObjectStore>,
}

impl DefaultStorageResolver {
    #[must_use]
    pub const fn new(store: Arc<ObjectStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl OrgStorageResolver for DefaultStorageResolver {
    async fn resolve(
        &self,
        _db: &DatabaseConnection,
        org_id: i64,
    ) -> Result<OrgStorage, StorageError> {
        Ok(OrgStorage::new(
            org_id,
            StorageMode::ArcoDash,
            self.store.clone(),
            format!("orgs/{org_id}/"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::blob::MemoryBlobStore;

    fn storage(mode: StorageMode, root: &str) -> (OrgStorage, Arc<MemoryBlobStore>) {
        let mem = Arc::new(MemoryBlobStore::default());
        (OrgStorage::new(7, mode, mem.clone(), root), mem)
    }

    #[tokio::test]
    async fn delete_org_objects_removes_every_area_in_managed_storage_only() {
        let seed = || {
            let mut keys: Vec<String> = StorageArea::ALL
                .iter()
                .map(|a| format!("orgs/7/{}/x/y", a.folder()))
                .collect();
            keys.push("orgs/70/results/x".to_string());
            keys
        };
        let (s, mem) = storage(StorageMode::ArcoDash, "orgs/7/");
        for k in seed() {
            mem.put(&k, Bytes::from_static(b"x"), "text/plain")
                .await
                .unwrap();
        }
        assert_eq!(
            delete_org_objects(&s).await.unwrap(),
            StorageArea::ALL.len()
        );
        assert_eq!(mem.keys(), ["orgs/70/results/x"]);

        let (c, cmem) = storage(StorageMode::Customer, "orgs/7/");
        for k in seed() {
            cmem.put(&k, Bytes::from_static(b"x"), "text/plain")
                .await
                .unwrap();
        }
        assert_eq!(delete_org_objects(&c).await.unwrap(), 0);
        assert_eq!(cmem.keys().len(), StorageArea::ALL.len() + 1);
        assert_eq!(cmem.deletes(), 0);
    }

    #[tokio::test]
    async fn empty_rel_prefix_stays_inside_the_area_folder() {
        let (s, mem) = storage(StorageMode::ArcoDash, "");
        for k in ["results/a", "results-old/b"] {
            mem.put(k, Bytes::from_static(b"x"), "text/plain")
                .await
                .unwrap();
        }
        for p in ["", ".", ".."] {
            assert_eq!(s.list(StorageArea::Results, p).await.unwrap().len(), 1);
        }
        assert_eq!(s.delete_prefix(StorageArea::Results, "").await.unwrap(), 1);
        assert_eq!(mem.keys(), ["results-old/b"]);
    }

    #[tokio::test]
    async fn dot_dot_prefix_never_widens_to_sibling_ids() {
        let (s, mem) = storage(StorageMode::ArcoDash, "");
        for k in ["results/3/a", "results/30/b"] {
            mem.put(k, Bytes::from_static(b"x"), "text/plain")
                .await
                .unwrap();
        }
        assert_eq!(s.list(StorageArea::Results, "3/..").await.unwrap().len(), 1);
        assert_eq!(s.list(StorageArea::Results, "3/.").await.unwrap().len(), 1);
    }

    #[test]
    fn key_layout_matches_spec() {
        let (s, _) = storage(StorageMode::ArcoDash, "orgs/7/");
        assert_eq!(
            s.key(StorageArea::Results, "3/abc.json.gz"),
            "orgs/7/results/3/abc.json.gz"
        );
        assert_eq!(StorageArea::AiProfiles.folder(), "ai-profiles");
        let folders: Vec<_> = StorageArea::ALL.iter().map(|a| a.folder()).collect();
        assert_eq!(
            folders,
            [
                "results",
                "monitors",
                "emails",
                "narratives",
                "ai-profiles",
                "uploads"
            ]
        );
    }

    #[test]
    fn customer_root_without_trailing_slash_is_normalized() {
        let (s, _) = storage(StorageMode::Customer, "acme/arcodash");
        assert_eq!(s.key(StorageArea::Results, "x"), "acme/arcodash/results/x");
        let (s, _) = storage(StorageMode::Customer, "");
        assert_eq!(s.key(StorageArea::Results, "x"), "results/x");
        let (s, _) = storage(StorageMode::Customer, "/a//");
        assert_eq!(s.root(), "a/");
    }

    #[test]
    fn manages_retention_only_in_arcodash_mode() {
        assert!(storage(StorageMode::ArcoDash, "r/").0.manages_retention());
        assert!(!storage(StorageMode::Customer, "r/").0.manages_retention());
    }

    #[tokio::test]
    async fn paused_storage_rejects_writes_but_allows_reads() {
        let (s, mem) = storage(StorageMode::ArcoDash, "orgs/7/");
        s.put(
            StorageArea::Results,
            "a",
            Bytes::from_static(b"1"),
            "text/plain",
        )
        .await
        .unwrap();
        let puts_before = mem.puts();
        let s = s.with_writes_paused();
        assert!(s.writes_paused());
        assert!(matches!(
            s.put(
                StorageArea::Results,
                "b",
                Bytes::from_static(b"2"),
                "text/plain"
            )
            .await,
            Err(StorageError::Switching)
        ));
        assert!(matches!(
            s.delete(StorageArea::Results, "a").await,
            Err(StorageError::Switching)
        ));
        assert!(matches!(
            s.delete_prefix(StorageArea::Results, "").await,
            Err(StorageError::Switching)
        ));
        assert_eq!(mem.puts(), puts_before);
        assert_eq!(mem.deletes(), 0);
        assert_eq!(s.get(StorageArea::Results, "a").await.unwrap(), "1");
    }

    #[tokio::test]
    async fn delete_prefix_removes_only_matching_keys() {
        let (s, mem) = storage(StorageMode::ArcoDash, "orgs/7/");
        for rel in ["3/a", "3/b", "30/c", "4/d"] {
            s.put(
                StorageArea::Results,
                rel,
                Bytes::from_static(b"x"),
                "text/plain",
            )
            .await
            .unwrap();
        }
        s.put(
            StorageArea::Emails,
            "3/a",
            Bytes::from_static(b"x"),
            "text/plain",
        )
        .await
        .unwrap();
        let n = s.delete_prefix(StorageArea::Results, "3/").await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            mem.keys(),
            vec![
                "orgs/7/emails/3/a",
                "orgs/7/results/30/c",
                "orgs/7/results/4/d"
            ]
        );
        let listed = s.list(StorageArea::Results, "").await.unwrap();
        let rels: Vec<_> = listed.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(rels, ["30/c", "4/d"]);
    }

    #[test]
    fn key_neutralizes_path_traversal() {
        let (s, _) = storage(StorageMode::ArcoDash, "orgs/7/");
        assert_eq!(
            s.key(StorageArea::Results, "../../8/results/x"),
            "orgs/7/results/8/results/x"
        );
        assert_eq!(
            s.key(StorageArea::Results, "/etc/passwd"),
            "orgs/7/results/etc/passwd"
        );
        assert_eq!(s.key(StorageArea::Results, "a/../b"), "orgs/7/results/a/b");
        assert!(!s.key(StorageArea::Results, "..").contains(".."));
    }
}

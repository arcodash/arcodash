use arcodash_core::org_storage::{
    DefaultStorageResolver, OrgStorageResolver, StorageArea, StorageError, StorageMode,
};
use arcodash_core::storage::ObjectStore;
use bytes::Bytes;
use sea_orm::DatabaseConnection;

#[tokio::test]
async fn default_resolver_round_trips_and_maps_404_to_not_found() {
    let Ok(host) = std::env::var("STORAGE_EMULATOR_HOST") else {
        eprintln!("skipping: STORAGE_EMULATOR_HOST not set");
        return;
    };
    let bucket =
        std::env::var("UPLOAD_BUCKET").unwrap_or_else(|_| "arcodash-test-uploads".to_string());
    let store = ObjectStore::for_emulator(&host, &bucket).unwrap();
    store.ensure_bucket().await.unwrap();

    let resolver = DefaultStorageResolver::new(store);
    let db = DatabaseConnection::default();
    let org_id = 987_654;
    let storage = resolver.resolve(&db, org_id).await.unwrap();
    assert_eq!(storage.org_id, org_id);
    assert_eq!(storage.mode, StorageMode::ArcoDash);
    assert!(storage.manages_retention());

    let rel = format!("{}/probe.json", uuid::Uuid::new_v4());
    assert!(storage
        .key(StorageArea::Results, &rel)
        .starts_with("orgs/987654/results/"));

    storage
        .put(
            StorageArea::Results,
            &rel,
            Bytes::from_static(b"{\"a\":1}"),
            "application/json",
        )
        .await
        .unwrap();
    assert_eq!(
        storage.get(StorageArea::Results, &rel).await.unwrap(),
        "{\"a\":1}"
    );
    let listed = storage.list(StorageArea::Results, &rel).await.unwrap();
    assert_eq!(listed, vec![(rel.clone(), 7)]);

    storage.delete(StorageArea::Results, &rel).await.unwrap();
    assert!(matches!(
        storage.get(StorageArea::Results, &rel).await,
        Err(StorageError::NotFound)
    ));
    storage.delete(StorageArea::Results, &rel).await.unwrap();
}

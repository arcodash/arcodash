//! Upload ownership checks — cross-org rejection and same-org linking.
//! Exercises the real DB rather than a mock: the whole point of this check is a query filtered
//! on `org_id`, which a fake/in-memory connection wouldn't meaningfully prove.

mod common;
use common::{make_org, spawn_app};

/// `verify_upload_ownership` then `link_upload`, as the create/update handlers run them.
#[allow(clippy::result_large_err)] // ApiError is large by design; test helper mirrors the service signature
async fn verify_and_link_upload(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    options: &serde_json::Value,
    type_name: &str,
    ds_id: i64,
) -> Result<(), arcodash_backend::error::ApiError> {
    if let Some(upload_id) = verify_upload_ownership(db, org_id, options, type_name).await? {
        link_upload(db, upload_id, ds_id).await?;
    }
    Ok(())
}

async fn insert_upload(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
) -> arcodash_core::entities::data_source_upload::Model {
    use arcodash_core::entities::data_source_upload;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};

    let model = data_source_upload::ActiveModel {
        id: Set(uuid::Uuid::new_v4()),
        org_id: Set(org_id),
        object_key: Set(format!("orgs/{org_id}/uploads/csv/test/data.csv")),
        filename: Set("data.csv".to_string()),
        byte_size: Set(42),
        content_type: Set("text/csv".to_string()),
        created_by: Set(None),
        data_source_id: Set(None),
        created_at: Set(chrono::Utc::now().into()),
    };
    model.insert(db).await.expect("upload insert failed")
}

/// A caller in a different org than the upload row must be rejected — otherwise any org could
/// link (and thereby read, via the connector's `store.get(object_key)`) another org's uploaded
/// file just by guessing its UUID.
#[tokio::test]
async fn cross_org_upload_reference_is_forbidden() {
    let app = skip_if_none!(spawn_app().await);

    let org_a = make_org(&app.db).await;
    let org_b = make_org(&app.db).await;
    let upload = insert_upload(&app.db, org_a).await;

    let options = serde_json::json!({ "upload_id": upload.id.to_string() });
    let err = verify_and_link_upload(&app.db, org_b, &options, "csv", 999)
        .await
        .expect_err("upload from another org must not be linkable");
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
}

/// A caller in the upload's own org links it successfully, setting `data_source_id`.
#[tokio::test]
async fn same_org_upload_reference_links_the_data_source() {
    let app = skip_if_none!(spawn_app().await);

    let org_a = make_org(&app.db).await;
    let upload = insert_upload(&app.db, org_a).await;
    // A real row, not a made-up id — `data_source_id` has a live FK to `data_sources.id`.
    let ds = arcodash_core::test_support::new_data_source(org_a)
        .insert(&app.db)
        .await
        .expect("data source insert failed");

    let options = serde_json::json!({ "upload_id": upload.id.to_string() });
    verify_and_link_upload(&app.db, org_a, &options, "csv", ds.id)
        .await
        .expect("same-org upload reference must link");

    use arcodash_core::entities::data_source_upload;
    use sea_orm::EntityTrait;
    let reloaded = data_source_upload::Entity::find_by_id(upload.id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("upload row must still exist");
    assert_eq!(reloaded.data_source_id, Some(ds.id));
}

/// A non-csv/sqlite type is a no-op — no upload lookup, no error, regardless of `options`.
#[tokio::test]
async fn non_upload_type_is_a_no_op() {
    let app = skip_if_none!(spawn_app().await);

    let options = serde_json::json!({ "upload_id": "not-even-a-uuid" });
    verify_and_link_upload(&app.db, 1, &options, "pg", 1)
        .await
        .expect("non-upload connector types must never consult upload_id");
}

// ── Ordering guarantee: verify-before-persist on create/update ─────────────────────────────
//
// `handlers::data_sources::create`/`update` must reject a cross-org `upload_id` *before*
// writing the data source row — the per-request transaction in `middleware/org.rs` commits
// regardless of the handler's response status, so checking only after a write would leave a
// rejected cross-org reference persisted anyway. These mirror the handlers' exact call order
// using the split `verify_upload_ownership`/`link_upload`/`plan_update` functions.

use arcodash_backend::services::data_source_service::{
    create, link_upload, plan_update, update, verify_upload_ownership,
};
use arcodash_core::connectors::registry::ConnectorRegistry;
use arcodash_core::entities::data_source;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};

/// Same fixed order as `handlers::data_sources::create`.
#[allow(clippy::result_large_err)] // ApiError is large by design; test helper mirrors the service signature
async fn simulate_create(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    options: &serde_json::Value,
    type_name: &str,
) -> Result<i64, arcodash_backend::error::ApiError> {
    let upload_id = verify_upload_ownership(db, org_id, options, type_name).await?;
    let registry = ConnectorRegistry::new();
    let key = [7u8; 32];
    let ds = create(
        db,
        &registry,
        &key,
        org_id,
        "Test".to_string(),
        type_name.to_string(),
        options.clone(),
    )
    .await?;
    if let Some(upload_id) = upload_id {
        link_upload(db, upload_id, ds.id).await?;
    }
    Ok(ds.id)
}

#[tokio::test]
async fn cross_org_upload_id_on_create_is_rejected_before_any_data_source_row_is_inserted() {
    let app = skip_if_none!(spawn_app().await);

    let org_a = make_org(&app.db).await;
    let org_b = make_org(&app.db).await;
    let upload = insert_upload(&app.db, org_a).await;
    let options = serde_json::json!({ "upload_id": upload.id.to_string() });

    let err = simulate_create(&app.db, org_b, &options, "csv")
        .await
        .expect_err("a cross-org upload_id must not be creatable");
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);

    let count = data_source::Entity::find()
        .filter(data_source::Column::OrgId.eq(org_b))
        .count(&app.db)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "a rejected cross-org upload_id must leave no data source row behind"
    );
}

/// Same fixed order as `handlers::data_sources::update`: resolve what `update()` is about to
/// persist via `plan_update`, verify ownership against that, only then call `update()`.
#[allow(clippy::result_large_err)] // ApiError is large by design; test helper mirrors the service signature
async fn simulate_update(
    db: &sea_orm::DatabaseConnection,
    registry: &ConnectorRegistry,
    key: &[u8; 32],
    org_id: i64,
    existing: data_source::Model,
    new_options: serde_json::Value,
) -> Result<i64, arcodash_backend::error::ApiError> {
    let (final_type, merged) = plan_update(registry, key, &existing, None, Some(&new_options))?;
    let upload_id = match &merged {
        Some(m) => verify_upload_ownership(db, org_id, m, &final_type).await?,
        None => None,
    };
    let updated = update(db, registry, key, existing, None, None, Some(new_options)).await?;
    if let Some(upload_id) = upload_id {
        link_upload(db, upload_id, updated.id).await?;
    }
    Ok(updated.id)
}

#[tokio::test]
async fn cross_org_upload_id_on_update_is_rejected_before_options_are_persisted() {
    let app = skip_if_none!(spawn_app().await);

    let org_a = make_org(&app.db).await;
    let org_b = make_org(&app.db).await;
    let upload = insert_upload(&app.db, org_a).await;

    let mut model = arcodash_core::test_support::new_data_source(org_b);
    model.r#type = sea_orm::ActiveValue::Set("csv".to_string());
    let existing = model
        .insert(&app.db)
        .await
        .expect("data source insert failed");
    let options_before = existing.encrypted_options.clone();

    let registry = ConnectorRegistry::new();
    let key = [7u8; 32];
    let new_options = serde_json::json!({ "upload_id": upload.id.to_string() });

    let err = simulate_update(
        &app.db,
        &registry,
        &key,
        org_b,
        existing.clone(),
        new_options,
    )
    .await
    .expect_err("a cross-org upload_id must not be linkable via update");
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);

    let reloaded = data_source::Entity::find_by_id(existing.id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("data source row must still exist");
    assert_eq!(
        reloaded.encrypted_options, options_before,
        "a rejected cross-org upload_id must leave the row's persisted options untouched"
    );
}

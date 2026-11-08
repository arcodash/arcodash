//! A caught unique-violation insert must run inside its own nested scope so it doesn't poison the
//! enclosing transaction — verified directly against Postgres under both `Db::Txn` (a per-request
//! transaction) and `Db::Pool` (a plain pooled connection), and against
//! `permission_service::grant`'s catch-path recovery under a real concurrent conflict.

mod common;
use common::{make_org, spawn_app, user_with_role};

use std::sync::Arc;
use std::time::Duration;

use arcodash_backend::api::handlers::groups::{self, AddRoleBody, AssignDataSourceBody};
use arcodash_backend::auth::{CurrentOrg, CurrentUser};
use arcodash_backend::db::{insert_scoped, Db, PostCommitJobs};
use arcodash_backend::error::is_unique_violation;
use arcodash_backend::extract::{Json as ExtractJson, Path};
use arcodash_backend::services::{dashboard_service, permission_service};
use arcodash_core::entities::{
    access_permission, data_source_group, favorite, group, group_role, organization, role,
};
use arcodash_core::test_support::{new_dashboard, new_data_source, new_query};
use sea_orm::{
    ActiveModelTrait, ActiveValue::NotSet, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter,
    TransactionTrait,
};

async fn seed_org(db: &sea_orm::DatabaseConnection, label: &str) -> i64 {
    organization::Entity::insert(organization::ActiveModel {
        id: NotSet,
        name: Set(format!("{label} org")),
        slug: Set(format!("{label}-{}", uuid::Uuid::new_v4().simple())),
        settings: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
    })
    .exec(db)
    .await
    .expect("org insert failed")
    .last_insert_id
}

fn group_model(org_id: i64, name: &str) -> group::ActiveModel {
    let now = chrono::Utc::now();
    group::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set(name.to_string()),
        permissions: Set(vec![]),
        r#type: Set("custom".to_string()),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
        data_source_access_restricted: Set(false),
    }
}

async fn group_exists(db: &sea_orm::DatabaseConnection, org_id: i64, name: &str) -> bool {
    group::Entity::find()
        .filter(group::Column::OrgId.eq(org_id))
        .filter(group::Column::Name.eq(name))
        .one(db)
        .await
        .unwrap()
        .is_some()
}

#[tokio::test]
async fn insert_scoped_keeps_the_enclosing_txn_usable_under_db_txn() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = seed_org(&app.db, "insert-scoped-txn").await;
    let name = format!("dup-{}", uuid::Uuid::new_v4().simple());

    group_model(org_id, &name)
        .insert(&app.db)
        .await
        .expect("seed group insert failed");

    let txn = app.db.begin().await.expect("begin failed");
    let db = Db::Txn(Arc::new(txn), PostCommitJobs::default());

    let err = insert_scoped(&db, group_model(org_id, &name))
        .await
        .expect_err("duplicate insert must fail");
    assert!(
        is_unique_violation(&err),
        "expected a unique-violation DbErr, got: {err}"
    );

    let after_name = format!("after-{}", uuid::Uuid::new_v4().simple());
    group_model(org_id, &after_name)
        .insert(&db)
        .await
        .expect("a statement after the caught violation must still succeed under Db::Txn");

    let Db::Txn(arc_txn, _) = db else {
        unreachable!("db was constructed as Db::Txn above")
    };
    let txn = Arc::try_unwrap(arc_txn).unwrap_or_else(|_| panic!("txn still shared"));
    txn.commit().await.expect("outer commit must succeed");

    assert!(
        group_exists(&app.db, org_id, &name).await,
        "the original row must survive the outer commit"
    );
    assert!(
        group_exists(&app.db, org_id, &after_name).await,
        "the statement issued after the caught violation must have committed"
    );
}

/// Negative control: without the nested scope, the same shape poisons the enclosing transaction
/// and the following statement also fails. Without this, the assertions above could pass
/// vacuously if `insert_scoped` weren't actually doing anything.
#[tokio::test]
async fn plain_insert_after_a_caught_violation_fails_the_following_statement_under_db_txn() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = seed_org(&app.db, "insert-scoped-control").await;
    let name = format!("dup-{}", uuid::Uuid::new_v4().simple());

    group_model(org_id, &name)
        .insert(&app.db)
        .await
        .expect("seed group insert failed");

    let txn = app.db.begin().await.expect("begin failed");
    let db = Db::Txn(Arc::new(txn), PostCommitJobs::default());

    group_model(org_id, &name)
        .insert(&db)
        .await
        .expect_err("duplicate insert must fail");

    let after_name = format!("after-{}", uuid::Uuid::new_v4().simple());
    let result = group_model(org_id, &after_name).insert(&db).await;
    assert!(
        result.is_err(),
        "a plain insert (no nested scope) must leave the transaction aborted, \
         failing the following statement"
    );
}

#[tokio::test]
async fn insert_scoped_behaves_identically_under_db_pool() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = seed_org(&app.db, "insert-scoped-pool").await;
    let name = format!("dup-{}", uuid::Uuid::new_v4().simple());

    group_model(org_id, &name)
        .insert(&app.db)
        .await
        .expect("seed group insert failed");

    let db = Db::Pool(app.db.clone());
    let err = insert_scoped(&db, group_model(org_id, &name))
        .await
        .expect_err("duplicate insert must fail under Db::Pool too");
    assert!(is_unique_violation(&err));

    let after_name = format!("after-{}", uuid::Uuid::new_v4().simple());
    group_model(org_id, &after_name)
        .insert(&db)
        .await
        .expect("Db::Pool has no ambient transaction to poison; the next statement must succeed");
}

/// Two concurrent grants for the same `(object, grantee)` under `Db::Txn`: the loser's insert
/// blocks on the unique index until the winner commits, then hits the caught-conflict path in
/// `permission_service::grant`, which must recover via its `find_existing` + `UPDATE` rather than
/// erroring or leaving the enclosing transaction unusable.
#[tokio::test]
async fn concurrent_grant_recovers_via_the_catch_path_update() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (grantor, _) = user_with_role(&app.db, org_id, &["edit_query", "view_query"]).await;
    let (grantee, _) = user_with_role(&app.db, org_id, &[]).await;

    let ds = new_data_source(org_id).insert(&app.db).await.unwrap();
    let query = new_query(org_id, ds.id, grantor.id)
        .insert(&app.db)
        .await
        .unwrap();

    let txn_a = app.db.begin().await.expect("begin failed");
    let now = chrono::Utc::now();
    access_permission::ActiveModel {
        id: NotSet,
        object_type: Set("query".to_string()),
        object_id: Set(query.id),
        access_type: Set("viewer".to_string()),
        grantor_id: Set(grantor.id),
        grantee_id: Set(Some(grantee.id)),
        grantee_group_id: Set(None),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    }
    .insert(&txn_a)
    .await
    .expect("seed conflicting row failed");

    let db = app.db.clone();
    let grantor_for_b = grantor.clone();
    let grantee_id = grantee.id;
    let query_id = query.id;
    let handle = tokio::spawn(async move {
        let txn_b = db.begin().await.expect("begin failed");
        let db_b = Db::Txn(Arc::new(txn_b), PostCommitJobs::default());

        let model = permission_service::grant(
            &db_b,
            org_id,
            &grantor_for_b,
            "query",
            query_id,
            Some(grantee_id),
            None,
            "editor",
        )
        .await?;

        // A statement issued by the caller after `grant` returns — proves the transaction
        // survived the catch path, not just that `grant`'s own internal write did.
        arcodash_core::events::record_event(
            &db_b,
            org_id,
            Some(grantor_for_b.id),
            "test.marker",
            None,
            None,
            serde_json::json!({}),
        )
        .await
        .map_err(arcodash_backend::error::ApiError::from)?;

        let Db::Txn(arc_txn, _) = db_b else {
            unreachable!("db_b was constructed as Db::Txn above")
        };
        let txn_b = Arc::try_unwrap(arc_txn).unwrap_or_else(|_| panic!("txn still shared"));
        txn_b.commit().await.expect("B's commit must succeed");
        Ok::<_, arcodash_backend::error::ApiError>(model)
    });

    // Give B's SELECT time to miss (A is still uncommitted) and its INSERT time to start
    // blocking on the unique index before A commits.
    tokio::time::sleep(Duration::from_millis(300)).await;
    txn_a.commit().await.expect("commit failed");

    let model = handle
        .await
        .expect("spawned task panicked")
        .expect("grant and the follow-up statement must not error out of this race");
    assert_eq!(
        model.access_type, "editor",
        "the catch path's UPDATE must apply the racing call's access_type"
    );

    let rows = access_permission::Entity::find()
        .filter(access_permission::Column::ObjectType.eq("query"))
        .filter(access_permission::Column::ObjectId.eq(query.id))
        .filter(access_permission::Column::GranteeId.eq(grantee.id))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "exactly one access_permissions row must exist for this (object, grantee): {rows:?}"
    );
    assert_eq!(
        rows[0].access_type, "editor",
        "the surviving row must carry the racing call's access_type, proving the UPDATE ran"
    );
}

/// `favorites::star_query`'s catch-and-continue duplicate insert must not poison the enclosing
/// transaction. Seeds the conflicting favorite row inside the same `Db::Txn` the handler runs
/// under, calls the handler directly (bypassing HTTP), then proves the transaction is still
/// usable — the failure mode that matters, not just "the call itself returned Ok".
#[tokio::test]
async fn star_query_duplicate_leaves_the_enclosing_txn_usable_under_db_txn() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (user, _group_id) = user_with_role(&app.db, org_id, &["view_query"]).await;

    let ds = new_data_source(org_id).insert(&app.db).await.unwrap();
    let query = new_query(org_id, ds.id, user.id)
        .insert(&app.db)
        .await
        .unwrap();

    let txn = app.db.begin().await.expect("begin failed");
    let arc_txn = Arc::new(txn);

    let now = chrono::Utc::now();
    favorite::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        org_id: Set(org_id),
        object_type: Set("query".to_string()),
        object_id: Set(query.id),
        user_id: Set(user.id),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    }
    .insert(&Db::Txn(arc_txn.clone(), PostCommitJobs::default()))
    .await
    .expect("seed favorite insert failed");

    let db_for_handler = Db::Txn(arc_txn.clone(), PostCommitJobs::default());
    arcodash_backend::api::handlers::favorites::star_query(
        db_for_handler,
        CurrentUser(user.clone()),
        Path(query.id),
    )
    .await
    .expect("starring an already-favorited query must not error");

    // A statement issued through the same underlying transaction after the caught duplicate —
    // proves the SAVEPOINT rollback, not a full transaction abort, is what happened.
    let marker_db = Db::Txn(arc_txn.clone(), PostCommitJobs::default());
    arcodash_core::events::record_event(
        &marker_db,
        org_id,
        Some(user.id),
        "test.marker",
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .expect("transaction must still be usable after the caught duplicate");
    // `marker_db` holds its own clone of `arc_txn` — drop it explicitly so `try_unwrap` below
    // sees a strong count of 1 rather than panicking on a still-live binding.
    drop(marker_db);

    let txn = Arc::try_unwrap(arc_txn).unwrap_or_else(|_| panic!("txn still shared"));
    txn.commit().await.expect("outer commit must succeed");

    let rows = favorite::Entity::find()
        .filter(favorite::Column::UserId.eq(user.id))
        .filter(favorite::Column::ObjectType.eq("query"))
        .filter(favorite::Column::ObjectId.eq(query.id))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "exactly one favorite row must exist despite the duplicate star attempt: {rows:?}"
    );
}

/// Same shape as the query favorite above, but for `dashboard_service::star`'s duplicate-insert
/// catch path.
#[tokio::test]
async fn dashboard_star_duplicate_leaves_the_enclosing_txn_usable_under_db_txn() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (user, _group_id) = user_with_role(&app.db, org_id, &["view_dashboard"]).await;

    let dash = new_dashboard(org_id, user.id)
        .insert(&app.db)
        .await
        .unwrap();

    let txn = app.db.begin().await.expect("begin failed");
    let arc_txn = Arc::new(txn);

    let now = chrono::Utc::now();
    favorite::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        org_id: Set(org_id),
        object_type: Set("dashboard".to_string()),
        object_id: Set(dash.id),
        user_id: Set(user.id),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    }
    .insert(&Db::Txn(arc_txn.clone(), PostCommitJobs::default()))
    .await
    .expect("seed favorite insert failed");

    let db_for_star = Db::Txn(arc_txn.clone(), PostCommitJobs::default());
    dashboard_service::star(&db_for_star, org_id, &user, dash.id)
        .await
        .expect("starring an already-favorited dashboard must not error");
    // `dashboard_service::star` takes `db` by reference, so `db_for_star` (unlike the by-value
    // handler calls elsewhere in this file) survives the call and must be dropped explicitly.
    drop(db_for_star);

    let marker_db = Db::Txn(arc_txn.clone(), PostCommitJobs::default());
    arcodash_core::events::record_event(
        &marker_db,
        org_id,
        Some(user.id),
        "test.marker",
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .expect("transaction must still be usable after the caught duplicate");
    // `marker_db` holds its own clone of `arc_txn` — drop it explicitly so `try_unwrap` below
    // sees a strong count of 1 rather than panicking on a still-live binding.
    drop(marker_db);

    let txn = Arc::try_unwrap(arc_txn).unwrap_or_else(|_| panic!("txn still shared"));
    txn.commit().await.expect("outer commit must succeed");

    let rows = favorite::Entity::find()
        .filter(favorite::Column::UserId.eq(user.id))
        .filter(favorite::Column::ObjectType.eq("dashboard"))
        .filter(favorite::Column::ObjectId.eq(dash.id))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "exactly one favorite row must exist despite the duplicate star attempt: {rows:?}"
    );
}

/// Same shape again, for `groups::assign_data_source`'s duplicate-insert catch path. The seeded
/// row stands in for a prior successful assignment in the same transaction — catching the duplicate
/// without a nested scope would abort the ambient transaction and silently roll that prior row
/// back at commit.
#[tokio::test]
async fn group_data_source_assign_duplicate_leaves_the_enclosing_txn_usable_under_db_txn() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (caller, _group_id) = user_with_role(&app.db, org_id, &["admin"]).await;
    let org = organization::Entity::find_by_id(org_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let ds = new_data_source(org_id).insert(&app.db).await.unwrap();
    let target_group = group_model(org_id, &format!("target-{}", uuid::Uuid::new_v4().simple()))
        .insert(&app.db)
        .await
        .unwrap();

    let txn = app.db.begin().await.expect("begin failed");
    let arc_txn = Arc::new(txn);

    let now = chrono::Utc::now();
    data_source_group::ActiveModel {
        id: NotSet,
        data_source_id: Set(ds.id),
        group_id: Set(target_group.id),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    }
    .insert(&Db::Txn(arc_txn.clone(), PostCommitJobs::default()))
    .await
    .expect("seed data_source_group insert failed");

    let db_for_handler = Db::Txn(arc_txn.clone(), PostCommitJobs::default());
    groups::assign_data_source(
        db_for_handler,
        CurrentOrg(org.clone()),
        CurrentUser(caller.clone()),
        Path(target_group.id),
        ExtractJson(AssignDataSourceBody {
            data_source_id: ds.id,
        }),
    )
    .await
    .expect("assigning an already-assigned data source must not error");

    // A statement issued through the same underlying transaction after the caught duplicate —
    // proves the SAVEPOINT rollback, not a full transaction abort, is what happened.
    let marker_db = Db::Txn(arc_txn.clone(), PostCommitJobs::default());
    arcodash_core::events::record_event(
        &marker_db,
        org_id,
        Some(caller.id),
        "test.marker",
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .expect("transaction must still be usable after the caught duplicate");
    // `marker_db` holds its own clone of `arc_txn` — drop it explicitly so `try_unwrap` below
    // sees a strong count of 1 rather than panicking on a still-live binding.
    drop(marker_db);

    let txn = Arc::try_unwrap(arc_txn).unwrap_or_else(|_| panic!("txn still shared"));
    txn.commit().await.expect("outer commit must succeed");

    let rows = data_source_group::Entity::find()
        .filter(data_source_group::Column::GroupId.eq(target_group.id))
        .filter(data_source_group::Column::DataSourceId.eq(ds.id))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the prior successful assignment must survive the commit despite the duplicate: {rows:?}"
    );
}

/// Same shape as the data-source assignment above, for `groups::add_role`'s duplicate-insert
/// catch path.
#[tokio::test]
async fn group_role_attach_duplicate_leaves_the_enclosing_txn_usable_under_db_txn() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (caller, _group_id) = user_with_role(&app.db, org_id, &["admin"]).await;
    let org = organization::Entity::find_by_id(org_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let target_group = group_model(org_id, &format!("target-{}", uuid::Uuid::new_v4().simple()))
        .insert(&app.db)
        .await
        .unwrap();
    let attached_role = role::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set(format!("role-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec!["view_query".to_string()]),
        r#type: Set("custom".to_string()),
        created_at: NotSet,
        updated_at: NotSet,
    }
    .insert(&app.db)
    .await
    .unwrap();

    let txn = app.db.begin().await.expect("begin failed");
    let arc_txn = Arc::new(txn);

    let now = chrono::Utc::now();
    group_role::ActiveModel {
        id: NotSet,
        group_id: Set(target_group.id),
        role_id: Set(attached_role.id),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    }
    .insert(&Db::Txn(arc_txn.clone(), PostCommitJobs::default()))
    .await
    .expect("seed group_role insert failed");

    let db_for_handler = Db::Txn(arc_txn.clone(), PostCommitJobs::default());
    groups::add_role(
        db_for_handler,
        CurrentOrg(org.clone()),
        CurrentUser(caller.clone()),
        Path(target_group.id),
        ExtractJson(AddRoleBody {
            role_id: attached_role.id,
        }),
    )
    .await
    .expect("attaching an already-attached role must not error");

    let marker_db = Db::Txn(arc_txn.clone(), PostCommitJobs::default());
    arcodash_core::events::record_event(
        &marker_db,
        org_id,
        Some(caller.id),
        "test.marker",
        None,
        None,
        serde_json::json!({}),
    )
    .await
    .expect("transaction must still be usable after the caught duplicate");
    // `marker_db` holds its own clone of `arc_txn` — drop it explicitly so `try_unwrap` below
    // sees a strong count of 1 rather than panicking on a still-live binding.
    drop(marker_db);

    let txn = Arc::try_unwrap(arc_txn).unwrap_or_else(|_| panic!("txn still shared"));
    txn.commit().await.expect("outer commit must succeed");

    let rows = group_role::Entity::find()
        .filter(group_role::Column::GroupId.eq(target_group.id))
        .filter(group_role::Column::RoleId.eq(attached_role.id))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the prior successful attach must survive the commit despite the duplicate: {rows:?}"
    );
}

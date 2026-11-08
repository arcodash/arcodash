//! Integration check that freshly seeded builtin groups default to unrestricted data-source access.

mod common;
use common::{create_throwaway_db, drop_throwaway_db};

use arcodash_core::entities::group;
use arcodash_core::test_support;
use arcodash_migration::m20260812_000001_seed_viewers_group::Migration as SeedViewersGroupMigration;
use sea_orm_migration::sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter,
};
use sea_orm_migration::{MigrationName, MigratorTrait};

#[tokio::test]
async fn seeded_builtin_groups_default_to_unrestricted() {
    let Ok(base_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("[SKIP] TEST_DATABASE_URL not set — skipping org_seed restriction test");
        return;
    };

    let (db, admin_url, db_name) = create_throwaway_db(&base_url, "ds_group_restriction").await;
    arcodash_migration::Migrator::up(&db, None)
        .await
        .expect("apply full migration chain");

    let org = test_support::new_organization().insert(&db).await.unwrap();
    let seeded = arcodash_core::org_seed::seed_builtin_groups(&db, org.id)
        .await
        .expect("seed builtin groups");

    for id in [seeded.admin_group_id, seeded.default_group_id] {
        let g = group::Entity::find_by_id(id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !g.data_source_access_restricted,
            "freshly seeded builtin group {id} must default to unrestricted"
        );
    }

    drop_throwaway_db(&admin_url, &db_name).await;
}

/// An `INSERT` that omits `data_source_access_restricted` entirely must land as restricted, not
/// fall through to org-wide reach.
#[tokio::test]
async fn a_group_inserted_without_the_column_defaults_to_restricted() {
    let Ok(base_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("[SKIP] TEST_DATABASE_URL not set — skipping org_seed restriction test");
        return;
    };

    let (db, admin_url, db_name) = create_throwaway_db(&base_url, "ds_group_restriction").await;
    arcodash_migration::Migrator::up(&db, None)
        .await
        .expect("apply full migration chain");

    let org = test_support::new_organization().insert(&db).await.unwrap();

    db.execute_unprepared(&format!(
        "INSERT INTO groups (org_id, name, permissions, type) \
         VALUES ({}, 'no-column-group', ARRAY[]::text[], 'regular')",
        org.id
    ))
    .await
    .expect("insert group without naming data_source_access_restricted");

    let g = group::Entity::find()
        .filter(group::Column::OrgId.eq(org.id))
        .filter(group::Column::Name.eq("no-column-group"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        g.data_source_access_restricted,
        "a group inserted without naming the column must default to restricted"
    );

    drop_throwaway_db(&admin_url, &db_name).await;
}

/// The `viewers` backfill migration (`m20260812_000001`) never names the column and runs before
/// the default is flipped to restricted, so its rows must keep their unrestricted value
/// regardless of what the column's default is later set to.
#[tokio::test]
async fn the_backfilled_viewers_group_is_unrestricted() {
    let Ok(base_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("[SKIP] TEST_DATABASE_URL not set — skipping org_seed restriction test");
        return;
    };

    let (db, admin_url, db_name) = create_throwaway_db(&base_url, "ds_group_restriction").await;

    // Run only up through the migration immediately before the backfill, so the org and its
    // builtin Viewer role exist in the same state an existing install would have been in when
    // m20260812_000001 first ran on it.
    let seed_viewers_index = arcodash_migration::Migrator::migrations()
        .iter()
        .position(|m| m.name() == SeedViewersGroupMigration.name())
        .expect("m20260812_000001_seed_viewers_group must be registered");
    arcodash_migration::Migrator::up(&db, Some(seed_viewers_index as u32))
        .await
        .expect("apply migrations up to (not including) the viewers backfill");

    let org = test_support::new_organization().insert(&db).await.unwrap();
    db.execute_unprepared(&format!(
        "INSERT INTO roles (org_id, name, permissions, type) \
         VALUES ({}, 'Viewer', ARRAY[]::text[], 'builtin')",
        org.id
    ))
    .await
    .expect("insert builtin Viewer role");

    // Now apply the backfill itself — at this point in the chain the column's default is
    // still the original one.
    arcodash_migration::Migrator::up(&db, Some(1))
        .await
        .expect("apply the viewers backfill migration");

    // Finish the chain, including the default flip, and confirm the already-written
    // row was left alone rather than picking up the new default retroactively.
    arcodash_migration::Migrator::up(&db, None)
        .await
        .expect("apply the remaining migration chain");

    let viewers = group::Entity::find()
        .filter(group::Column::OrgId.eq(org.id))
        .filter(group::Column::Name.eq("viewers"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !viewers.data_source_access_restricted,
        "the backfilled viewers group must stay unrestricted regardless of the column's current default"
    );

    drop_throwaway_db(&admin_url, &db_name).await;
}

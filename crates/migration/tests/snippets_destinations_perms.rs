//! Invariant test for `m20260722_000002_snippets_destinations_perms`: seeds a pre-migration
//! builtin Editor role (missing `view_snippets`/`manage_snippets`/`view_destinations`) and a
//! pre-migration builtin Viewer role (missing `view_snippets`/`view_destinations`), plus a
//! same-named custom role and the builtin Admin role, runs the migration, and asserts only the
//! builtin Editor/Viewer roles gain the intended keys — everything else is untouched, and a
//! second run doesn't duplicate anything. Mirrors the throwaway-DB pattern in
//! `seed_viewers_group.rs`.

mod common;
use common::{create_throwaway_db, drop_throwaway_db};

use arcodash_core::entities::{organization, role};
use sea_orm_migration::sea_orm::{ActiveValue::Set, DatabaseConnection, EntityTrait};
use sea_orm_migration::MigratorTrait;
use std::collections::HashSet;

const TARGET_MIGRATION: &str = "m20260722_000002_snippets_destinations_perms";

fn as_set(v: &[String]) -> HashSet<&str> {
    v.iter().map(|s| s.as_str()).collect()
}
fn as_set_static<'a>(v: &'a [&'a str]) -> HashSet<&'a str> {
    v.iter().copied().collect()
}

#[tokio::test]
async fn migration_grants_snippet_and_destination_view_keys_to_builtin_roles_only() {
    let Ok(base_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!(
            "[SKIP] TEST_DATABASE_URL not set — skipping snippets/destinations perms migration test"
        );
        return;
    };

    let (db, admin_url, db_name) =
        create_throwaway_db(&base_url, "snippets_destinations_perms").await;
    let result = run(&db).await;
    drop_throwaway_db(&admin_url, &db_name).await;
    result.unwrap();
}

async fn run(db: &DatabaseConnection) -> Result<(), Box<dyn std::error::Error>> {
    // Apply every migration strictly before the target — the builtin roles seeded here simulate
    // an existing org's roles as they looked immediately before this migration, i.e. without the
    // new snippet/destination keys.
    let before = arcodash_migration::Migrator::migrations()
        .iter()
        .take_while(|m| m.name() != TARGET_MIGRATION)
        .count();
    arcodash_migration::Migrator::up(db, Some(before as u32)).await?;

    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let org_id = organization::Entity::insert(organization::ActiveModel {
        name: Set("Snippets Destinations Perms Org".to_string()),
        slug: Set(format!("snippets-destinations-perms-{suffix}")),
        settings: Set(serde_json::json!({})),
        ..Default::default()
    })
    .exec(db)
    .await?
    .last_insert_id;
    // A second org for the same-named custom role: `roles` has a unique (org_id, name) index, so
    // a custom "Editor" can't coexist with the builtin "Editor" in the same org — using a separate
    // org is the only way to exercise the `type = 'builtin'` qualifier at all.
    let other_org_id = organization::Entity::insert(organization::ActiveModel {
        name: Set("Snippets Destinations Perms Org 2".to_string()),
        slug: Set(format!("snippets-destinations-perms-2-{suffix}")),
        settings: Set(serde_json::json!({})),
        ..Default::default()
    })
    .exec(db)
    .await?
    .last_insert_id;

    let now: sea_orm_migration::sea_orm::prelude::DateTimeWithTimeZone = chrono::Utc::now().into();

    async fn insert_role(
        db: &DatabaseConnection,
        org_id: i64,
        name: &str,
        r#type: &str,
        perms: Vec<&str>,
        now: sea_orm_migration::sea_orm::prelude::DateTimeWithTimeZone,
    ) -> Result<i64, sea_orm_migration::sea_orm::DbErr> {
        Ok(role::Entity::insert(role::ActiveModel {
            org_id: Set(org_id),
            name: Set(name.to_string()),
            permissions: Set(perms.into_iter().map(str::to_string).collect()),
            r#type: Set(r#type.to_string()),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await?
        .last_insert_id)
    }

    // Pre-migration builtin Editor: no snippet/destination keys yet.
    const PRE_MIGRATION_EDITOR_PERMS: &[&str] = &[
        "create_dashboard",
        "edit_dashboard",
        "view_dashboard",
        "create_query",
        "edit_query",
        "view_query",
        "execute_query",
        "schedule_query",
        "view_data_source",
        "view_monitor",
        "create_monitor",
        "edit_monitor",
        "view_users",
        "create_embed",
    ];
    let builtin_editor_id = insert_role(
        db,
        org_id,
        "Editor",
        "builtin",
        PRE_MIGRATION_EDITOR_PERMS.to_vec(),
        now,
    )
    .await?;

    // A CUSTOM role also named "Editor" (different org — `roles` has a unique (org_id, name)
    // index) — must not be touched, since only `type = 'builtin'` qualifies.
    let custom_editor_id = insert_role(
        db,
        other_org_id,
        "Editor",
        "custom",
        vec!["view_dashboard"],
        now,
    )
    .await?;

    // Pre-migration builtin Viewer: no snippet/destination keys yet.
    const PRE_MIGRATION_VIEWER_PERMS: &[&str] = &["view_dashboard", "view_query", "view_monitor"];
    let builtin_viewer_id = insert_role(
        db,
        org_id,
        "Viewer",
        "builtin",
        PRE_MIGRATION_VIEWER_PERMS.to_vec(),
        now,
    )
    .await?;

    // Builtin Admin — must not gain anything (implies-all via the `admin` key already).
    let builtin_admin_id = insert_role(db, org_id, "Admin", "builtin", vec!["admin"], now).await?;

    // Apply the target migration (and anything after it).
    arcodash_migration::Migrator::up(db, None).await?;

    async fn perms(db: &DatabaseConnection, role_id: i64) -> Vec<String> {
        role::Entity::find_by_id(role_id)
            .one(db)
            .await
            .expect("role query")
            .expect("role must still exist")
            .permissions
    }

    // Builtin Editor gained exactly the three new keys, on top of its prior permissions.
    let editor_perms = perms(db, builtin_editor_id).await;
    let mut expected_editor: Vec<&str> = PRE_MIGRATION_EDITOR_PERMS.to_vec();
    expected_editor.push("view_snippets");
    expected_editor.push("manage_snippets");
    expected_editor.push("view_destinations");
    assert_eq!(as_set(&editor_perms), as_set_static(&expected_editor));

    // Builtin Viewer gained exactly the two view keys.
    let viewer_perms = perms(db, builtin_viewer_id).await;
    let mut expected_viewer: Vec<&str> = PRE_MIGRATION_VIEWER_PERMS.to_vec();
    expected_viewer.push("view_snippets");
    expected_viewer.push("view_destinations");
    assert_eq!(as_set(&viewer_perms), as_set_static(&expected_viewer));

    // Custom role named "Editor" is untouched.
    assert_eq!(
        as_set(&perms(db, custom_editor_id).await),
        as_set_static(&["view_dashboard"])
    );

    // Builtin Admin untouched.
    assert_eq!(
        as_set(&perms(db, builtin_admin_id).await),
        as_set_static(&["admin"])
    );

    // Idempotency: re-running the whole migrator makes no further changes (no duplicate entries).
    arcodash_migration::Migrator::up(db, None).await?;
    let editor_perms_after_rerun = perms(db, builtin_editor_id).await;
    assert_eq!(
        as_set(&editor_perms_after_rerun),
        as_set_static(&expected_editor)
    );
    assert_eq!(
        editor_perms_after_rerun
            .iter()
            .filter(|p| p.as_str() == "view_snippets")
            .count(),
        1,
        "re-running must not duplicate view_snippets"
    );

    let viewer_perms_after_rerun = perms(db, builtin_viewer_id).await;
    assert_eq!(
        as_set(&viewer_perms_after_rerun),
        as_set_static(&expected_viewer)
    );
    assert_eq!(
        viewer_perms_after_rerun
            .iter()
            .filter(|p| p.as_str() == "view_destinations")
            .count(),
        1,
        "re-running must not duplicate view_destinations"
    );

    Ok(())
}

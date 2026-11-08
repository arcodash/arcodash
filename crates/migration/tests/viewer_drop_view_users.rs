//! Invariant test for `m20260809_000001_viewer_drop_view_users`: seeds a pre-migration builtin
//! Viewer role (carrying `view_users`), plus a same-named custom role and the builtin Editor
//! role, runs the migration, and asserts only the builtin Viewer role loses `view_users` —
//! everything else is untouched, and a second run doesn't change anything further. Mirrors the
//! throwaway-DB pattern in `snippets_destinations_perms.rs`.

mod common;
use common::{create_throwaway_db, drop_throwaway_db};

use arcodash_core::entities::{organization, role};
use sea_orm_migration::sea_orm::{ActiveValue::Set, DatabaseConnection, EntityTrait};
use sea_orm_migration::MigratorTrait;
use std::collections::HashSet;

const TARGET_MIGRATION: &str = "m20260809_000001_viewer_drop_view_users";

fn as_set(v: &[String]) -> HashSet<&str> {
    v.iter().map(|s| s.as_str()).collect()
}
fn as_set_static<'a>(v: &'a [&'a str]) -> HashSet<&'a str> {
    v.iter().copied().collect()
}

#[tokio::test]
async fn migration_drops_view_users_from_builtin_viewer_only() {
    let Ok(base_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!(
            "[SKIP] TEST_DATABASE_URL not set — skipping viewer-drop-view_users migration test"
        );
        return;
    };

    let (db, admin_url, db_name) = create_throwaway_db(&base_url, "viewer_drop_view_users").await;
    let result = run(&db).await;
    drop_throwaway_db(&admin_url, &db_name).await;
    result.unwrap();
}

async fn run(db: &DatabaseConnection) -> Result<(), Box<dyn std::error::Error>> {
    // Apply every migration strictly before the target — the builtin roles seeded here simulate
    // an existing org's roles as they looked immediately before this migration, i.e. still
    // carrying view_users on Viewer.
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
        name: Set("Viewer Drop View Users Org".to_string()),
        slug: Set(format!("viewer-drop-view-users-{suffix}")),
        settings: Set(serde_json::json!({})),
        ..Default::default()
    })
    .exec(db)
    .await?
    .last_insert_id;
    // A second org for the same-named custom role: `roles` has a unique (org_id, name) index, so
    // a custom "Viewer" can't coexist with the builtin "Viewer" in the same org.
    let other_org_id = organization::Entity::insert(organization::ActiveModel {
        name: Set("Viewer Drop View Users Org 2".to_string()),
        slug: Set(format!("viewer-drop-view-users-2-{suffix}")),
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

    // Pre-migration builtin Viewer: still carries view_users.
    const PRE_MIGRATION_VIEWER_PERMS: &[&str] = &[
        "view_dashboard",
        "view_query",
        "view_data_source",
        "view_monitor",
        "view_destinations",
        "view_users",
        "view_snippets",
    ];
    let builtin_viewer_id = insert_role(
        db,
        org_id,
        "Viewer",
        "builtin",
        PRE_MIGRATION_VIEWER_PERMS.to_vec(),
        now,
    )
    .await?;

    // A CUSTOM role also named "Viewer" (different org — `roles` has a unique (org_id, name)
    // index) — must not be touched, since only `type = 'builtin'` qualifies.
    let custom_viewer_id = insert_role(
        db,
        other_org_id,
        "Viewer",
        "custom",
        vec!["view_dashboard", "view_users"],
        now,
    )
    .await?;

    // Builtin Editor — must not be touched by this migration.
    const EDITOR_PERMS: &[&str] = &["view_dashboard", "create_query", "view_users"];
    let builtin_editor_id =
        insert_role(db, org_id, "Editor", "builtin", EDITOR_PERMS.to_vec(), now).await?;

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

    // Builtin Viewer lost exactly view_users, kept everything else.
    let viewer_perms = perms(db, builtin_viewer_id).await;
    let expected_viewer: Vec<&str> = PRE_MIGRATION_VIEWER_PERMS
        .iter()
        .copied()
        .filter(|p| *p != "view_users")
        .collect();
    assert_eq!(as_set(&viewer_perms), as_set_static(&expected_viewer));
    assert!(!viewer_perms.iter().any(|p| p == "view_users"));

    // Custom role named "Viewer" is untouched — still has view_users.
    assert_eq!(
        as_set(&perms(db, custom_viewer_id).await),
        as_set_static(&["view_dashboard", "view_users"])
    );

    // Builtin Editor untouched.
    assert_eq!(
        as_set(&perms(db, builtin_editor_id).await),
        as_set_static(EDITOR_PERMS)
    );

    // Idempotency: re-running the whole migrator makes no further changes.
    arcodash_migration::Migrator::up(db, None).await?;
    let viewer_perms_after_rerun = perms(db, builtin_viewer_id).await;
    assert_eq!(
        as_set(&viewer_perms_after_rerun),
        as_set_static(&expected_viewer)
    );
    assert!(!viewer_perms_after_rerun.iter().any(|p| p == "view_users"));

    Ok(())
}

//! Invariant test for `m20260812_000001_seed_viewers_group`: backfills a builtin `viewers`
//! group linked to the builtin Viewer role into every org that has one, without touching
//! `users.groups` or any existing `group_roles` row, and without duplicating a pre-existing
//! custom group of the same name. Mirrors the throwaway-DB pattern in
//! `viewer_drop_view_users.rs`.

mod common;
use common::{create_throwaway_db, drop_throwaway_db};

use arcodash_core::entities::{group, group_role, organization, role, user};
use sea_orm_migration::sea_orm::{
    ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
};
use sea_orm_migration::MigratorTrait;
use std::collections::HashSet;

const TARGET_MIGRATION: &str = "m20260812_000001_seed_viewers_group";

fn as_set(v: &[String]) -> HashSet<&str> {
    v.iter().map(|s| s.as_str()).collect()
}

async fn insert_org(db: &DatabaseConnection, slug: &str) -> i64 {
    organization::Entity::insert(organization::ActiveModel {
        name: Set(slug.to_string()),
        slug: Set(slug.to_string()),
        settings: Set(serde_json::json!({})),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("insert organization")
    .last_insert_id
}

async fn insert_role(db: &DatabaseConnection, org_id: i64, name: &str, r#type: &str) -> i64 {
    role::Entity::insert(role::ActiveModel {
        org_id: Set(org_id),
        name: Set(name.to_string()),
        permissions: Set(vec!["view_dashboard".to_string(), "view_query".to_string()]),
        r#type: Set(r#type.to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("insert role")
    .last_insert_id
}

async fn insert_group(
    db: &DatabaseConnection,
    org_id: i64,
    name: &str,
    r#type: &str,
    perms: Vec<&str>,
) -> i64 {
    group::Entity::insert(group::ActiveModel {
        org_id: Set(org_id),
        name: Set(name.to_string()),
        permissions: Set(perms.into_iter().map(str::to_string).collect()),
        r#type: Set(r#type.to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("insert group")
    .last_insert_id
}

async fn link(db: &DatabaseConnection, group_id: i64, role_id: i64) {
    group_role::Entity::insert(group_role::ActiveModel {
        group_id: Set(group_id),
        role_id: Set(role_id),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("insert group_role link");
}

async fn viewers_group(db: &DatabaseConnection, org_id: i64) -> Option<group::Model> {
    group::Entity::find()
        .filter(group::Column::OrgId.eq(org_id))
        .filter(group::Column::Name.eq("viewers"))
        .one(db)
        .await
        .expect("group query failed")
}

async fn group_role_links(db: &DatabaseConnection, group_id: i64) -> Vec<group_role::Model> {
    group_role::Entity::find()
        .filter(group_role::Column::GroupId.eq(group_id))
        .all(db)
        .await
        .expect("group_role query failed")
}

#[tokio::test]
async fn seeds_viewers_group_without_disturbing_existing_state() {
    let Ok(base_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("[SKIP] TEST_DATABASE_URL not set — skipping seed_viewers_group migration test");
        return;
    };

    let (db, admin_url, db_name) = create_throwaway_db(&base_url, "seed_viewers_group").await;
    let result = run(&db).await;
    drop_throwaway_db(&admin_url, &db_name).await;
    result.unwrap();
}

async fn run(db: &DatabaseConnection) -> Result<(), Box<dyn std::error::Error>> {
    // Apply every migration strictly before the target, so the fixtures below simulate orgs as
    // they looked immediately before this migration ran.
    let before = arcodash_migration::Migrator::migrations()
        .iter()
        .take_while(|m| m.name() != TARGET_MIGRATION)
        .count();
    arcodash_migration::Migrator::up(db, Some(before as u32)).await?;

    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    // ── Org A: a plain existing org with a user parked in the builtin default group ────
    let org_a = insert_org(db, &format!("seed-viewers-a-{suffix}")).await;
    let admin_role_a = insert_role(db, org_a, "Admin", "builtin").await;
    let editor_role_a = insert_role(db, org_a, "Editor", "builtin").await;
    let viewer_role_a = insert_role(db, org_a, "Viewer", "builtin").await;
    let admin_group_a = insert_group(db, org_a, "admin", "builtin", vec!["admin"]).await;
    let default_group_a =
        insert_group(db, org_a, "default", "builtin", vec!["view_dashboard"]).await;
    link(db, admin_group_a, admin_role_a).await;
    link(db, default_group_a, editor_role_a).await;

    let user_id = user::Entity::insert(user::ActiveModel {
        org_id: Set(org_a),
        name: Set("Org A User".to_string()),
        email: Set(format!("seed-viewers-a-user-{suffix}@example.com")),
        password_hash: Set(None),
        groups: Set(vec![default_group_a]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(&format!(
            "seed-viewers-a-{suffix}"
        ))),
        disabled_at: Set(None),
        is_invitation_pending: Set(false),
        profile_image_url: Set(None),
        details: Set(serde_json::json!({})),
        digest_enabled: Set(false),
        digest_unsubscribe_token: Set(None),
        ..Default::default()
    })
    .exec(db)
    .await?
    .last_insert_id;

    // ── Org B: already has a custom (non-builtin) group named "viewers" ────────────────
    let org_b = insert_org(db, &format!("seed-viewers-b-{suffix}")).await;
    insert_role(db, org_b, "Viewer", "builtin").await;
    let unrelated_role_b = insert_role(db, org_b, "Unrelated", "custom").await;
    let custom_viewers_b = insert_group(
        db,
        org_b,
        "viewers",
        "custom",
        vec!["view_dashboard", "custom_marker"],
    )
    .await;
    link(db, custom_viewers_b, unrelated_role_b).await;

    // ── Org C: no builtin Viewer role at all ────────────────────────────────────────────
    let org_c = insert_org(db, &format!("seed-viewers-c-{suffix}")).await;
    insert_role(db, org_c, "Admin", "builtin").await;

    // Apply the target migration (and anything after it).
    arcodash_migration::Migrator::up(db, None).await?;

    // Org A: viewers group created and linked to the builtin Viewer role.
    let viewers_a = viewers_group(db, org_a)
        .await
        .expect("org A must get a viewers group");
    assert_eq!(viewers_a.r#type, "builtin");
    let links_a = group_role_links(db, viewers_a.id).await;
    assert_eq!(
        links_a.len(),
        1,
        "viewers group must link to exactly one role"
    );
    assert_eq!(links_a[0].role_id, viewer_role_a);

    // Org A: default->Editor link untouched.
    let default_links_a = group_role_links(db, default_group_a).await;
    assert_eq!(default_links_a.len(), 1);
    assert_eq!(default_links_a[0].role_id, editor_role_a);

    // Org A: the user's groups array is byte-identical — nobody demoted or promoted.
    let user_after = user::Entity::find_by_id(user_id)
        .one(db)
        .await?
        .expect("user must still exist");
    assert_eq!(
        user_after.groups,
        vec![default_group_a],
        "seeding the viewers group must never touch users.groups"
    );

    // Org B: the pre-existing custom "viewers" group is untouched, no duplicate created.
    let all_viewers_b: Vec<group::Model> = group::Entity::find()
        .filter(group::Column::OrgId.eq(org_b))
        .filter(group::Column::Name.eq("viewers"))
        .all(db)
        .await?;
    assert_eq!(
        all_viewers_b.len(),
        1,
        "must not duplicate a pre-existing custom 'viewers' group"
    );
    assert_eq!(all_viewers_b[0].id, custom_viewers_b);
    assert_eq!(all_viewers_b[0].r#type, "custom");
    assert_eq!(
        as_set(&all_viewers_b[0].permissions),
        ["view_dashboard", "custom_marker"].into_iter().collect()
    );
    let links_b = group_role_links(db, custom_viewers_b).await;
    assert_eq!(links_b.len(), 1);
    assert_eq!(links_b[0].role_id, unrelated_role_b);

    // Org C: no Viewer role -> no viewers group, no error.
    assert!(
        viewers_group(db, org_c).await.is_none(),
        "an org with no builtin Viewer role must not get a viewers group"
    );

    // Idempotency: re-running the whole migrator makes no further changes.
    let group_count_before = group::Entity::find().all(db).await?.len();
    let link_count_before = group_role::Entity::find().all(db).await?.len();
    arcodash_migration::Migrator::up(db, None).await?;
    let group_count_after = group::Entity::find().all(db).await?.len();
    let link_count_after = group_role::Entity::find().all(db).await?.len();
    assert_eq!(group_count_before, group_count_after);
    assert_eq!(link_count_before, link_count_after);

    Ok(())
}

//! Integration tests: `bootstrap::bootstrap_admin_user`'s admin-email lookup.

mod common;
use common::{spawn_app, ADMIN_EMAIL, ADMIN_PASSWORD};

/// `spawn_app` seeds an admin user at `ADMIN_EMAIL` (`admin@test.local`). Bootstrapping again
/// with a differently-cased env var must find that row via the case-insensitive lookup and
/// return `Ok(())` without inserting a second one — an exact-match lookup would miss it, attempt
/// an INSERT, and fail on the case-insensitive unique index instead.
#[tokio::test]
async fn bootstrap_finds_an_existing_admin_by_case_insensitive_email_match() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    std::env::set_var("ADMIN_EMAIL", "Admin@Test.Local");
    std::env::set_var("ADMIN_PASSWORD", ADMIN_PASSWORD);
    let result = arcodash_backend::bootstrap::bootstrap_admin_user(&app.db).await;
    std::env::remove_var("ADMIN_EMAIL");
    std::env::remove_var("ADMIN_PASSWORD");

    assert!(result.is_ok(), "bootstrap failed: {:?}", result.err());

    use arcodash_core::entities::user;
    use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
    let count = user::Entity::find()
        .filter(user::Column::Email.eq(ADMIN_EMAIL))
        .count(&app.db)
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "case-insensitive match must not create a duplicate admin row"
    );
}

/// Two replicas booting simultaneously both pass the "does this admin exist yet" check and race
/// to insert the same row — `bootstrap_admin_user` must recognize the loser's unique-violation as
/// "created concurrently" and return `Ok(())`, not propagate a startup-aborting error.
#[tokio::test]
async fn concurrent_bootstrap_calls_create_exactly_one_admin() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let email = format!("race-admin-{}@test.local", uuid::Uuid::new_v4().simple());
    std::env::set_var("ADMIN_EMAIL", &email);
    std::env::set_var("ADMIN_PASSWORD", ADMIN_PASSWORD);

    let db_a = app.db.clone();
    let db_b = app.db.clone();
    let (result_a, result_b) = tokio::join!(
        arcodash_backend::bootstrap::bootstrap_admin_user(&db_a),
        arcodash_backend::bootstrap::bootstrap_admin_user(&db_b),
    );

    std::env::remove_var("ADMIN_EMAIL");
    std::env::remove_var("ADMIN_PASSWORD");

    assert!(result_a.is_ok(), "{:?}", result_a.err());
    assert!(result_b.is_ok(), "{:?}", result_b.err());

    use arcodash_core::entities::user;
    use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
    let count = user::Entity::find()
        .filter(user::Column::Email.eq(&email))
        .count(&app.db)
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "concurrent bootstrap calls racing on the same email must create exactly one admin row"
    );
}

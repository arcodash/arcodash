//! Integration tests: SSO identity binding is case-insensitive on email, disabled-aware, and
//! matches the `(org_id, lower(email))` unique index. Shared by every SSO provider via
//! `find_or_create_sso_user[_with_groups]`.

mod common;
use common::{make_org, spawn_app, user_with_role};

use arcodash_backend::services::user_service::{
    find_or_create_sso_user, find_or_create_sso_user_with_groups, forgot_password, invite,
};
use arcodash_core::entities::user;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
};

async fn seed_user(db: &impl sea_orm::ConnectionTrait, org_id: i64, email: &str) -> user::Model {
    seed_user_pending(db, org_id, email, false).await
}

async fn seed_user_pending(
    db: &impl sea_orm::ConnectionTrait,
    org_id: i64,
    email: &str,
    is_invitation_pending: bool,
) -> user::Model {
    use sea_orm::ActiveValue::NotSet;

    let id = user::Entity::insert(user::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set("Existing User".to_string()),
        email: Set(email.to_string()),
        password_hash: Set(None),
        groups: Set(vec![]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(
            &uuid::Uuid::new_v4().simple().to_string(),
        )),
        disabled_at: Set(None),
        is_invitation_pending: Set(is_invitation_pending),
        profile_image_url: Set(None),
        details: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
        digest_enabled: Set(false),
        digest_unsubscribe_token: Set(None),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("user insert failed")
    .last_insert_id;

    user::Entity::find_by_id(id)
        .one(db)
        .await
        .expect("user query failed")
        .expect("user must exist after insert")
}

async fn count_users_with_email_ci(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    email: &str,
) -> usize {
    user::Entity::find()
        .filter(user::Column::OrgId.eq(org_id))
        .all(db)
        .await
        .expect("user query failed")
        .into_iter()
        .filter(|u| u.email.eq_ignore_ascii_case(email))
        .count()
}

/// A case-different email on an existing active user matches the same row instead of creating a
/// second.
#[tokio::test]
async fn case_insensitive_match_returns_same_user() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;

    let existing = seed_user(&app.db, org_id, "User@X.com").await;

    let found = find_or_create_sso_user(&app.db, org_id, "user@x.com", "n", None)
        .await
        .expect("SSO login should succeed for an active user");

    assert_eq!(found.id, existing.id, "must resolve to the same user row");
    assert_eq!(
        count_users_with_email_ci(&app.db, org_id, "user@x.com").await,
        1,
        "no second row should be created"
    );
}

/// A disabled user is found (case-insensitively) but the login is rejected — not reactivated,
/// and no new row is created for them.
#[tokio::test]
async fn disabled_user_is_rejected_not_reactivated() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;

    let existing = seed_user(&app.db, org_id, "user@x.com").await;
    let mut active: user::ActiveModel = existing.clone().into();
    active.disabled_at = Set(Some(chrono::Utc::now().into()));
    active.update(&app.db).await.expect("disable update failed");

    let result = find_or_create_sso_user(&app.db, org_id, "USER@X.com", "n", None).await;

    assert!(
        result.is_err(),
        "SSO login for a disabled account must be rejected"
    );
    let err = result.unwrap_err();
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);

    assert_eq!(
        count_users_with_email_ci(&app.db, org_id, "user@x.com").await,
        1,
        "rejected login must not create a new row"
    );

    let reloaded = user::Entity::find_by_id(existing.id)
        .one(&app.db)
        .await
        .expect("user query failed")
        .expect("user must still exist");
    assert!(
        reloaded.disabled_at.is_some(),
        "rejected login must not reactivate the account"
    );
}

/// An admin-invited user (`is_invitation_pending = true`) who logs in via SSO must have the pending
/// flag cleared, or the session cookie is immediately rejected by the pending-invite filter.
#[tokio::test]
async fn sso_login_clears_pending_invitation() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;

    let existing = seed_user_pending(&app.db, org_id, "invited@x.com", true).await;
    assert!(existing.is_invitation_pending);

    let found = find_or_create_sso_user(&app.db, org_id, "invited@x.com", "Some Name", None)
        .await
        .expect("SSO login should succeed for an invited user");

    assert!(
        !found.is_invitation_pending,
        "SSO login must clear is_invitation_pending"
    );
}

/// `invite()` matches an existing email case-insensitively like SSO: a second invite with different
/// casing must be rejected.
#[tokio::test]
async fn invite_rejects_existing_email_case_insensitively() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (caller, _role_id) = user_with_role(&app.db, org_id, &["manage_users"]).await;

    seed_user(&app.db, org_id, "Admin@Corp.com").await;

    let result = invite(
        &app.db,
        org_id,
        &caller,
        "Someone Else".to_string(),
        "admin@corp.com".to_string(),
        vec![],
    )
    .await;

    assert!(
        result.is_err(),
        "invite() must reject a case-different collision with an existing active user"
    );
    assert_eq!(
        count_users_with_email_ci(&app.db, org_id, "admin@corp.com").await,
        1,
        "no duplicate row should be created"
    );
}

/// A brand-new plain-SSO user (no IdP group data) lands in the read-only builtin "viewers" group,
/// not the Editor-carrying "default" group.
#[tokio::test]
async fn new_plain_sso_user_lands_in_viewers() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let seeded = arcodash_core::org_seed::seed_builtin_groups(&app.db, org_id)
        .await
        .expect("seed_builtin_groups failed");

    let found = find_or_create_sso_user(&app.db, org_id, "fresh@x.com", "Fresh User", None)
        .await
        .expect("SSO login should provision a new user");

    assert_eq!(
        found.groups,
        vec![seeded.viewer_group_id],
        "a first-login plain-SSO user must land in the read-only viewers group"
    );
}

/// A new user whose IdP group names match nothing also falls back to "viewers".
#[tokio::test]
async fn new_sso_user_with_unmatched_idp_groups_lands_in_viewers() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let seeded = arcodash_core::org_seed::seed_builtin_groups(&app.db, org_id)
        .await
        .expect("seed_builtin_groups failed");

    let found = find_or_create_sso_user_with_groups(
        &app.db,
        org_id,
        "mismatched@x.com",
        "Mismatched User",
        None,
        Some(vec!["NoSuchIdpGroup".to_string()]),
    )
    .await
    .expect("SSO login should provision a new user despite an unmatched group name");

    assert_eq!(
        found.groups,
        vec![seeded.viewer_group_id],
        "an unmatched IdP group name on first login must fall back to viewers"
    );
}

/// A brand-new user whose IdP-supplied group name matches a non-admin org group is placed there
/// directly — an explicit mapping wins over the read-only fallback.
#[tokio::test]
async fn new_sso_user_with_matched_idp_group_bypasses_viewers_fallback() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let seeded = arcodash_core::org_seed::seed_builtin_groups(&app.db, org_id)
        .await
        .expect("seed_builtin_groups failed");

    let found = find_or_create_sso_user_with_groups(
        &app.db,
        org_id,
        "mapped@x.com",
        "Mapped User",
        None,
        Some(vec!["default".to_string()]),
    )
    .await
    .expect("SSO login should provision a new user via the matched mapping");

    assert_eq!(
        found.groups,
        vec![seeded.default_group_id],
        "a matched, non-admin IdP group name must win over the viewers fallback"
    );
}

/// An IdP group name matching the org's `admin` group must NOT confer admin on a brand-new user (a
/// directory group named "admin" isn't trustworthy); the resolver drops it and falls back.
#[tokio::test]
async fn new_sso_user_with_idp_group_named_admin_does_not_become_admin() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let seeded = arcodash_core::org_seed::seed_builtin_groups(&app.db, org_id)
        .await
        .expect("seed_builtin_groups failed");

    let found = find_or_create_sso_user_with_groups(
        &app.db,
        org_id,
        "wouldbeadmin@x.com",
        "Would Be Admin",
        None,
        Some(vec!["admin".to_string()]),
    )
    .await
    .expect("SSO login should provision a new user despite the admin-named group being dropped");

    assert_eq!(
        found.groups,
        vec![seeded.viewer_group_id],
        "an IdP group literally named admin must never become the new user's admin group"
    );
}

/// An existing NON-ADMIN user's re-sync on an unmatched IdP group still falls back to "default";
/// only admin-conferring membership is never revoked.
#[tokio::test]
async fn existing_sso_user_resync_with_unmatched_idp_groups_still_falls_back_to_default() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let seeded = arcodash_core::org_seed::seed_builtin_groups(&app.db, org_id)
        .await
        .expect("seed_builtin_groups failed");

    let existing = seed_user(&app.db, org_id, "resync@x.com").await;
    let mut active: user::ActiveModel = existing.into();
    active.groups = Set(vec![seeded.viewer_group_id]);
    active.update(&app.db).await.expect("group backfill failed");

    let found = find_or_create_sso_user_with_groups(
        &app.db,
        org_id,
        "resync@x.com",
        "Resync User",
        None,
        Some(vec!["NoSuchIdpGroup".to_string()]),
    )
    .await
    .expect("SSO re-login should succeed for an existing user");

    assert_eq!(
        found.groups,
        vec![seeded.default_group_id],
        "a non-admin existing user's unmatched-group resync must still fall back to default"
    );
}

/// An existing ADMIN's group re-sync must never revoke admin membership the IdP omits (the IdP only
/// asserts non-admin membership); it rides along unioned with the non-admin fallback.
#[tokio::test]
async fn existing_admin_keeps_admin_when_idp_assertion_omits_the_admin_group() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let seeded = arcodash_core::org_seed::seed_builtin_groups(&app.db, org_id)
        .await
        .expect("seed_builtin_groups failed");

    let existing = seed_user(&app.db, org_id, "stillsadmin@x.com").await;
    let mut active: user::ActiveModel = existing.into();
    active.groups = Set(vec![seeded.admin_group_id]);
    active.update(&app.db).await.expect("group backfill failed");

    let found = find_or_create_sso_user_with_groups(
        &app.db,
        org_id,
        "stillsadmin@x.com",
        "Still Admin",
        None,
        Some(vec!["NoSuchIdpGroup".to_string()]),
    )
    .await
    .expect("SSO re-login should succeed for an existing admin");

    assert!(
        found.groups.contains(&seeded.admin_group_id),
        "an existing admin's membership must survive a resync that never mentions admin: {:?}",
        found.groups
    );
}

/// A group re-sync that actually changes an existing user's groups writes a
/// `user.groups_synced` event with the before/after id sets.
#[tokio::test]
async fn group_resync_change_records_a_groups_synced_event() {
    use arcodash_core::entities::event;

    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let seeded = arcodash_core::org_seed::seed_builtin_groups(&app.db, org_id)
        .await
        .expect("seed_builtin_groups failed");

    let existing = seed_user(&app.db, org_id, "eventresync@x.com").await;
    let mut active: user::ActiveModel = existing.clone().into();
    active.groups = Set(vec![seeded.viewer_group_id]);
    active.update(&app.db).await.expect("group backfill failed");

    let synced = find_or_create_sso_user_with_groups(
        &app.db,
        org_id,
        "eventresync@x.com",
        "Event Resync User",
        None,
        Some(vec!["default".to_string()]),
    )
    .await
    .expect("SSO re-login should succeed");

    let events = event::Entity::find()
        .filter(event::Column::Action.eq("user.groups_synced"))
        .filter(event::Column::ObjectId.eq(existing.id))
        .all(&app.db)
        .await
        .expect("event query failed");
    assert_eq!(
        events.len(),
        1,
        "a real group change on resync must record exactly one user.groups_synced event: {events:?}"
    );

    let mut recorded_after: Vec<i64> = events[0].additional_properties["after"]
        .as_array()
        .expect("after must be an array")
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    let mut actual_after = synced.groups.clone();
    recorded_after.sort_unstable();
    actual_after.sort_unstable();
    assert_eq!(
        recorded_after, actual_after,
        "the event's 'after' must match the user's real post-write groups column, not a \
         pre-computed value that could diverge from what the write actually did"
    );
}

/// `forgot_password()` finds the user case-insensitively like SSO/login. Needs a password-bearing
/// account, since it no-ops without a `password_hash`.
#[tokio::test]
async fn forgot_password_matches_email_case_insensitively() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;

    let hash = arcodash_backend::auth::password::hash_password("S0meP4ssword!Pass")
        .await
        .unwrap();
    let mut active: user::ActiveModel = seed_user(&app.db, org_id, "User@X.com").await.into();
    active.password_hash = Set(Some(hash));
    active
        .update(&app.db)
        .await
        .expect("password backfill failed");

    let result = forgot_password(&app.db, org_id, "user@x.com")
        .await
        .expect("forgot_password must not error");

    assert!(
        result.is_some(),
        "forgot_password must find the user case-insensitively"
    );
}

/// A concurrent SSO login provisioning the same email must converge on the winner's row, not 500.
/// B's INSERT conflicts after A commits; the conflict must stay scoped to a nested transaction so
/// B's transaction can still take another write (every SSO handler calls `record_event` next).
#[tokio::test]
async fn concurrent_sso_provisioning_for_the_same_email_converges_on_the_winner() {
    let app = skip_if_none!(spawn_app().await);
    let org_id = make_org(&app.db).await;
    let email = format!("race-{}@test.local", uuid::Uuid::new_v4().simple());

    use sea_orm::TransactionTrait;
    let txn_a = app.db.begin().await.expect("begin failed");
    let a_user = seed_user(&txn_a, org_id, &email).await;

    let db = app.db.clone();
    let email_b = email.clone();
    let handle = tokio::spawn(async move {
        let txn_b = db.begin().await.expect("begin failed");
        let user =
            find_or_create_sso_user(&txn_b, org_id, &email_b, "Race Winner Should Be A", None)
                .await?;
        // Like the real handlers: a statement in the same transaction right after
        // `find_or_create_sso_user` proves it is still usable.
        arcodash_core::events::record_event(
            &txn_b,
            org_id,
            Some(user.id),
            "user.login",
            Some("user"),
            Some(user.id),
            serde_json::json!({}),
        )
        .await
        .map_err(arcodash_backend::error::ApiError::from)?;
        txn_b.commit().await.expect("B's commit must succeed");
        Ok::<_, arcodash_backend::error::ApiError>(user)
    });

    // Give B's SELECT time to miss and its INSERT time to block on the unique index before A
    // commits.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    txn_a.commit().await.expect("commit failed");

    let result = handle.await.expect("spawned task panicked").expect(
        "find_or_create_sso_user + the follow-up statement must not error out of this race",
    );
    assert_eq!(
        result.id, a_user.id,
        "the racing call must return A's row, not fail or create a second one"
    );

    let count = user::Entity::find()
        .filter(user::Column::OrgId.eq(org_id))
        .filter(user::Column::Email.eq(email.as_str()))
        .count(&app.db)
        .await
        .unwrap();
    assert_eq!(count, 1, "exactly one user row must exist for the email");
}

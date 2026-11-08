//! Integration tests: user management, invitation flow, and password reset (admin-only invite,
//! duplicate-email rejection, single-use invitation accept, api_key visibility, self-disable
//! rejection, no-enumeration forgot-password).

mod common;
use common::{
    authed_client, extract_set_cookie, make_org, spawn_app, spawn_app_with_real_mailer,
    user_with_role,
};
use common::{default_org_id, ok_json};

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::Value;

/// Read the invitation token (`users.details.invitation_token`) straight from the DB for a freshly
/// invited user.
async fn invitation_token_for_email(app: &common::TestApp, email: &str) -> String {
    use arcodash_core::entities::user;
    let u = user::Entity::find()
        .filter(user::Column::Email.eq(email))
        .one(&app.db)
        .await
        .expect("user query failed")
        .expect("invited user must exist");
    u.details
        .get("invitation_token")
        .and_then(Value::as_str)
        .expect("invited user must have details.invitation_token")
        .to_string()
}

/// Back-date `invitation_token_expires_at` so the token reads as expired, isolating the expiry
/// check from token matching.
async fn expire_invitation_token(app: &common::TestApp, email: &str) {
    use arcodash_core::entities::user;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

    let u = user::Entity::find()
        .filter(user::Column::Email.eq(email))
        .one(&app.db)
        .await
        .expect("user query failed")
        .expect("invited user must exist");

    let mut details = u.details.clone();
    details["invitation_token_expires_at"] =
        serde_json::json!((chrono::Utc::now() - chrono::Duration::hours(1)).timestamp());

    let mut active: user::ActiveModel = u.into();
    active.details = Set(details);
    active
        .update(&app.db)
        .await
        .expect("failed to back-date invitation token");
}

/// Set `disabled_at` on a user directly, without depending on the disable endpoint.
async fn disable_user_by_email(app: &common::TestApp, email: &str) {
    use arcodash_core::entities::user;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

    let u = user::Entity::find()
        .filter(user::Column::Email.eq(email))
        .one(&app.db)
        .await
        .expect("user query failed")
        .expect("invited user must exist");

    let mut active: user::ActiveModel = u.into();
    active.disabled_at = Set(Some(chrono::Utc::now().into()));
    active
        .update(&app.db)
        .await
        .expect("failed to disable user");
}

/// Invite a user as admin and return the created user's id + email.
async fn invite_user(app: &common::TestApp, admin: &reqwest::Client) -> (i64, String) {
    let email = format!("invitee_{}@test.local", uuid::Uuid::new_v4().simple());
    let body = ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({ "name": "Invitee", "email": email, "groups": [] }))
            .send()
            .await
            .unwrap(),
        "invite_user",
    )
    .await;
    (body["id"].as_i64().unwrap(), email)
}

/// Insert a standalone group backed by a role granting exactly `perms`, for the grant-ceiling
/// tests.
async fn insert_group_with_perms(app: &common::TestApp, org_id: i64, perms: &[&str]) -> i64 {
    use arcodash_core::entities::{group, group_role, role};
    use sea_orm::{ActiveValue::*, EntityTrait};

    let role_id = role::Entity::insert(role::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set(format!("role-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(perms.iter().map(|s| s.to_string()).collect()),
        r#type: Set("custom".to_string()),
        created_at: NotSet,
        updated_at: NotSet,
    })
    .exec(&app.db)
    .await
    .expect("role insert failed")
    .last_insert_id;

    let group_id = group::Entity::insert(group::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set(format!("group-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec![]),
        r#type: Set("regular".to_string()),
        created_at: NotSet,
        updated_at: NotSet,
        data_source_access_restricted: Set(false),
    })
    .exec(&app.db)
    .await
    .expect("group insert failed")
    .last_insert_id;

    group_role::Entity::insert(group_role::ActiveModel {
        id: NotSet,
        group_id: Set(group_id),
        role_id: Set(role_id),
        created_at: NotSet,
        updated_at: NotSet,
    })
    .exec(&app.db)
    .await
    .expect("group_role insert failed");

    group_id
}

/// Seed a non-admin user directly in the DB (default builtin group); returns a logged-in client +
/// id.
async fn create_nonadmin_client(app: &common::TestApp) -> (reqwest::Client, i64) {
    use arcodash_core::entities::{group, organization, user};
    use sea_orm::{sea_query::OnConflict, ActiveValue::*};

    let email = format!("nonadmin_{}@test.local", uuid::Uuid::new_v4().simple());
    let password = "N0nAdmin!Pass";

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();

    let default_group = group::Entity::find()
        .filter(group::Column::OrgId.eq(org.id))
        .filter(group::Column::Name.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default group must exist after migrations");

    let hash = {
        use argon2::{Argon2, PasswordHasher};
        Argon2::default()
            .hash_password(password.as_bytes())
            .unwrap()
            .to_string()
    };
    // Derived from the unique email so tests can reconstruct it; the row stores only the digest.
    let api_key = common::test_api_key(&email);

    let result = user::Entity::insert(user::ActiveModel {
        id: NotSet,
        org_id: Set(org.id),
        name: Set("Non Admin".to_string()),
        email: Set(email.clone()),
        password_hash: Set(Some(hash)),
        groups: Set(vec![default_group.id]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(&api_key)),
        disabled_at: Set(None),
        is_invitation_pending: Set(false),
        profile_image_url: Set(None),
        details: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
        digest_enabled: Set(false),
        digest_unsubscribe_token: Set(None),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::columns([user::Column::OrgId, user::Column::Email])
            .do_nothing()
            .to_owned(),
    )
    .exec(&app.db)
    .await;

    let user_id = match result {
        Ok(r) => r.last_insert_id,
        Err(sea_orm::DbErr::RecordNotInserted) => {
            user::Entity::find()
                .filter(user::Column::Email.eq(&email))
                .one(&app.db)
                .await
                .unwrap()
                .unwrap()
                .id
        }
        Err(e) => panic!("non-admin user seed failed: {e}"),
    };

    let (session, csrf) = app.login(&email, password).await;
    let client = common::authed_client(&app.address, &session, &csrf);
    (client, user_id)
}

/// POST /api/users as admin → 200 with is_invitation_pending=true and details.invitation_token
/// persisted, but no api_key (a pending user's key can't authenticate, so returning it would only
/// leak a secret).
#[tokio::test]
async fn invite_as_admin_returns_pending_user_without_api_key() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let email = format!("invitee_{}@test.local", uuid::Uuid::new_v4().simple());

    let body = ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({ "name": "Invitee", "email": email, "groups": [] }))
            .send()
            .await
            .unwrap(),
        "invite as admin",
    )
    .await;

    assert_eq!(
        body["is_invitation_pending"], true,
        "invited user must be pending"
    );
    assert!(
        body.get("api_key").is_none() || body["api_key"].is_null(),
        "invite response must not include api_key; got: {body}"
    );
    assert_eq!(body["email"], email);

    let token = invitation_token_for_email(&app, &email).await;
    assert!(
        !token.is_empty(),
        "invitation_token must be stored in details"
    );
}

/// POST /api/users as non-admin → 403.
#[tokio::test]
async fn invite_as_nonadmin_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (client, _) = create_nonadmin_client(&app).await;
    let resp = client
        .post(format!("{}/api/users", app.address))
        .json(&serde_json::json!({ "name": "Nope", "email": "nope@test.local", "groups": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 403, "non-admin invite must be 403");
}

/// POST /api/users with an already-existing email → 400.
#[tokio::test]
async fn invite_duplicate_email_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .post(format!("{}/api/users", app.address))
        .json(&serde_json::json!({
            "name": "Dup",
            "email": common::ADMIN_EMAIL,
            "groups": []
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "duplicate email invite must be 400"
    );
}

/// POST /api/users for an email that's still pending → resends: same row, a new token (old one
/// stops working), response still omits api_key.
#[tokio::test]
async fn invite_resend_for_pending_email_regenerates_token() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (user_id, email) = invite_user(&app, &admin).await;
    let old_token = invitation_token_for_email(&app, &email).await;

    let resend_body = ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({ "name": "Invitee", "email": email, "groups": [] }))
            .send()
            .await
            .unwrap(),
        "resend invite for pending email",
    )
    .await;

    assert_eq!(
        resend_body["is_invitation_pending"], true,
        "resent invite must still be pending"
    );
    assert!(
        resend_body.get("api_key").is_none() || resend_body["api_key"].is_null(),
        "resend response must not include api_key; got: {resend_body}"
    );
    assert_eq!(
        resend_body["id"].as_i64(),
        Some(user_id),
        "resend must reuse the same row"
    );

    let new_token = invitation_token_for_email(&app, &email).await;
    assert_ne!(
        new_token, old_token,
        "resend must regenerate a genuinely different token"
    );

    use arcodash_core::entities::user;
    let rows = user::Entity::find()
        .filter(user::Column::Email.eq(&email))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "resend must not create a duplicate user row; got: {rows:?}"
    );

    let anon = reqwest::Client::new();
    let stale = anon
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(&serde_json::json!({
            "token": old_token,
            "name": "Stale",
            "password": "Wh4tever!Pass"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        stale.status().as_u16(),
        400,
        "the original token must no longer work after resend"
    );

    let accepted = ok_json(
        anon.post(format!("{}/api/users/invitation/accept", app.address))
            .json(&serde_json::json!({
                "token": new_token,
                "name": "Resent Invitee",
                "password": "Acc3pt3d!Pass"
            }))
            .send()
            .await
            .unwrap(),
        "accept with the resent token",
    )
    .await;
    assert_eq!(
        accepted["is_invitation_pending"], false,
        "the resent token must accept successfully"
    );
}

/// A re-invite for a pending email applies the corrected `groups`, not just a fresh token.
#[tokio::test]
async fn invite_resend_updates_groups_to_the_newly_requested_set() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin = app.admin_client().await;

    let group_a = insert_group_with_perms(&app, org_id, &["view_dashboard"]).await;
    let group_b = insert_group_with_perms(&app, org_id, &["view_query"]).await;

    let email = format!("regroup-{}@test.local", uuid::Uuid::new_v4().simple());
    ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({ "name": "Regroup", "email": email, "groups": [group_a] }))
            .send()
            .await
            .unwrap(),
        "invite into group A",
    )
    .await;

    let resend = ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({ "name": "Regroup", "email": email, "groups": [group_b] }))
            .send()
            .await
            .unwrap(),
        "re-invite into group B",
    )
    .await;
    assert_eq!(
        resend["is_invitation_pending"], true,
        "re-invite of a still-pending user must still return pending"
    );

    let groups: Vec<i64> = resend["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert_eq!(
        groups,
        vec![group_b],
        "re-invite must replace the pending user's groups with the newly requested set"
    );
}

/// An idempotent resend with empty `groups` must not clear existing assignments (empty means no
/// change).
#[tokio::test]
async fn invite_resend_with_empty_groups_leaves_existing_groups_unchanged() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin = app.admin_client().await;

    let group_a = insert_group_with_perms(&app, org_id, &["view_dashboard"]).await;

    let email = format!("noop-resend-{}@test.local", uuid::Uuid::new_v4().simple());
    ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(
                &serde_json::json!({ "name": "Noop Resend", "email": email, "groups": [group_a] }),
            )
            .send()
            .await
            .unwrap(),
        "invite into group A",
    )
    .await;

    let resend = ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({ "name": "Noop Resend", "email": email, "groups": [] }))
            .send()
            .await
            .unwrap(),
        "resend with empty groups",
    )
    .await;

    let groups: Vec<i64> = resend["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert_eq!(
        groups,
        vec![group_a],
        "an empty groups list on resend must not clear the pending user's existing groups"
    );
}

/// The grant ceiling applies to re-invites like first invites: inviting first into a grantable
/// group, then re-inviting into one the caller can't grant, must not bypass it.
#[tokio::test]
async fn invite_resend_still_ceiling_checked_for_a_caller_who_cannot_grant_the_new_group() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin = app.admin_client().await;

    let (_, email) = invite_user(&app, &admin).await;
    let elevated_group = insert_group_with_perms(&app, org_id, &["manage_roles"]).await;
    let (caller, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;

    let bare = reqwest::Client::new();
    let resp = bare
        .post(format!(
            "{}/api/users?api_key={}",
            app.address,
            common::test_api_key(&caller.email)
        ))
        .json(&serde_json::json!({
            "name": "Invitee",
            "email": email,
            "groups": [elevated_group]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "a caller lacking the target group's permissions must be rejected on the re-invite path"
    );

    use arcodash_core::entities::user;
    let refetched = user::Entity::find()
        .filter(user::Column::Email.eq(&email))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !refetched.groups.contains(&elevated_group),
        "the rejected re-invite must not have applied the elevated group"
    );
}

/// GET /api/users/invitation/verify?token=... with a valid token → {email, name}, with an
/// unauthenticated client (the endpoint is public).
#[tokio::test]
async fn verify_invitation_valid_token_public_returns_email_and_name() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (_, email) = invite_user(&app, &admin).await;
    let token = invitation_token_for_email(&app, &email).await;

    let anon = reqwest::Client::new();
    let body = ok_json(
        anon.get(format!(
            "{}/api/users/invitation/verify?token={token}",
            app.address
        ))
        .send()
        .await
        .unwrap(),
        "verify invitation (public)",
    )
    .await;

    assert_eq!(body["email"], email);
    assert_eq!(body["name"], "Invitee");
}

/// GET /api/users/invitation/verify with an invalid token → 400.
#[tokio::test]
async fn verify_invitation_invalid_token_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let anon = reqwest::Client::new();
    let resp = anon
        .get(format!(
            "{}/api/users/invitation/verify?token=not-a-real-token",
            app.address
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "invalid invitation token must be 400"
    );
}

/// Verify for a disabled user's pending invitation → 400, indistinguishable from an invalid token.
#[tokio::test]
async fn verify_invitation_disabled_user_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (_, email) = invite_user(&app, &admin).await;
    let token = invitation_token_for_email(&app, &email).await;
    disable_user_by_email(&app, &email).await;

    let anon = reqwest::Client::new();
    let resp = anon
        .get(format!(
            "{}/api/users/invitation/verify?token={token}",
            app.address
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "a disabled user's invitation token must be rejected"
    );
}

/// Verify with an expired token → 400 with the `invitation_expired` code.
#[tokio::test]
async fn verify_invitation_expired_token_returns_expired_error_code() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (_, email) = invite_user(&app, &admin).await;
    let token = invitation_token_for_email(&app, &email).await;
    expire_invitation_token(&app, &email).await;

    let anon = reqwest::Client::new();
    let resp = anon
        .get(format!(
            "{}/api/users/invitation/verify?token={token}",
            app.address
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "expired invitation token must be 400"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error_code"], "invitation_expired",
        "expired token must carry a distinguishable error_code, not just a message string"
    );
}

/// POST /api/users/invitation/accept with valid token+password → 200, pending cleared, token
/// consumed (reuse → 400), new password can log in.
#[tokio::test]
async fn accept_invitation_sets_password_and_consumes_token() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (_, email) = invite_user(&app, &admin).await;
    let token = invitation_token_for_email(&app, &email).await;
    let password = "Acc3pt3d!Pass";

    let anon = reqwest::Client::new();
    let body = ok_json(
        anon.post(format!("{}/api/users/invitation/accept", app.address))
            .json(&serde_json::json!({
                "token": token,
                "name": "Accepted User",
                "password": password
            }))
            .send()
            .await
            .unwrap(),
        "accept invitation",
    )
    .await;

    assert_eq!(
        body["is_invitation_pending"], false,
        "accept must clear pending flag"
    );
    assert_eq!(body["name"], "Accepted User");

    // Token consumed: reuse must fail.
    let reuse = anon
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(&serde_json::json!({
            "token": token,
            "name": "Again",
            "password": "Wh4tever!Pass"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        reuse.status().as_u16(),
        400,
        "reusing a consumed invitation token must be 400"
    );

    let (session, _csrf) = app.login(&email, password).await;
    assert!(
        !session.is_empty(),
        "accepted user must be able to log in with new password"
    );
}

/// `accept_invitation` removes `details.invitation_token` while `forgot_password` adds
/// `details.reset_token`; when concurrent, both effects must survive rather than one snapshot write
/// dropping the other's key.
#[tokio::test]
async fn accept_invitation_concurrent_with_forgot_password_both_details_keys_survive() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (user_id, email) = invite_user(&app, &admin).await;
    let token = invitation_token_for_email(&app, &email).await;

    // Give the pending row a password directly so `forgot_password`'s `password_hash.is_some()`
    // gate passes whichever request lands first; the property under test is the `details`
    // merge/remove composition.
    let hash = arcodash_backend::auth::password::hash_password("Placeholder-Pw-1234!")
        .await
        .unwrap();
    {
        use arcodash_core::entities::user;
        use sea_orm::{ActiveModelTrait, ActiveValue::Set};
        let row = user::Entity::find_by_id(user_id)
            .one(&app.db)
            .await
            .unwrap()
            .unwrap();
        let mut active: user::ActiveModel = row.into();
        active.password_hash = Set(Some(hash));
        active.update(&app.db).await.unwrap();
    }

    let anon = reqwest::Client::new();
    let accept_req = anon
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(&serde_json::json!({
            "token": token,
            "name": "Accepted User",
            "password": "Acc3pt3d!Pass",
        }))
        .send();
    let reset_req = anon
        .post(format!("{}/api/forgot_password", app.address))
        .json(&serde_json::json!({ "email": email }))
        .send();
    let (accept_resp, reset_resp) = tokio::join!(accept_req, reset_req);
    assert_eq!(
        accept_resp.unwrap().status().as_u16(),
        200,
        "accept must succeed"
    );
    assert_eq!(
        reset_resp.unwrap().status().as_u16(),
        204,
        "forgot_password must succeed"
    );

    use arcodash_core::entities::user;
    let row = user::Entity::find_by_id(user_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !row.is_invitation_pending,
        "accept's effect must have landed"
    );
    assert!(
        row.details.get("reset_token").is_some(),
        "forgot_password's reset_token key must survive the concurrent accept write, got: {}",
        row.details
    );
    assert!(
        row.details.get("invitation_token").is_none(),
        "accept must still have removed its own key, got: {}",
        row.details
    );
}

/// Accept for a disabled user's pending invitation → 400 and no password set; the revocation holds
/// at accept time.
#[tokio::test]
async fn accept_invitation_disabled_user_returns_400_and_sets_no_password() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (_, email) = invite_user(&app, &admin).await;
    let token = invitation_token_for_email(&app, &email).await;
    disable_user_by_email(&app, &email).await;

    let anon = reqwest::Client::new();
    let resp = anon
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(&serde_json::json!({
            "token": token,
            "name": "Should Not Land",
            "password": "Acc3pt3d!Pass"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "a disabled user's invitation must not be acceptable"
    );

    use arcodash_core::entities::user;
    let refetched = user::Entity::find()
        .filter(user::Column::Email.eq(&email))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        refetched.is_invitation_pending,
        "a rejected accept must leave the pending flag untouched"
    );
    assert!(
        refetched.password_hash.is_none(),
        "a rejected accept must not set a password"
    );
}

/// Accept with a too-short password → 400 and the token is NOT consumed (a direct API call must not
/// set a trivial password).
#[tokio::test]
async fn accept_invitation_below_minimum_length_returns_400_and_leaves_token_usable() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (_, email) = invite_user(&app, &admin).await;
    let token = invitation_token_for_email(&app, &email).await;

    let anon = reqwest::Client::new();
    let short_resp = anon
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(&serde_json::json!({ "token": token, "name": "Too Short", "password": "short1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        short_resp.status().as_u16(),
        400,
        "a too-short password must be rejected server-side"
    );

    let ok_resp = anon
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(&serde_json::json!({
            "token": token,
            "name": "Long Enough",
            "password": "L0ngEnough!Pass"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        ok_resp.status().as_u16(),
        200,
        "a subsequent well-formed accept with the same token must still succeed"
    );
}

/// Accept signs the new user in directly: the response session cookie resolves to the accepted user
/// on `/api/session`.
#[tokio::test]
async fn accept_invitation_issues_session_cookie_and_signs_user_in() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (_, email) = invite_user(&app, &admin).await;
    let token = invitation_token_for_email(&app, &email).await;

    let anon = reqwest::Client::new();
    let resp = anon
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(&serde_json::json!({
            "token": token,
            "name": "Session User",
            "password": "S3ss10nDirect!Pass"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        200,
        "accept invitation must succeed"
    );
    let session_cookie = extract_set_cookie(resp.headers(), "arcodash_session")
        .expect("accept invitation must set the arcodash_session cookie");
    let csrf_cookie = extract_set_cookie(resp.headers(), "csrf_token")
        .expect("accept invitation must set the csrf_token cookie");

    let client = authed_client(&app.address, &session_cookie, &csrf_cookie);
    let session_resp = client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(session_resp.status().as_u16(), 200);
    let body: Value = session_resp.json().await.unwrap();
    assert_eq!(
        body["user"]["email"], email,
        "the cookie issued on accept must resolve to the newly-accepted user"
    );
}

/// Accept with an expired token → 400 with `invitation_expired`; the account stays pending with no
/// password.
#[tokio::test]
async fn accept_invitation_expired_token_returns_expired_error_code() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (_, email) = invite_user(&app, &admin).await;
    let token = invitation_token_for_email(&app, &email).await;
    expire_invitation_token(&app, &email).await;

    let anon = reqwest::Client::new();
    let resp = anon
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(
            &serde_json::json!({ "token": token, "name": "Too Late", "password": "Wh4tever!Pass" }),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "expired invitation token must be 400"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error_code"], "invitation_expired",
        "expired token must carry a distinguishable error_code, not just a message string"
    );

    use arcodash_core::entities::user;
    let u = user::Entity::find()
        .filter(user::Column::Email.eq(&email))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        u.is_invitation_pending,
        "a rejected expired-token accept must not clear the pending flag"
    );
    assert!(
        u.password_hash.is_none(),
        "a rejected expired-token accept must not set a password"
    );
}

/// GET /api/users lists all members and exposes no API key, not even the caller's own (only the
/// digest is stored).
#[tokio::test]
async fn list_users_never_exposes_api_keys_to_nonadmin() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (client, self_id) = create_nonadmin_client(&app).await;

    let body = ok_json(
        client
            .get(format!("{}/api/users", app.address))
            .send()
            .await
            .unwrap(),
        "list users as non-admin",
    )
    .await;
    let users = body.as_array().expect("users must be an array");

    assert!(
        users.len() >= 2,
        "list must include all members; got {}",
        users.len()
    );

    let me = users
        .iter()
        .find(|u| u["id"].as_i64() == Some(self_id))
        .expect("self in list");
    assert!(
        me.get("api_key").is_none(),
        "the list must not carry an api_key, even the caller's own; got: {me}"
    );

    for u in users {
        assert!(
            u.get("api_key").is_none(),
            "no user payload may carry an api_key; got: {u}"
        );
    }
}

/// GET /api/users as admin → no member's API key is exposed (only digests are stored).
#[tokio::test]
async fn list_users_as_admin_never_exposes_api_keys() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (_nonadmin, other_id) = create_nonadmin_client(&app).await;
    let admin = app.admin_client().await;

    let body = ok_json(
        admin
            .get(format!("{}/api/users", app.address))
            .send()
            .await
            .unwrap(),
        "list users as admin",
    )
    .await;
    let users = body.as_array().expect("users must be an array");

    let other = users
        .iter()
        .find(|u| u["id"].as_i64() == Some(other_id))
        .expect("other user must be in admin's list");
    assert!(
        other.get("api_key").is_none(),
        "an admin must not receive another member's api_key; got: {other}"
    );
    for u in users {
        assert!(
            u.get("api_key").is_none() && u.get("api_key_hash").is_none(),
            "no user payload may carry a key or its digest; got: {u}"
        );
    }
}

/// POST /api/users/:id by admin to set groups and disabled → 200 and persisted.
#[tokio::test]
async fn update_user_by_admin_sets_groups_and_disabled() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (_nonadmin, target_id) = create_nonadmin_client(&app).await;
    let admin = app.admin_client().await;

    let body = ok_json(
        admin
            .post(format!("{}/api/users/{target_id}", app.address))
            .json(&serde_json::json!({ "name": "Renamed", "disabled": true, "groups": [] }))
            .send()
            .await
            .unwrap(),
        "admin update user",
    )
    .await;

    assert_eq!(body["name"], "Renamed");
    assert_eq!(
        body["disabled"], true,
        "admin must be able to disable via update"
    );
    assert_eq!(
        body["groups"].as_array().map(|a| a.len()),
        Some(0),
        "groups must be cleared"
    );
}

/// POST /api/users/:id with `{"disabled": false}` must re-enable: clear `disabled_at` and record a
/// `user.enabled` event (covers the unlocked fast path silently no-opping).
#[tokio::test]
async fn re_enable_via_update_clears_disabled_at_and_is_audited() {
    use arcodash_core::entities::{event, user};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (_nonadmin, target_id) = create_nonadmin_client(&app).await;
    let admin = app.admin_client().await;

    let disable_body = ok_json(
        admin
            .post(format!("{}/api/users/{target_id}", app.address))
            .json(&serde_json::json!({ "disabled": true }))
            .send()
            .await
            .unwrap(),
        "admin disable user",
    )
    .await;
    assert_eq!(disable_body["disabled"], true);

    let disabled_at = user::Entity::find_by_id(target_id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("target user exists")
        .disabled_at;
    assert!(disabled_at.is_some(), "disable must set disabled_at");

    let enable_body = ok_json(
        admin
            .post(format!("{}/api/users/{target_id}", app.address))
            .json(&serde_json::json!({ "disabled": false }))
            .send()
            .await
            .unwrap(),
        "admin re-enable user",
    )
    .await;
    assert_eq!(
        enable_body["disabled"], false,
        "response must reflect the user is enabled"
    );

    let reloaded = user::Entity::find_by_id(target_id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("target user exists");
    assert!(
        reloaded.disabled_at.is_none(),
        "re-enable must clear disabled_at, not silently no-op"
    );

    let enabled_events = event::Entity::find()
        .filter(event::Column::OrgId.eq(reloaded.org_id))
        .filter(event::Column::Action.eq("user.enabled"))
        .filter(event::Column::ObjectId.eq(target_id))
        .all(&app.db)
        .await
        .unwrap();
    assert!(
        !enabled_events.is_empty(),
        "re-enable must record a user.enabled audit event"
    );
}

/// POST /api/users/:id (self, non-manager) with only allowed fields (name) → 200, persisted.
#[tokio::test]
async fn self_update_by_nonmanager_allowed_fields_only_returns_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (client, self_id) = create_nonadmin_client(&app).await;

    let body = ok_json(
        client
            .post(format!("{}/api/users/{self_id}", app.address))
            .json(&serde_json::json!({ "name": "Self Renamed" }))
            .send()
            .await
            .unwrap(),
        "nonmanager self update, allowed fields only",
    )
    .await;

    assert_eq!(body["name"], "Self Renamed");
}

/// POST /api/users/:id (self, non-manager) with a privileged field (groups) → 403, rejecting the
/// whole request (verified via a separate admin read).
#[tokio::test]
async fn self_update_by_nonmanager_privileged_field_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (client, self_id) = create_nonadmin_client(&app).await;
    let admin = app.admin_client().await;

    let before = ok_json(
        admin
            .get(format!("{}/api/users/{self_id}", app.address))
            .send()
            .await
            .unwrap(),
        "admin read before",
    )
    .await;
    let before_groups = before["groups"].clone();

    let resp = client
        .post(format!("{}/api/users/{self_id}", app.address))
        .json(&serde_json::json!({ "name": "Should Not Apply", "groups": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        403,
        "non-manager self-update carrying a privileged field must be rejected"
    );

    let after = ok_json(
        admin
            .get(format!("{}/api/users/{self_id}", app.address))
            .send()
            .await
            .unwrap(),
        "admin read after",
    )
    .await;

    assert_eq!(
        after["groups"], before_groups,
        "groups must be unchanged after the rejected request"
    );
    assert_ne!(
        after["name"], "Should Not Apply",
        "the allowed field in the same payload must not apply either — reject the whole request, not just the privileged part"
    );
}

/// POST /api/users/:id (manager, on another user) with groups/disabled → 200 and applied.
#[tokio::test]
async fn manager_can_still_set_privileged_fields_on_other_user() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (_nonadmin, target_id) = create_nonadmin_client(&app).await;
    let admin = app.admin_client().await;

    let body = ok_json(
        admin
            .post(format!("{}/api/users/{target_id}", app.address))
            .json(&serde_json::json!({ "groups": [], "disabled": true }))
            .send()
            .await
            .unwrap(),
        "manager sets privileged fields on another user",
    )
    .await;

    assert_eq!(body["groups"].as_array().map(|a| a.len()), Some(0));
    assert_eq!(body["disabled"], true);
}

/// DELETE /api/users/:id of self → 400 'cannot disable yourself'.
#[tokio::test]
async fn disable_self_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let session_body = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "GET /api/session",
    )
    .await;
    let admin_id = session_body["user"]["id"].as_i64().unwrap();

    let resp = admin
        .delete(format!("{}/api/users/{admin_id}", app.address))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "disabling yourself must be 400"
    );
    let txt = resp.text().await.unwrap_or_default();
    assert!(
        txt.contains("cannot disable yourself"),
        "error body must mention self-disable; got: {txt}"
    );
}

/// POST /api/users/:id (self, via update) with disabled:true → 400 'cannot disable yourself';
/// disabled:false and a plain rename on self both still succeed.
#[tokio::test]
async fn self_disable_via_update_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let session_body = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "GET /api/session",
    )
    .await;
    let admin_id = session_body["user"]["id"].as_i64().unwrap();

    let resp = admin
        .post(format!("{}/api/users/{admin_id}", app.address))
        .json(&serde_json::json!({ "disabled": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "self-disable via update must be 400"
    );
    let txt = resp.text().await.unwrap_or_default();
    assert!(
        txt.contains("cannot disable yourself"),
        "error body must mention self-disable; got: {txt}"
    );

    let ok_resp = admin
        .post(format!("{}/api/users/{admin_id}", app.address))
        .json(&serde_json::json!({ "disabled": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        ok_resp.status().as_u16(),
        200,
        "disabled:false on self must succeed"
    );

    let rename_resp = admin
        .post(format!("{}/api/users/{admin_id}", app.address))
        .json(&serde_json::json!({ "name": "Still Me" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        rename_resp.status().as_u16(),
        200,
        "rename on self must succeed"
    );
}

/// DELETE /api/users/:id (disable other) as non-admin → 403.
#[tokio::test]
async fn disable_other_as_nonadmin_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (client, _) = create_nonadmin_client(&app).await;
    let admin = app.admin_client().await;
    let session_body = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "GET /api/session",
    )
    .await;
    let admin_id = session_body["user"]["id"].as_i64().unwrap();

    let resp = client
        .delete(format!("{}/api/users/{admin_id}", app.address))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        403,
        "non-admin disabling another user must be 403"
    );
}

/// POST /api/users/:id/regenerate_api_key as self → new api_key returned and differs.
#[tokio::test]
async fn regenerate_api_key_as_self_returns_new_key() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (client, self_id) = create_nonadmin_client(&app).await;

    // A key is never readable off the user row (only its digest), so the first regenerate call
    // yields a key to compare against.
    let first = ok_json(
        client
            .post(format!(
                "{}/api/users/{self_id}/regenerate_api_key",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "first regenerate",
    )
    .await;
    let old_key = first["api_key"]
        .as_str()
        .expect("regenerate must return the new api_key")
        .to_string();

    let body = ok_json(
        client
            .post(format!(
                "{}/api/users/{self_id}/regenerate_api_key",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "regenerate own api_key",
    )
    .await;

    let new_key = body["api_key"]
        .as_str()
        .expect("regenerate must return api_key");
    assert!(!new_key.is_empty(), "new api_key must be non-empty");
    assert_ne!(
        new_key, old_key,
        "regenerated api_key must differ from the old one"
    );

    // The returned key must authenticate (digest matches the auth path) and the superseded one must
    // not.
    let bare = reqwest::Client::new();
    let with_new = bare
        .get(format!(
            "{}/api/users/{self_id}?api_key={new_key}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        with_new.status().as_u16(),
        200,
        "the freshly returned api_key must authenticate"
    );

    let with_old = bare
        .get(format!(
            "{}/api/users/{self_id}?api_key={old_key}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        with_old.status().as_u16(),
        401,
        "a superseded api_key must stop authenticating"
    );
}

/// POST /api/users/:id/regenerate_api_key for another user as non-admin → 403.
#[tokio::test]
async fn regenerate_api_key_for_other_as_nonadmin_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (client, _) = create_nonadmin_client(&app).await;
    let admin = app.admin_client().await;
    let session_body = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "GET /api/session",
    )
    .await;
    let admin_id = session_body["user"]["id"].as_i64().unwrap();

    let resp = client
        .post(format!(
            "{}/api/users/{admin_id}/regenerate_api_key",
            app.address
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        403,
        "non-admin regenerating another user's api_key must be 403"
    );
}

/// POST /api/forgot_password for an unknown email → 204 (no enumeration).
#[tokio::test]
async fn forgot_password_unknown_email_returns_204() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    // forgot_password still passes through CSRF/session middleware, so use an authenticated client.
    let admin = app.admin_client().await;
    let resp = admin
        .post(format!("{}/api/forgot_password", app.address))
        .json(&serde_json::json!({ "email": "does-not-exist@test.local" }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        204,
        "unknown email must still return 204 (no enumeration)"
    );
}

/// POST /api/forgot_password for a known email → 204 and a reset_token is stored.
#[tokio::test]
async fn forgot_password_known_email_returns_204() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .post(format!("{}/api/forgot_password", app.address))
        .json(&serde_json::json!({ "email": common::ADMIN_EMAIL }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 204, "known email must return 204");

    use arcodash_core::entities::user;
    let u = user::Entity::find()
        .filter(user::Column::Email.eq(common::ADMIN_EMAIL))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        u.details
            .get("reset_token")
            .and_then(Value::as_str)
            .is_some(),
        "known-email forgot_password must store a reset_token; got details: {}",
        u.details
    );
}

/// POST /api/reset_password with an invalid token → 400.
#[tokio::test]
async fn reset_password_invalid_token_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .post(format!("{}/api/reset_password", app.address))
        .json(&serde_json::json!({ "token": "bogus-token", "password": "Wh4tever!Pass" }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "invalid reset token must be 400"
    );
}

/// Reset with a valid token but a too-short password → 400 and the token is NOT consumed.
#[tokio::test]
async fn reset_password_below_minimum_length_returns_400_and_leaves_token_usable() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .post(format!("{}/api/forgot_password", app.address))
        .json(&serde_json::json!({ "email": common::ADMIN_EMAIL }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);

    use arcodash_core::entities::user;
    let u = user::Entity::find()
        .filter(user::Column::Email.eq(common::ADMIN_EMAIL))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let token = u
        .details
        .get("reset_token")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let short_resp = admin
        .post(format!("{}/api/reset_password", app.address))
        .json(&serde_json::json!({ "token": token, "password": "" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        short_resp.status().as_u16(),
        400,
        "an empty password must be rejected server-side"
    );

    let ok_resp = admin
        .post(format!("{}/api/reset_password", app.address))
        .json(&serde_json::json!({ "token": token, "password": "L0ngEnough!Pass" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        ok_resp.status().as_u16(),
        204,
        "a subsequent well-formed reset with the same token must still succeed"
    );
}

/// An invite-pending user's API key must not authenticate anything; once the invitation is accepted
/// the same key works. A disabled user's key is rejected throughout.
#[tokio::test]
async fn invite_pending_api_key_rejected_until_accepted() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (user_id, email) = invite_user(&app, &admin).await;

    use arcodash_core::entities::user;
    let pending = user::Entity::find_by_id(user_id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("invited user must exist");
    assert!(pending.is_invitation_pending, "sanity: user starts pending");
    assert!(
        !pending.api_key_hash.is_empty(),
        "invited user must have a generated api_key"
    );

    // Only the key digest is stored, so plant a known key to exercise the pending/accepted rule.
    let key = "invite-pending-probe-key".to_string();
    {
        use sea_orm::{ActiveModelTrait, ActiveValue::Set};
        let mut active: user::ActiveModel = pending.clone().into();
        active.api_key_hash = Set(arcodash_core::hash::hash_api_key(&key));
        active.update(&app.db).await.expect("plant probe key");
    }

    let bare = reqwest::Client::new();
    let blocked = bare
        .get(format!("{}/api/queries?api_key={key}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        blocked.status().as_u16(),
        401,
        "an invite-pending user's api_key must not authenticate"
    );

    let token = invitation_token_for_email(&app, &email).await;
    let accept_resp = bare
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(&serde_json::json!({
            "token": token,
            "name": "Invitee",
            "password": "Acc3pt3d!Pass"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(accept_resp.status().as_u16(), 200, "accept invitation");

    let allowed = bare
        .get(format!("{}/api/queries?api_key={key}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        allowed.status().as_u16(),
        200,
        "the same api_key must authenticate once the invitation is accepted"
    );

    // A normal user's key still works; a disabled user's key is still rejected.
    let (normal_client, normal_id) = create_nonadmin_client(&app).await;
    let normal_user = user::Entity::find_by_id(normal_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let normal_ok = bare
        .get(format!(
            "{}/api/queries?api_key={}",
            app.address,
            common::test_api_key(&normal_user.email)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        normal_ok.status().as_u16(),
        200,
        "a normal accepted user's api_key must keep working"
    );
    drop(normal_client);

    let disable_resp = admin
        .post(format!("{}/api/users/{normal_id}", app.address))
        .json(&serde_json::json!({ "disabled": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(disable_resp.status().as_u16(), 200, "disable user");
    let disabled_blocked = bare
        .get(format!(
            "{}/api/queries?api_key={}",
            app.address,
            common::test_api_key(&normal_user.email)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        disabled_blocked.status().as_u16(),
        401,
        "a disabled user's api_key must still be rejected"
    );
}

// Last-admin guard (`user_service::update`): these tests run directly against a fresh `make_org`
// org, since the shared "default" org's seeded admin makes the admin count unpredictable.

/// A sole admin stripping their OWN admin-group membership must be rejected, or the org is left
/// with no admin.
#[tokio::test]
async fn sole_admin_cannot_remove_own_admin_group() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = make_org(&app.db).await;

    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let original_groups = admin.groups.clone();

    let result = arcodash_backend::services::user_service::update(
        &app.db,
        org_id,
        &admin,
        admin.id,
        None,
        Some(vec![]),
        None,
    )
    .await;
    let err = result.expect_err("removing the last admin's own admin group must be rejected");
    assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);

    use arcodash_core::entities::user;
    let refetched = user::Entity::find_by_id(admin.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        refetched.groups, original_groups,
        "the rejected update must not have touched the groups column"
    );
    assert!(
        arcodash_backend::services::user_service::is_admin_user(&app.db, &refetched)
            .await
            .unwrap(),
        "the org must still have its one admin"
    );
}

/// A `manage_users` holder (non-admin) stripping the only admin's admin-group membership is
/// rejected (403: the revoke ceiling stops them before the last-admin guard).
#[tokio::test]
async fn manage_users_holder_cannot_remove_a_different_sole_admins_group() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = make_org(&app.db).await;

    let (manager, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let (sole_admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let original_groups = sole_admin.groups.clone();

    let result = arcodash_backend::services::user_service::update(
        &app.db,
        org_id,
        &manager,
        sole_admin.id,
        None,
        Some(vec![]),
        None,
    )
    .await;
    let err = result.expect_err(
        "removing the org's only admin's admin group must be rejected, even by a different caller",
    );
    assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);

    use arcodash_core::entities::user;
    let refetched = user::Entity::find_by_id(sole_admin.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(refetched.groups, original_groups);
    assert!(
        arcodash_backend::services::user_service::is_admin_user(&app.db, &refetched)
            .await
            .unwrap(),
        "the sole admin must still be an admin"
    );
}

/// Removing an admin's admin-group membership is allowed once another admin exists.
#[tokio::test]
async fn removing_an_admins_group_is_allowed_when_another_admin_exists() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = make_org(&app.db).await;

    let (admin1, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let (admin2, _) = user_with_role(&app.db, org_id, &["admin"]).await;

    let updated = arcodash_backend::services::user_service::update(
        &app.db,
        org_id,
        &admin1,
        admin2.id,
        None,
        Some(vec![]),
        None,
    )
    .await;
    assert!(
        updated.is_ok(),
        "demoting one of two admins must be allowed while another admin remains: {:?}",
        updated.err()
    );

    use arcodash_core::entities::user;
    let refetched_admin2 = user::Entity::find_by_id(admin2.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !arcodash_backend::services::user_service::is_admin_user(&app.db, &refetched_admin2)
            .await
            .unwrap(),
        "admin2 must have lost admin status"
    );
    let refetched_admin1 = user::Entity::find_by_id(admin1.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        arcodash_backend::services::user_service::is_admin_user(&app.db, &refetched_admin1)
            .await
            .unwrap(),
        "admin1 must be unaffected"
    );
}

/// `disable` must reject disabling the org's sole admin, the same rule as `update`'s groups branch.
#[tokio::test]
async fn disable_rejects_the_sole_admin() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = make_org(&app.db).await;

    let (manager, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let (sole_admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;

    let result =
        arcodash_backend::services::user_service::disable(&app.db, org_id, &manager, sole_admin.id)
            .await;
    let err = result.expect_err("disabling the org's only admin must be rejected");
    assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);

    use arcodash_core::entities::user;
    let refetched = user::Entity::find_by_id(sole_admin.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(refetched.disabled_at.is_none(), "must not be disabled");
}

/// Disabling one of two admins is allowed — the guard only blocks reaching zero admins.
#[tokio::test]
async fn disable_allows_an_admin_when_another_admin_exists() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = make_org(&app.db).await;

    let (admin1, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let (admin2, _) = user_with_role(&app.db, org_id, &["admin"]).await;

    let result =
        arcodash_backend::services::user_service::disable(&app.db, org_id, &admin1, admin2.id)
            .await;
    assert!(result.is_ok(), "{:?}", result.err());

    use arcodash_core::entities::user;
    let refetched = user::Entity::find_by_id(admin2.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(refetched.disabled_at.is_some());
}

/// `update`'s `disabled: true` branch must also reject disabling the sole admin.
#[tokio::test]
async fn update_disabled_true_rejects_the_sole_admin() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = make_org(&app.db).await;

    let (manager, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let (sole_admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;

    let result = arcodash_backend::services::user_service::update(
        &app.db,
        org_id,
        &manager,
        sole_admin.id,
        None,
        None,
        Some(true),
    )
    .await;
    let err = result.expect_err("disabling the org's only admin via update must be rejected");
    assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);

    use arcodash_core::entities::user;
    let refetched = user::Entity::find_by_id(sole_admin.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(refetched.disabled_at.is_none());
}

/// A caller lacking `view_users` may fetch only their OWN profile; a `view_users` holder (or admin)
/// may fetch any teammate's. Runs on the shared "default" org since the OSS harness only resolves
/// api_key users there.
#[tokio::test]
async fn get_user_requires_view_users_for_other_profiles_but_not_self() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;

    let (narrow, _) = user_with_role(&app.db, org_id, &["view_dashboard"]).await;
    let (teammate, _) = user_with_role(&app.db, org_id, &["view_dashboard"]).await;
    let (viewer_holder, _) = user_with_role(&app.db, org_id, &["view_users"]).await;

    let bare = reqwest::Client::new();

    let other_resp = bare
        .get(format!(
            "{}/api/users/{}?api_key={}",
            app.address,
            teammate.id,
            common::test_api_key(&narrow.email)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        other_resp.status().as_u16(),
        403,
        "a caller lacking view_users must be denied another teammate's profile"
    );

    let self_resp = bare
        .get(format!(
            "{}/api/users/{}?api_key={}",
            app.address,
            narrow.id,
            common::test_api_key(&narrow.email)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        self_resp.status().as_u16(),
        200,
        "self-fetch must stay ungated regardless of view_users"
    );

    let holder_resp = bare
        .get(format!(
            "{}/api/users/{}?api_key={}",
            app.address,
            teammate.id,
            common::test_api_key(&viewer_holder.email)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        holder_resp.status().as_u16(),
        200,
        "a view_users holder must be able to fetch any teammate's profile"
    );
}

/// A session with exactly the builtin Viewer permission set (no `view_users`) is denied the org
/// directory list and other teammates' profiles.
#[tokio::test]
async fn viewer_permission_set_is_denied_user_directory_reads() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;

    let (viewer, _) = user_with_role(
        &app.db,
        org_id,
        arcodash_core::org_seed::VIEWER_ROLE_PERMISSIONS,
    )
    .await;
    let (teammate, _) = user_with_role(&app.db, org_id, &["view_dashboard"]).await;

    let bare = reqwest::Client::new();

    let list_resp = bare
        .get(format!(
            "{}/api/users?api_key={}",
            app.address,
            common::test_api_key(&viewer.email)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        list_resp.status().as_u16(),
        403,
        "a strict Viewer session must be denied the org user directory"
    );

    let other_resp = bare
        .get(format!(
            "{}/api/users/{}?api_key={}",
            app.address,
            teammate.id,
            common::test_api_key(&viewer.email)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        other_resp.status().as_u16(),
        403,
        "a strict Viewer session must be denied a teammate's profile"
    );
}

// Resend regenerates the invite token by id; revoke deletes the never-accepted row outright (unlike
// `disable`, which soft-disables an active account).

/// POST /api/users/:id/resend_invitation as admin → 200, a new token, the old one stops working.
#[tokio::test]
async fn resend_invitation_as_admin_regenerates_token() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (user_id, email) = invite_user(&app, &admin).await;
    let old_token = invitation_token_for_email(&app, &email).await;

    let body = ok_json(
        admin
            .post(format!(
                "{}/api/users/{user_id}/resend_invitation",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "resend invitation as admin",
    )
    .await;
    assert_eq!(body["id"].as_i64(), Some(user_id));
    assert_eq!(body["is_invitation_pending"], true);

    let new_token = invitation_token_for_email(&app, &email).await;
    assert_ne!(
        new_token, old_token,
        "resend must regenerate a genuinely different token"
    );

    let anon = reqwest::Client::new();
    let stale = anon
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(&serde_json::json!({ "token": old_token, "name": "Stale", "password": "Wh4tever!Pass" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        stale.status().as_u16(),
        400,
        "the original token must no longer work after resend"
    );

    let accepted = ok_json(
        anon.post(format!("{}/api/users/invitation/accept", app.address))
            .json(&serde_json::json!({ "token": new_token, "name": "Resent Invitee", "password": "Acc3pt3d!Pass" }))
            .send()
            .await
            .unwrap(),
        "accept with the resent token",
    )
    .await;
    assert_eq!(accepted["is_invitation_pending"], false);
}

/// POST /api/users/:id/resend_invitation as non-admin → 403.
#[tokio::test]
async fn resend_invitation_as_nonadmin_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (user_id, _email) = invite_user(&app, &admin).await;
    let (client, _) = create_nonadmin_client(&app).await;

    let resp = client
        .post(format!(
            "{}/api/users/{user_id}/resend_invitation",
            app.address
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 403, "non-admin resend must be 403");
}

/// Resend for a user who already accepted → 400, leaving the account untouched.
#[tokio::test]
async fn resend_invitation_on_accepted_user_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (user_id, email) = invite_user(&app, &admin).await;
    let token = invitation_token_for_email(&app, &email).await;

    let anon = reqwest::Client::new();
    anon.post(format!("{}/api/users/invitation/accept", app.address))
        .json(
            &serde_json::json!({ "token": token, "name": "Accepted", "password": "Acc3pt3d!Pass" }),
        )
        .send()
        .await
        .unwrap();

    let resp = admin
        .post(format!(
            "{}/api/users/{user_id}/resend_invitation",
            app.address
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "resend on an already-accepted user must be 400"
    );
}

/// DELETE /api/users/:id/invitation as admin → 204 and the pending row is gone (unlike `disable`);
/// the old token can't be accepted.
#[tokio::test]
async fn revoke_invitation_as_admin_deletes_the_pending_user() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (user_id, email) = invite_user(&app, &admin).await;
    let token = invitation_token_for_email(&app, &email).await;

    let resp = admin
        .delete(format!("{}/api/users/{user_id}/invitation", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204, "revoke invitation must be 204");

    use arcodash_core::entities::user;
    let gone = user::Entity::find_by_id(user_id)
        .one(&app.db)
        .await
        .unwrap();
    assert!(
        gone.is_none(),
        "a revoked invitation must delete the pending row outright, not soft-disable it"
    );

    let anon = reqwest::Client::new();
    let accept_after_revoke = anon
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(
            &serde_json::json!({ "token": token, "name": "Too Late", "password": "Wh4tever!Pass" }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        accept_after_revoke.status().as_u16(),
        400,
        "a revoked invitation's token must no longer be acceptable"
    );
}

/// DELETE /api/users/:id/invitation as non-admin → 403.
#[tokio::test]
async fn revoke_invitation_as_nonadmin_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (user_id, _email) = invite_user(&app, &admin).await;
    let (client, _) = create_nonadmin_client(&app).await;

    let resp = client
        .delete(format!("{}/api/users/{user_id}/invitation", app.address))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 403, "non-admin revoke must be 403");
}

/// Revoke on an already-active user → 400 and the account stays; revoke only targets pending
/// invitations.
#[tokio::test]
async fn revoke_invitation_on_active_user_returns_400_and_leaves_account_intact() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (client, target_id) = create_nonadmin_client(&app).await;
    drop(client);

    let resp = admin
        .delete(format!("{}/api/users/{target_id}/invitation", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "revoke on an active (already-accepted) user must be 400"
    );

    use arcodash_core::entities::user;
    let still_there = user::Entity::find_by_id(target_id)
        .one(&app.db)
        .await
        .unwrap();
    assert!(
        still_there.is_some(),
        "an active user must be untouched by a rejected revoke call"
    );
}

/// The invitation email is a real SMTP send (`spawn_app_with_real_mailer`) verified against a real
/// Mailpit: both MIME parts exist, org and inviter are named, a non-ASCII org name survives, and
/// the brand string stays lowercase. Must use `admin_client()` (the shared "default" org): OSS
/// `resolve_org` always resolves to "default", so a key-authenticated user in another org can't
/// authenticate. The test renames "default" for the send and restores it afterwards. Skips if
/// Mailpit isn't reachable.
#[tokio::test]
async fn invite_email_lands_in_mailpit_with_html_text_and_org_inviter_named() {
    let app = skip_if_none!(spawn_app_with_real_mailer().await);
    app.cleanup().await;

    let mailpit_base =
        std::env::var("MAILPIT_API_BASE").unwrap_or_else(|_| "http://localhost:8026".to_string());
    let client = reqwest::Client::new();
    if client
        .get(format!("{mailpit_base}/api/v1/info"))
        .send()
        .await
        .is_err()
    {
        eprintln!("skipping: Mailpit not reachable at {mailpit_base}");
        return;
    }

    let admin = app.admin_client().await;
    let email = format!(
        "mailpit-invite-{}@test.local",
        uuid::Uuid::new_v4().simple()
    );

    use arcodash_core::entities::{organization, user};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};
    let inviter_name = user::Entity::find()
        .filter(user::Column::Email.eq(common::ADMIN_EMAIL))
        .one(&app.db)
        .await
        .unwrap()
        .expect("seeded admin must exist")
        .name;
    // A non-ASCII org name (with an em dash) is what proves the SMTP round-trip keeps UTF-8 intact.
    let original_org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist");
    let original_org_name = original_org.name.clone();
    let mut default_org: organization::ActiveModel = original_org.into();
    default_org.name = Set("Café Données — Équipe".to_string());
    let org_name = default_org.update(&app.db).await.unwrap().name;

    let body = ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({ "name": "Mailpit Invitee", "email": email, "groups": [] }))
            .send()
            .await
            .unwrap(),
        "invite for mailpit check",
    )
    .await;
    assert_eq!(body["email"], email);

    let mut message_id = None;
    for _ in 0..30 {
        let search: Value = client
            .get(format!("{mailpit_base}/api/v1/search"))
            .query(&[("query", format!("to:{email}"))])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(m) = search["messages"].as_array().and_then(|a| a.first()) {
            message_id = m["ID"].as_str().map(str::to_string);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    let message_id = message_id.expect("invitation email must land in Mailpit within ~9s");

    let detail: Value = client
        .get(format!("{mailpit_base}/api/v1/message/{message_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Restore before asserting so a failure cannot leave the shared org renamed.
    let mut default_org: organization::ActiveModel = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist")
        .into();
    default_org.name = Set(original_org_name);
    default_org.update(&app.db).await.unwrap();

    let subject = detail["Subject"].as_str().unwrap_or_default();
    assert_eq!(
        subject,
        format!("{inviter_name} invited you to {org_name} on arcodash")
    );

    let html = detail["HTML"].as_str().unwrap_or_default();
    let text = detail["Text"].as_str().unwrap_or_default();
    assert!(!html.is_empty(), "HTML part must be present");
    assert!(!text.is_empty(), "text alternative must be present");

    for part in [html, text] {
        assert!(part.contains(&org_name), "org name must be named: {part}");
        assert!(
            part.contains(&inviter_name),
            "inviter must be named: {part}"
        );
        assert!(
            part.contains("arcodash") && !part.contains("Arcodash"),
            "brand string must stay lowercase per the UX casing ruling: {part}"
        );
    }
    assert!(
        text.contains('—'),
        "the org name's em dash must survive the real SMTP round-trip: {text}"
    );
    assert!(
        html.contains('—'),
        "the org name's em dash must survive in the HTML part too: {html}"
    );
    assert!(html.contains("Accept invitation"));
    assert!(text.contains("Accept invitation:"));
}

/// Unknown-inviter fallback: an inviter with an empty `name` must not produce a subject like `"
/// invited you to ... on arcodash"`; it falls back to name-free phrasing and omits the `Invited by`
/// fact. Uses a purpose-built `manage_users` holder rather than mutating the shared admin.
#[tokio::test]
async fn invite_email_with_empty_inviter_name_falls_back_per_spec() {
    let app = skip_if_none!(spawn_app_with_real_mailer().await);
    app.cleanup().await;

    let mailpit_base =
        std::env::var("MAILPIT_API_BASE").unwrap_or_else(|_| "http://localhost:8026".to_string());
    let client = reqwest::Client::new();
    if client
        .get(format!("{mailpit_base}/api/v1/info"))
        .send()
        .await
        .is_err()
    {
        eprintln!("skipping: Mailpit not reachable at {mailpit_base}");
        return;
    }

    use arcodash_core::entities::{organization, user};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};

    let org_id = default_org_id(&app.db).await;
    let (inviter, _) = user_with_role(&app.db, org_id, &["manage_users"]).await;
    let mut active: user::ActiveModel = inviter.clone().into();
    active.name = Set(String::new());
    active
        .update(&app.db)
        .await
        .expect("failed to blank the inviter's name");

    let org_name = organization::Entity::find_by_id(org_id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist")
        .name;

    let invitee_email = format!(
        "mailpit-noname-invite-{}@test.local",
        uuid::Uuid::new_v4().simple()
    );
    let resp = client
        .post(format!(
            "{}/api/users?api_key={}",
            app.address,
            common::test_api_key(&inviter.email)
        ))
        .json(
            &serde_json::json!({ "name": "No-Name Invitee", "email": invitee_email, "groups": [] }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "invite must succeed");

    let mut message_id = None;
    for _ in 0..30 {
        let search: Value = client
            .get(format!("{mailpit_base}/api/v1/search"))
            .query(&[("query", format!("to:{invitee_email}"))])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(m) = search["messages"].as_array().and_then(|a| a.first()) {
            message_id = m["ID"].as_str().map(str::to_string);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    let message_id = message_id.expect("invitation email must land in Mailpit within ~9s");

    let detail: Value = client
        .get(format!("{mailpit_base}/api/v1/message/{message_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let subject = detail["Subject"].as_str().unwrap_or_default();
    assert_eq!(
        subject,
        format!("You've been invited to {org_name} on arcodash"),
        "an empty inviter name must never be interpolated into the subject"
    );

    let text = detail["Text"].as_str().unwrap_or_default();
    let html = detail["HTML"].as_str().unwrap_or_default();
    for part in [text, html] {
        assert!(
            !part.contains("Invited by"),
            "the Invited by fact must be omitted entirely, never shown with a blank/'someone' value: {part}"
        );
    }
}

/// Two admins, each the sole member of their own admin-conferring group, concurrently drop their
/// own membership via `POST /api/users/:id`: the last-admin write-skew race that
/// `user_service::update`'s `pg_advisory_xact_lock` closes. Exactly one self-removal must win.
#[tokio::test]
async fn concurrent_self_group_removal_via_update_never_leaves_zero_admins() {
    use arcodash_core::entities::user;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;

    // Strip the seeded admin's admin membership first, so the two purpose-built admins are the
    // org's only admins.
    let seeded_admin = user::Entity::find()
        .filter(user::Column::OrgId.eq(org_id))
        .filter(user::Column::Email.eq(common::ADMIN_EMAIL))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut seeded_active: user::ActiveModel = seeded_admin.into();
    seeded_active.groups = Set(vec![]);
    seeded_active.update(&app.db).await.unwrap();

    let (admin_a, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let (admin_b, _) = user_with_role(&app.db, org_id, &["admin"]).await;

    let bare = reqwest::Client::new();
    let update_a = bare
        .post(format!(
            "{}/api/users/{}?api_key={}",
            app.address,
            admin_a.id,
            common::test_api_key(&admin_a.email)
        ))
        .json(&serde_json::json!({ "groups": [] }));
    let update_b = bare
        .post(format!(
            "{}/api/users/{}?api_key={}",
            app.address,
            admin_b.id,
            common::test_api_key(&admin_b.email)
        ))
        .json(&serde_json::json!({ "groups": [] }));

    let (resp_a, resp_b) = tokio::join!(update_a.send(), update_b.send());
    let status_a = resp_a.unwrap().status().as_u16();
    let status_b = resp_b.unwrap().status().as_u16();

    // Checked before the status codes: the count is the property that matters and detects a lock
    // regression better than codes, which can coincidentally be [200, 400].
    let remaining_admins = user::Entity::find()
        .filter(user::Column::Id.is_in([admin_a.id, admin_b.id]))
        .filter(user::Column::DisabledAt.is_null())
        .all(&app.db)
        .await
        .unwrap()
        .into_iter()
        .filter(|u| !u.groups.is_empty())
        .count();
    assert_eq!(
        remaining_admins, 1,
        "the org must end with exactly one admin, never zero and never both"
    );

    let mut statuses = [status_a, status_b];
    statuses.sort_unstable();
    assert_eq!(
        statuses,
        [200, 400],
        "exactly one self-removal must be rejected as the last admin; got {status_a} and {status_b}"
    );
}

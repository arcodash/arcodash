//! User management: CRUD, invitations, forgot/reset password.

use std::collections::HashSet;

use chrono::Utc;
use sea_orm::sea_query::Expr;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, TransactionTrait,
};
use serde_json::{json, Value};
use uuid::Uuid;

use arcodash_core::entities::{api_key, group, user};
use arcodash_core::events::record_event;

use crate::auth::password::{hash_password, verify_password};
use crate::authz;
use crate::error::{is_unique_violation, ApiError};

/// Invitation tokens sit unread far longer than a password reset (1 hour), so they get a much
/// longer window.
pub const INVITATION_TOKEN_TTL_DAYS: i64 = 7;

/// Matches the frontend minimum (`PasswordField`); enforced server-side for `change_password`,
/// `reset_password` and `accept_invitation`.
const MIN_PASSWORD_LENGTH: usize = 8;

fn validate_password_length(password: &str) -> Result<(), ApiError> {
    if password.chars().count() < MIN_PASSWORD_LENGTH {
        return Err(ApiError::bad_request(format!(
            "password must be at least {MIN_PASSWORD_LENGTH} characters"
        )));
    }
    Ok(())
}

/// The `details` patch for a fresh invitation token + its expiry, shared by every issuance site.
fn invitation_token_patch(token: &str) -> Value {
    let expires_at = (Utc::now() + chrono::Duration::days(INVITATION_TOKEN_TTL_DAYS)).timestamp();
    json!({
        "invitation_token": token,
        "invitation_token_expires_at": expires_at,
    })
}

/// Regenerate an invitation token for a pending user, persist it, and record the resend event.
async fn regenerate_invitation_token(
    db: &impl ConnectionTrait,
    org_id: i64,
    caller: &user::Model,
    u: user::Model,
) -> Result<(user::Model, String), ApiError> {
    let token = Uuid::new_v4().to_string();
    arcodash_core::user_details::merge_user_details(db, u.id, invitation_token_patch(&token))
        .await?;
    let user = get(db, org_id, u.id).await?;

    record_event(
        db,
        org_id,
        Some(caller.id),
        "user.invitation_resent",
        Some("user"),
        Some(user.id),
        json!({ "email": user.email }),
    )
    .await?;

    Ok((user, token))
}

async fn user_groups(
    db: &impl ConnectionTrait,
    u: &user::Model,
) -> Result<Vec<group::Model>, ApiError> {
    authz::permissions::load_effective_groups(db, u)
        .await
        .map_err(Into::into)
}

/// Add/remove group ids on `users.groups` in one statement against the live row, so concurrent
/// membership edits compose instead of overwriting each other.
fn mutate_user_groups_stmt(
    user_id: i64,
    org_id: i64,
    add: &[i64],
    remove: &[i64],
) -> sea_orm::Statement {
    sea_orm::Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE users SET groups = ( \
             SELECT coalesce(array_agg(DISTINCT g ORDER BY g), '{}') \
             FROM unnest(groups || $1::bigint[]) AS g \
             WHERE g <> ALL($2::bigint[]) \
         ), updated_at = now() \
         WHERE id = $3 AND org_id = $4 \
         RETURNING groups",
        [
            add.to_vec().into(),
            remove.to_vec().into(),
            user_id.into(),
            org_id.into(),
        ],
    )
}

pub(crate) async fn mutate_user_groups(
    db: &impl ConnectionTrait,
    user_id: i64,
    org_id: i64,
    add: &[i64],
    remove: &[i64],
) -> Result<Vec<i64>, ApiError> {
    let row = db
        .query_one_raw(mutate_user_groups_stmt(user_id, org_id, add, remove))
        .await?
        .ok_or_else(|| ApiError::not_found("user not found"))?;
    Ok(row.try_get::<Vec<i64>>("", "groups")?)
}

/// `(added, removed)` turning `old` into `new`.
fn group_delta(new: &[i64], old: &[i64]) -> (Vec<i64>, Vec<i64>) {
    let missing_from = |a: &[i64], b: &[i64]| -> Vec<i64> {
        a.iter().filter(|g| !b.contains(g)).copied().collect()
    };
    (missing_from(new, old), missing_from(old, new))
}

async fn require_manage_users(
    db: &impl ConnectionTrait,
    caller: &user::Model,
    denied: &str,
) -> Result<(), ApiError> {
    let caller_groups = user_groups(db, caller).await?;
    if !authz::permissions::user_has_permission(caller, &caller_groups, "manage_users") {
        return Err(ApiError::forbidden(denied));
    }
    Ok(())
}

/// A user whose invitation is still pending; 400 once it has been accepted.
async fn get_pending(
    db: &impl ConnectionTrait,
    org_id: i64,
    id: i64,
) -> Result<user::Model, ApiError> {
    let u = get(db, org_id, id).await?;
    if !u.is_invitation_pending {
        return Err(ApiError::bad_request(
            "this user has already accepted their invitation",
        ));
    }
    Ok(u)
}

/// Org-scoped advisory lock serializing admin-count check-write-rechecks. Transaction-scoped
/// (`pg_advisory_xact_lock`), so it lives until the outer transaction ends even if `.begin()` only
/// issued a SAVEPOINT.
async fn lock_org_admin_set(db: &impl ConnectionTrait, org_id: i64) -> Result<(), ApiError> {
    db.execute_raw(sea_orm::Statement::from_sql_and_values(
        sea_orm::DbBackend::Postgres,
        "SELECT pg_advisory_xact_lock($1)",
        [org_id.into()],
    ))
    .await?;
    Ok(())
}

/// `require_admin_remains` for disabling user `id`; returns whether an admin existed before.
async fn require_admin_remains_without(
    db: &impl ConnectionTrait,
    org_id: i64,
    id: i64,
) -> Result<bool, ApiError> {
    let admin_groups = authz::permissions::admin_conferring_group_ids(db, org_id).await?;
    authz::permissions::require_admin_remains(db, org_id, &admin_groups, |candidate| {
        (candidate.id != id).then(|| candidate.groups.clone())
    })
    .await
}

pub async fn is_admin_user(db: &impl ConnectionTrait, u: &user::Model) -> Result<bool, ApiError> {
    let groups = user_groups(db, u).await?;
    Ok(authz::permissions::is_admin(&groups))
}

pub async fn list(
    db: &impl ConnectionTrait,
    org_id: i64,
    include_disabled: bool,
) -> Result<Vec<user::Model>, ApiError> {
    let mut q = user::Entity::find().filter(user::Column::OrgId.eq(org_id));
    if !include_disabled {
        q = q.filter(user::Column::DisabledAt.is_null());
    }
    Ok(q.all(db).await?)
}

pub async fn get(db: &impl ConnectionTrait, org_id: i64, id: i64) -> Result<user::Model, ApiError> {
    user::Entity::find_by_id(id)
        .filter(user::Column::OrgId.eq(org_id))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("user not found"))
}

/// `POST /api/users` — invite a new user (requires `manage_users`); returns the model so the
/// handler can email the link.
///
/// An email with a still-pending invitation gets a fresh token (the old one stops working); an
/// active user's email is rejected.
pub async fn invite(
    db: &impl ConnectionTrait,
    org_id: i64,
    caller: &user::Model,
    name: String,
    email: String,
    groups: Vec<i64>,
) -> Result<(user::Model, String), ApiError> {
    require_manage_users(
        db,
        caller,
        "the manage_users permission is required to invite users",
    )
    .await?;

    let existing = find_by_email_ci(db, org_id, &email).await?;
    if existing.as_ref().is_some_and(|e| !e.is_invitation_pending) {
        return Err(ApiError::bad_request(
            "a user with this email already exists",
        ));
    }

    // Every `groups` list is ceiling-checked before it's written, after the email-collision lookup
    // so an unsatisfiable invite reports the collision (400) rather than a grant-limit error (403).
    authz::permissions::require_can_grant_groups(db, caller, org_id, &groups).await?;

    if let Some(mut existing) = existing {
        // Empty `groups` means "no change requested", so a resend doesn't strip a pending user's
        // assignments.
        if !groups.is_empty() {
            // Add/remove delta against the live row via `mutate_user_groups`, not a whole-array
            // write of the caller's snapshot.
            let (added, removed) = group_delta(&groups, &existing.groups);
            existing.groups = mutate_user_groups(db, existing.id, org_id, &added, &removed).await?;
        }
        return regenerate_invitation_token(db, org_id, caller, existing).await;
    }

    let token = Uuid::new_v4().to_string();
    let details = invitation_token_patch(&token);
    let now = Utc::now();
    let api_key_hash = arcodash_core::hash::hash_api_key(&arcodash_core::hash::generate_api_key());

    let model = user::ActiveModel {
        org_id: Set(org_id),
        name: Set(name),
        email: Set(email),
        password_hash: Set(None),
        groups: Set(groups),
        api_key_hash: Set(api_key_hash),
        disabled_at: Set(None),
        is_invitation_pending: Set(true),
        profile_image_url: Set(None),
        details: Set(details),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
        ..Default::default()
    };

    let user = model.insert(db).await.map_err(|err| {
        if is_unique_violation(&err) {
            ApiError::bad_request("a user with this email already exists")
        } else {
            err.into()
        }
    })?;

    record_event(
        db,
        org_id,
        Some(caller.id),
        "user.invited",
        Some("user"),
        Some(user.id),
        json!({ "email": user.email }),
    )
    .await?;

    Ok((user, token))
}

/// `POST /api/users/:id/resend_invitation` — regenerate the token for a pending user.
///
/// Not covered by `ACCOUNT_RECOVERY_RATE_LIMIT_PER_MIN`, which protects anonymous endpoints;
/// callers here already hold `manage_users`.
pub async fn resend_invitation(
    db: &impl ConnectionTrait,
    org_id: i64,
    caller: &user::Model,
    id: i64,
) -> Result<(user::Model, String), ApiError> {
    require_manage_users(
        db,
        caller,
        "the manage_users permission is required to resend invitations",
    )
    .await?;
    let u = get_pending(db, org_id, id).await?;
    regenerate_invitation_token(db, org_id, caller, u).await
}

/// `DELETE /api/users/:id/invitation` — hard-delete a pending invitation (the row owns no data), so
/// a revoked invite can't be revived via `update(disabled: false)`.
pub async fn revoke_invitation(
    db: &impl ConnectionTrait,
    org_id: i64,
    caller: &user::Model,
    id: i64,
) -> Result<(), ApiError> {
    require_manage_users(
        db,
        caller,
        "the manage_users permission is required to revoke invitations",
    )
    .await?;
    let u = get_pending(db, org_id, id).await?;
    user::Entity::delete_by_id(u.id).exec(db).await?;

    record_event(
        db,
        org_id,
        Some(caller.id),
        "user.invitation_revoked",
        Some("user"),
        Some(id),
        json!({ "email": u.email }),
    )
    .await?;

    Ok(())
}

/// `GET /api/users/invitation/verify?token=...` — public; returns the user's email if valid.
pub async fn verify_invitation(
    db: &impl ConnectionTrait,
    token: &str,
) -> Result<user::Model, ApiError> {
    // JSONB path predicate pushes the token match into Postgres rather than scanning every pending
    // invitation in Rust. `DisabledAt.is_null()`: a disabled account's invitation is not
    // redeemable, and gets the same "invalid token" error as a bad token.
    let u = user::Entity::find()
        .filter(user::Column::IsInvitationPending.eq(true))
        .filter(user::Column::DisabledAt.is_null())
        .filter(Expr::cust_with_values(
            "details ->> 'invitation_token' = $1",
            [token],
        ))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::bad_request("invalid invitation token"))?;

    // A token with no recorded expiry is treated as expired.
    let expires_at = u.details["invitation_token_expires_at"]
        .as_i64()
        .unwrap_or(0);
    if Utc::now().timestamp() > expires_at {
        return Err(ApiError::bad_request_with_code(
            "invitation link has expired",
            "invitation_expired",
        ));
    }

    Ok(u)
}

/// `POST /api/users/invitation/accept` — set name + password, clear pending flag.
pub async fn accept_invitation(
    db: &impl ConnectionTrait,
    token: &str,
    name: String,
    password: String,
) -> Result<user::Model, ApiError> {
    let u = verify_invitation(db, token).await?;
    validate_password_length(&password)?;

    let hash = hash_password(&password)
        .await
        .map_err(|e| ApiError::internal(format!("password hash failed: {e}")))?;

    // Merged against the live row, not via `update_many`, so a concurrent `details` writer isn't
    // clobbered.
    arcodash_core::user_details::remove_user_detail_keys(db, u.id, &["invitation_token"]).await?;

    // `is_invitation_pending = true` is the compare half of a CAS: of two concurrent accepts that
    // both pass `verify_invitation`, only the first UPDATE matches.
    let affected = user::Entity::update_many()
        .col_expr(user::Column::Name, Expr::value(name))
        .col_expr(user::Column::PasswordHash, Expr::value(Some(hash)))
        .col_expr(user::Column::IsInvitationPending, Expr::value(false))
        .col_expr(user::Column::UpdatedAt, Expr::value(Utc::now()))
        .filter(user::Column::Id.eq(u.id))
        .filter(user::Column::IsInvitationPending.eq(true))
        .exec(db)
        .await?
        .rows_affected;
    if affected == 0 {
        return Err(ApiError::bad_request_with_code(
            "invitation link has already been used",
            "invitation_used",
        ));
    }

    get(db, u.org_id, u.id).await
}

/// `POST /api/forgot_password` — generate a reset token in `details.reset_token`. Always returns Ok
/// (no enumeration).
pub async fn forgot_password(
    db: &impl ConnectionTrait,
    org_id: i64,
    email: &str,
) -> Result<Option<(user::Model, String)>, ApiError> {
    let Some(u) = find_by_email_ci(db, org_id, email)
        .await?
        .filter(|u| u.disabled_at.is_none())
        // SSO-provisioned accounts have no password to reset; treat as missing so no reset link is
        // mailed.
        .filter(|u| u.password_hash.is_some())
    else {
        return Ok(None); // no enumeration — caller sends success either way
    };

    let token = Uuid::new_v4().to_string();
    let expires_at = (Utc::now() + chrono::Duration::hours(1)).timestamp();
    arcodash_core::user_details::merge_user_details(
        db,
        u.id,
        json!({ "reset_token": token, "reset_token_expires_at": expires_at }),
    )
    .await?;
    let updated = get(db, org_id, u.id).await?;

    Ok(Some((updated, token)))
}

/// Marks every dedicated `api_keys` row for a user inactive, so no old key authenticates after a
/// rotation.
async fn deactivate_api_keys(db: &impl ConnectionTrait, user_id: i64) -> Result<(), ApiError> {
    api_key::Entity::update_many()
        .col_expr(api_key::Column::Active, Expr::value(false))
        .filter(api_key::Column::ObjectType.eq(crate::auth::api_key::OBJECT_TYPE_USER))
        .filter(api_key::Column::ObjectId.eq(user_id))
        .exec(db)
        .await?;
    Ok(())
}

/// Retires every API key the account can authenticate with (dedicated rows plus
/// `users.api_key_hash`, returned so the caller writes it in the same update). A credential reset
/// assumes old credentials are compromised, and an API key carries no `session_token_version` to
/// fence it.
async fn revoke_api_keys(
    db: &impl ConnectionTrait,
    user: &user::Model,
) -> Result<String, ApiError> {
    deactivate_api_keys(db, user.id).await?;
    // Throwaway value that makes the legacy column stop matching; never handed to any caller.
    Ok(arcodash_core::hash::hash_api_key(
        &arcodash_core::hash::generate_api_key(),
    ))
}

/// `POST /api/reset_password` — validate the reset token and set a new password.
pub async fn reset_password(
    db: &impl ConnectionTrait,
    token: &str,
    new_password: &str,
) -> Result<(), ApiError> {
    // Same JSONB path predicate as `verify_invitation`.
    let u = user::Entity::find()
        .filter(user::Column::DisabledAt.is_null())
        .filter(Expr::cust_with_values(
            "details ->> 'reset_token' = $1",
            [token],
        ))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::bad_request("invalid or expired reset token"))?;

    let expires_at = u.details["reset_token_expires_at"].as_i64().unwrap_or(0);
    if Utc::now().timestamp() > expires_at {
        return Err(ApiError::bad_request("reset token has expired"));
    }
    if u.password_hash.is_none() {
        return Err(ApiError::bad_request(
            "this account signs in via SSO and has no password to reset",
        ));
    }
    validate_password_length(new_password)?;

    let hash = hash_password(new_password)
        .await
        .map_err(|e| ApiError::internal(format!("password hash failed: {e}")))?;

    arcodash_core::user_details::remove_user_detail_keys(
        db,
        u.id,
        &["reset_token", "reset_token_expires_at"],
    )
    .await?;

    // Bumping the version invalidates every session issued before this reset.
    let next_version = u.session_token_version + 1;
    let legacy_api_key_hash = revoke_api_keys(db, &u).await?;
    let (org_id, user_id) = (u.org_id, u.id);
    let mut active: user::ActiveModel = u.into();
    active.password_hash = Set(Some(hash));
    active.details = sea_orm::ActiveValue::NotSet;
    active.session_token_version = Set(next_version);
    active.api_key_hash = Set(legacy_api_key_hash);
    active.updated_at = Set(Utc::now().into());
    active.update(db).await?;

    record_event(
        db,
        org_id,
        None,
        "user.api_keys_revoked",
        Some("user"),
        Some(user_id),
        json!({}),
    )
    .await?;

    Ok(())
}

/// `POST /api/users/me/password` — change the caller's own password (requires the current one).
///
/// SSO users with no `password_hash` are rejected. Returns the updated row so the handler can
/// reissue a session cookie under the bumped `session_token_version` (which invalidates every OTHER
/// session). Does not revoke API keys: a routine change is not a compromise signal.
pub async fn change_password(
    db: &impl ConnectionTrait,
    caller: &user::Model,
    current_password: &str,
    new_password: &str,
) -> Result<user::Model, ApiError> {
    let Some(existing_hash) = caller.password_hash.as_deref() else {
        return Err(ApiError::bad_request(
            "this account signs in via SSO and has no password to change",
        ));
    };

    let matches = verify_password(current_password, existing_hash)
        .await
        .map_err(|e| ApiError::internal(format!("password verify failed: {e}")))?;
    if !matches {
        return Err(ApiError::unauthorized("current password is incorrect"));
    }
    validate_password_length(new_password)?;

    let hash = hash_password(new_password)
        .await
        .map_err(|e| ApiError::internal(format!("password hash failed: {e}")))?;

    let mut active: user::ActiveModel = caller.clone().into();
    active.password_hash = Set(Some(hash));
    active.session_token_version = Set(caller.session_token_version + 1);
    active.updated_at = Set(Utc::now().into());
    let updated = active.update(db).await?;

    record_event(
        db,
        caller.org_id,
        Some(caller.id),
        "user.password_changed",
        Some("user"),
        Some(caller.id),
        json!({}),
    )
    .await?;

    Ok(updated)
}

/// `POST /api/logout` — invalidates every session cookie issued before this call, including the
/// caller's. API keys are untouched.
pub async fn revoke_sessions(
    db: &impl ConnectionTrait,
    user: &user::Model,
) -> Result<(), ApiError> {
    let mut active: user::ActiveModel = user.clone().into();
    active.groups = sea_orm::ActiveValue::NotSet;
    active.session_token_version = Set(user.session_token_version + 1);
    active.updated_at = Set(Utc::now().into());
    active.update(db).await?;
    Ok(())
}

/// Writes `name`/`disabled_at`/`updated_at` for `update` on both its locked and unlocked paths;
/// `groups` is written separately via `mutate_user_groups`.
async fn write_update_fields(
    db: &impl ConnectionTrait,
    u: user::Model,
    name: Option<String>,
    disabled: Option<bool>,
) -> Result<user::Model, ApiError> {
    // Captured before `u` is moved: re-disabling keeps the original `disabled_at`.
    let existing_disabled_at = u.disabled_at;
    let mut active: user::ActiveModel = u.into();
    if let Some(n) = name {
        active.name = Set(n);
    }
    active.groups = sea_orm::ActiveValue::NotSet;
    if let Some(d) = disabled {
        active.disabled_at = Set(if d {
            existing_disabled_at.or_else(|| Some(Utc::now().into()))
        } else {
            None
        });
    }
    active.updated_at = Set(Utc::now().into());
    Ok(active.update(db).await?)
}

/// `POST /api/users/:id` — update user fields. `manage_users` holders can change any user; anyone
/// can update themselves (name only). Email is immutable here.
pub async fn update(
    db: &(impl ConnectionTrait + sea_orm::TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    org_id: i64,
    caller: &user::Model,
    id: i64,
    name: Option<String>,
    groups: Option<Vec<i64>>,
    disabled: Option<bool>,
) -> Result<user::Model, ApiError> {
    let caller_groups = user_groups(db, caller).await?;
    let can_manage =
        authz::permissions::user_has_permission(caller, &caller_groups, "manage_users");

    if !can_manage && caller.id != id {
        return Err(ApiError::forbidden("you can only update your own profile"));
    }

    if !can_manage && (groups.is_some() || disabled.is_some()) {
        return Err(ApiError::forbidden(
            "the manage_users permission is required to set groups or disabled",
        ));
    }

    if can_manage && disabled == Some(true) && caller.id == id {
        // Self-disable gets its own message before the general last-admin guard.
        return Err(ApiError::bad_request("cannot disable yourself"));
    }

    let field_changed = name.is_some() || (can_manage && groups.is_some());
    let disabled_change = if can_manage { disabled } else { None };

    // Only a groups change or a disable taking effect can shrink the admin count, so only those
    // take the advisory-lock + pre/post admin check path; every field is still written via
    // `write_update_fields`.
    let admin_risk = can_manage && (groups.is_some() || disabled == Some(true));

    if !admin_risk {
        let u = get(db, org_id, id).await?;
        let updated = write_update_fields(db, u, name, disabled).await?;
        record_update_events(
            db,
            org_id,
            caller,
            updated.id,
            disabled_change,
            field_changed.then(|| json!({})),
        )
        .await?;
        return Ok(updated);
    }

    // The check-write-recheck runs in a transaction holding an org-scoped advisory lock, so
    // concurrent writes on other users can't both see "an admin remains" under READ COMMITTED and
    // leave zero admins.
    let txn = db.begin().await?;
    lock_org_admin_set(&txn, org_id).await?;

    let u = get(&txn, org_id, id).await?;

    let mut pending_delta: Option<(Vec<i64>, Vec<i64>)> = None;
    let mut had_admin_before = false;
    // Captured before `u` is consumed so the `user.updated` audit event carries before/after group
    // lists.
    let groups_before = groups.as_ref().map(|_| u.groups.clone());
    if let Some(new_groups) = &groups {
        let (added, removed) = group_delta(new_groups, &u.groups);
        if !added.is_empty() {
            authz::permissions::require_can_grant_groups(&txn, caller, org_id, &added).await?;
        }
        if !removed.is_empty() {
            // Same ceiling as the `added` check, for removal: a non-admin `manage_users` holder
            // must not strip a permission by removing a user from the group that confers it.
            let removed_groups =
                authz::permissions::load_effective_groups_by_ids(&txn, org_id, &removed).await?;
            let revoked: Vec<String> = removed_groups
                .iter()
                .flat_map(|g| g.permissions.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            authz::permissions::require_can_revoke(&txn, caller, &revoked).await?;
        }
        pending_delta = Some((added, removed));

        let admin_groups = authz::permissions::admin_conferring_group_ids(&txn, org_id).await?;
        had_admin_before |=
            authz::permissions::require_admin_remains(&txn, org_id, &admin_groups, |candidate| {
                if candidate.id == u.id {
                    Some(new_groups.clone())
                } else {
                    Some(candidate.groups.clone())
                }
            })
            .await?;
    }
    if disabled == Some(true) {
        had_admin_before |= require_admin_remains_without(&txn, org_id, id).await?;
    }

    // `groups` is written separately via `mutate_user_groups`; `write_update_fields` excludes it so
    // this update can't reintroduce a lost update.
    let mut updated = write_update_fields(&txn, u, name, disabled).await?;

    if let Some((added, removed)) = pending_delta {
        updated.groups = mutate_user_groups(&txn, id, org_id, &added, &removed).await?;
    }

    // Compared against `had_admin_before` captured above; see `require_admin_survived`.
    authz::permissions::require_admin_survived(&txn, org_id, had_admin_before).await?;

    let details = match &groups_before {
        Some(before) => json!({ "before": before, "after": updated.groups }),
        None => json!({}),
    };
    record_update_events(
        &txn,
        org_id,
        caller,
        updated.id,
        disabled_change,
        field_changed.then_some(details),
    )
    .await?;

    txn.commit().await?;

    Ok(updated)
}

/// `update`'s audit events: `user.disabled`/`user.enabled` for a disable change, then `user.updated`
/// (with `updated_details`) when a field changed.
async fn record_update_events(
    db: &impl ConnectionTrait,
    org_id: i64,
    caller: &user::Model,
    user_id: i64,
    disabled_change: Option<bool>,
    updated_details: Option<Value>,
) -> Result<(), ApiError> {
    if let Some(d) = disabled_change {
        let action = if d { "user.disabled" } else { "user.enabled" };
        record_event(
            db,
            org_id,
            Some(caller.id),
            action,
            Some("user"),
            Some(user_id),
            json!({}),
        )
        .await?;
    }
    if let Some(details) = updated_details {
        record_event(
            db,
            org_id,
            Some(caller.id),
            "user.updated",
            Some("user"),
            Some(user_id),
            details,
        )
        .await?;
    }
    Ok(())
}

pub async fn disable(
    db: &(impl ConnectionTrait + sea_orm::TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    org_id: i64,
    caller: &user::Model,
    id: i64,
) -> Result<(), ApiError> {
    require_manage_users(
        db,
        caller,
        "the manage_users permission is required to disable users",
    )
    .await?;
    if caller.id == id {
        return Err(ApiError::bad_request("cannot disable yourself"));
    }

    let u = get(db, org_id, id).await?;

    // Check-write-recheck under the org-scoped advisory lock so concurrent disables can't both see
    // "an admin remains".
    let txn = db.begin().await?;
    lock_org_admin_set(&txn, org_id).await?;
    let had_admin_before = require_admin_remains_without(&txn, org_id, id).await?;

    let mut active: user::ActiveModel = u.into();
    active.disabled_at = Set(Some(Utc::now().into()));
    active.updated_at = Set(Utc::now().into());
    active.update(&txn).await?;

    // Compared against `had_admin_before` captured above; see `require_admin_survived`.
    authz::permissions::require_admin_survived(&txn, org_id, had_admin_before).await?;

    record_event(
        &txn,
        org_id,
        Some(caller.id),
        "user.disabled",
        Some("user"),
        Some(id),
        json!({}),
    )
    .await?;

    txn.commit().await?;

    Ok(())
}

pub async fn regenerate_api_key(
    db: &impl ConnectionTrait,
    org_id: i64,
    caller: &user::Model,
    id: i64,
) -> Result<String, ApiError> {
    let caller_groups = user_groups(db, caller).await?;
    let can_manage =
        authz::permissions::user_has_permission(caller, &caller_groups, "manage_users");

    if !can_manage && caller.id != id {
        return Err(ApiError::forbidden(
            "you can only regenerate your own API key",
        ));
    }

    let u = get(db, org_id, id).await?;
    if caller.id != id {
        let target_keys = authz::permissions::effective_permission_keys(db, &u).await?;
        authz::permissions::require_can_grant(db, caller, &target_keys).await?;
    }
    // Revoke any dedicated `api_keys` row so it can't outlive this rotation.
    deactivate_api_keys(db, id).await?;
    // Returned once; only its digest is persisted.
    let new_key = arcodash_core::hash::generate_api_key();
    let mut active: user::ActiveModel = u.into();
    active.api_key_hash = Set(arcodash_core::hash::hash_api_key(&new_key));
    active.updated_at = Set(Utc::now().into());
    active.update(db).await?;

    record_event(
        db,
        org_id,
        Some(caller.id),
        "user.api_key_regenerated",
        Some("user"),
        Some(id),
        json!({}),
    )
    .await?;

    Ok(new_key)
}

/// Find or create a user for an SSO login (SAML, LDAP, remote-user); existing users get
/// their name updated, new ones join the read-only "viewers" group.
pub async fn find_or_create_sso_user(
    db: &(impl ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    org_id: i64,
    email: &str,
    name: &str,
    profile_image_url: Option<&str>,
) -> Result<user::Model, ApiError> {
    find_or_create_sso_user_with_groups(db, org_id, email, name, profile_image_url, None).await
}

/// Resolve `IdP` group names to this org's `groups.id`s. Unknown names are skipped (fail-closed: they
/// grant no access) and only matched rows yield ids.
async fn resolve_group_ids_by_name(
    db: &impl ConnectionTrait,
    org_id: i64,
    names: &[String],
) -> Result<Vec<i64>, ApiError> {
    if names.is_empty() {
        return Ok(vec![]);
    }
    let groups = group::Entity::find()
        .filter(group::Column::OrgId.eq(org_id))
        .filter(group::Column::Name.is_in(names.to_vec()))
        .all(db)
        .await?;
    // An IdP-asserted group must never confer admin; filtered on actual conferral, not `type ==
    // "builtin"`.
    let admin_groups = authz::permissions::admin_conferring_group_ids(db, org_id).await?;
    Ok(groups
        .into_iter()
        .filter_map(|g| {
            if admin_groups.contains(&g.id) {
                tracing::warn!(
                    org_id,
                    group_name = %g.name,
                    "IdP-asserted group confers admin; dropped from SSO group mapping"
                );
                return None;
            }
            Some(g.id)
        })
        .collect())
}

/// The builtin group `name`'s id (empty if missing). Builtin renaming is blocked and names are
/// unique per org; the type filter is defense in depth.
async fn builtin_group_ids(
    db: &impl ConnectionTrait,
    org_id: i64,
    name: &str,
) -> Result<Vec<i64>, ApiError> {
    Ok(group::Entity::find()
        .filter(group::Column::OrgId.eq(org_id))
        .filter(group::Column::Name.eq(name))
        .filter(group::Column::Type.eq("builtin"))
        .one(db)
        .await?
        .map(|g| vec![g.id])
        .unwrap_or_default())
}

/// The builtin "default" group's id, used only to preserve an existing user's re-sync when `IdP`
/// names match nothing.
async fn default_group_ids(db: &impl ConnectionTrait, org_id: i64) -> Result<Vec<i64>, ApiError> {
    builtin_group_ids(db, org_id, "default").await
}

/// The builtin "viewers" group's id (empty if missing, which means zero permissions). The fallback
/// for a brand-new SSO user: read-only until an admin grants more.
async fn provisioning_fallback_group_ids(
    db: &impl ConnectionTrait,
    org_id: i64,
) -> Result<Vec<i64>, ApiError> {
    builtin_group_ids(db, org_id, "viewers").await
}

/// Whether an existing SSO user's row has anything worth writing; a pure predicate so the no-op
/// write on every SSO request is testable without a database.
fn sso_user_row_needs_write(
    existing_is_invitation_pending: bool,
    existing_name: &str,
    new_name: &str,
    existing_profile_image_url: Option<&str>,
    new_profile_image_url: Option<&str>,
) -> bool {
    existing_is_invitation_pending
        || existing_name != new_name
        || new_profile_image_url.is_some_and(|img| existing_profile_image_url != Some(img))
}

/// Like [`find_or_create_sso_user`], plus `IdP` group-name mapping.
///
/// New users with unmatched/absent names fall back to "viewers"; existing users to "default"; a
/// non-empty `group_names` re-syncs groups on every login.
pub async fn find_or_create_sso_user_with_groups(
    db: &(impl ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    org_id: i64,
    email: &str,
    name: &str,
    profile_image_url: Option<&str>,
    group_names: Option<Vec<String>>,
) -> Result<user::Model, ApiError> {
    // Case-insensitive (matches the `(org_id, lower(email))` unique index). Includes disabled users
    // so login can be rejected rather than provisioning a duplicate.
    let existing = find_by_email_ci(db, org_id, email).await?;

    // A mapping miss must never strip a user of all membership. `None` means the IdP asserted
    // nothing, so leave existing groups alone.
    let mapped_groups = match &group_names {
        Some(names) if !names.is_empty() => {
            let mut ids = resolve_group_ids_by_name(db, org_id, names).await?;
            if ids.is_empty() {
                ids = if existing.is_some() {
                    default_group_ids(db, org_id).await?
                } else {
                    provisioning_fallback_group_ids(db, org_id).await?
                };
            }
            Some(ids)
        }
        _ => None,
    };

    if let Some(existing) = existing {
        if existing.disabled_at.is_some() {
            return Err(ApiError::forbidden("account is disabled"));
        }

        // Only rewrite the row when something IdP-observable changed, not on every SSO-resolved
        // request.
        let needs_write = sso_user_row_needs_write(
            existing.is_invitation_pending,
            &existing.name,
            name,
            existing.profile_image_url.as_deref(),
            profile_image_url,
        );
        let mut sso_group_delta: Option<(Vec<i64>, Vec<i64>)> = None;
        if let Some(idp_groups) = mapped_groups {
            // The IdP asserts non-admin membership only; re-syncing keeps any admin-conferring
            // group already on the account.
            let admin_groups = authz::permissions::admin_conferring_group_ids(db, org_id).await?;
            let mut new_groups = idp_groups;
            for g in existing.groups.iter().filter(|g| admin_groups.contains(g)) {
                if !new_groups.contains(g) {
                    new_groups.push(*g);
                }
            }

            let before: HashSet<i64> = existing.groups.iter().copied().collect();
            let after: HashSet<i64> = new_groups.iter().copied().collect();
            if before != after {
                tracing::info!(
                    org_id,
                    user_id = existing.id,
                    before = ?existing.groups,
                    after = ?new_groups,
                    "SSO login re-synced user groups"
                );
                sso_group_delta = Some(group_delta(&new_groups, &existing.groups));
            }
        }
        let mut updated = if needs_write {
            let mut active: user::ActiveModel = existing.clone().into();
            active.name = Set(name.to_string());
            if let Some(img) = profile_image_url {
                active.profile_image_url = Set(Some(img.to_string()));
            }
            // Written separately via `mutate_user_groups`, as in `update`.
            active.groups = sea_orm::ActiveValue::NotSet;
            // A successful SSO login proves identity, so clear the pending flag.
            active.is_invitation_pending = Set(false);
            active.updated_at = Set(Utc::now().into());
            active.update(db).await?
        } else {
            existing.clone()
        };
        if let Some((added, removed)) = sso_group_delta {
            // Recorded after the write lands, from its real result, so a failed write never leaves
            // a false audit row.
            updated.groups = mutate_user_groups(db, existing.id, org_id, &added, &removed).await?;
            record_event(
                db,
                org_id,
                None,
                "user.groups_synced",
                Some("user"),
                Some(existing.id),
                json!({ "before": existing.groups, "after": updated.groups }),
            )
            .await?;
        }
        return Ok(updated);
    }

    // New SSO user: IdP-mapped groups, else the read-only fallback.
    let groups = match mapped_groups {
        Some(groups) => groups,
        None => provisioning_fallback_group_ids(db, org_id).await?,
    };
    let api_key_hash = arcodash_core::hash::hash_api_key(&arcodash_core::hash::generate_api_key());
    let now = Utc::now();

    let model = user::ActiveModel {
        org_id: Set(org_id),
        name: Set(name.to_string()),
        email: Set(email.to_string()),
        password_hash: Set(None),
        groups: Set(groups),
        api_key_hash: Set(api_key_hash),
        disabled_at: Set(None),
        is_invitation_pending: Set(false),
        profile_image_url: Set(profile_image_url.map(String::from)),
        details: Set(json!({})),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
        ..Default::default()
    };

    match crate::db::insert_scoped(db, model).await {
        Ok(user) => Ok(user),
        Err(err) if is_unique_violation(&err) => {
            // A concurrent login inserted first; return their row.
            find_by_email_ci(db, org_id, email)
                .await?
                .ok_or_else(|| ApiError::internal("SSO user insert conflicted but no row found"))
        }
        Err(err) => Err(err.into()),
    }
}

/// Look up a user by `(org_id, lower(email))`, including disabled users.
pub(crate) async fn find_by_email_ci(
    db: &impl ConnectionTrait,
    org_id: i64,
    email: &str,
) -> Result<Option<user::Model>, ApiError> {
    Ok(user::Entity::find()
        .filter(user::Column::OrgId.eq(org_id))
        .filter(Expr::cust_with_values("lower(email) = lower($1)", [email]))
        .one(db)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Confirms a single `UPDATE ... RETURNING groups` statement is sent, never a `SELECT` first.
    #[test]
    fn mutate_user_groups_stmt_is_one_statement_no_select_first() {
        let stmt = mutate_user_groups_stmt(7, 1, &[3], &[4]);
        let sql = stmt.sql.to_lowercase();
        assert!(sql.starts_with("update users"), "sql: {sql}");
        assert!(sql.contains("returning groups"), "sql: {sql}");
        assert!(
            !sql.contains("select id"),
            "must not read-then-write: {sql}"
        );
    }

    #[test]
    fn invitation_token_patch_carries_token_and_a_future_expiry() {
        let patch = invitation_token_patch("tok-123");
        assert_eq!(patch["invitation_token"], "tok-123");
        let expires_at = patch["invitation_token_expires_at"].as_i64().unwrap();
        assert!(expires_at > Utc::now().timestamp());
    }

    #[test]
    fn sso_row_write_is_skipped_when_nothing_changed() {
        assert!(!sso_user_row_needs_write(
            false,
            "Alice",
            "Alice",
            Some("https://img/a.png"),
            Some("https://img/a.png")
        ));
        assert!(!sso_user_row_needs_write(
            false,
            "Alice",
            "Alice",
            Some("https://img/a.png"),
            None
        ));
    }

    #[test]
    fn sso_row_write_fires_on_a_still_pending_invitation() {
        assert!(sso_user_row_needs_write(true, "Alice", "Alice", None, None));
    }

    #[test]
    fn sso_row_write_fires_on_a_changed_name() {
        assert!(sso_user_row_needs_write(
            false, "Alice", "Alicia", None, None
        ));
    }

    #[test]
    fn sso_row_write_fires_on_a_changed_profile_image() {
        assert!(sso_user_row_needs_write(
            false,
            "Alice",
            "Alice",
            Some("https://img/old.png"),
            Some("https://img/new.png")
        ));
    }
}

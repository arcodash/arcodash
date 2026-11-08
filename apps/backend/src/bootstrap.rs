//! First-admin bootstrap: if `ADMIN_EMAIL`/`ADMIN_PASSWORD` are both set and no user with that
//! email exists yet in the default org, create that user and put them in the `admin` group.
//!
//! Safe to run on every startup (no-op once the user exists, no-op if the vars are unset) — these
//! are dev/ops-supplied env vars, never hardcoded credentials.

use anyhow::{Context, Result};
use sea_orm::sea_query::Expr;
use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use arcodash_core::entities::{group, organization, user};

use crate::auth::password::hash_password;
use crate::auth::DEFAULT_ORG_SLUG;
use crate::error::is_unique_violation;

/// Run the first-admin bootstrap against the default org. Logs whether it created the admin
/// user, found it already existed, or skipped because the env vars weren't set.
pub async fn bootstrap_admin_user(db: &DatabaseConnection) -> Result<()> {
    let creds = std::env::var("ADMIN_EMAIL")
        .ok()
        .zip(std::env::var("ADMIN_PASSWORD").ok())
        .map(|(e, p)| {
            (
                e.trim().trim_matches(|c| c == '"' || c == '\'').to_string(),
                p.trim_matches(|c| c == '\r' || c == '\n' || c == '"' || c == '\'')
                    .to_string(),
            )
        })
        .filter(|(e, p)| !e.is_empty() && !p.is_empty());
    let Some((email, password)) = creds else {
        tracing::debug!("ADMIN_EMAIL/ADMIN_PASSWORD not set, skipping first-admin bootstrap");
        return Ok(());
    };

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq(DEFAULT_ORG_SLUG))
        .one(db)
        .await
        .context("failed to look up default organization for admin bootstrap")?
        .context("default organization not found — has the migrator run?")?;

    // Case-insensitive: matches the DB's `(org_id, lower(email))` unique index, so a casing
    // change in the env var between deploys still finds the existing row instead of racing it
    // into a unique-violation INSERT.
    let existing = user::Entity::find()
        .filter(user::Column::OrgId.eq(org.id))
        .filter(Expr::cust_with_values(
            "lower(email) = lower($1)",
            [email.clone()],
        ))
        .one(db)
        .await
        .context("failed to check for existing admin user")?;

    if existing.is_some() {
        tracing::debug!(%email, "admin bootstrap user already exists, skipping");
        return Ok(());
    }

    let admin_group = group::Entity::find()
        .filter(group::Column::OrgId.eq(org.id))
        .filter(group::Column::Name.eq("admin"))
        .one(db)
        .await
        .context("failed to look up the builtin admin group")?
        .context("builtin 'admin' group not found — has the migrator run?")?;

    let password_hash = hash_password(&password)
        .await
        .context("failed to hash admin password")?;

    let new_user = user::ActiveModel {
        org_id: Set(org.id),
        name: Set("Admin".to_string()),
        email: Set(email.clone()),
        password_hash: Set(Some(password_hash)),
        groups: Set(vec![admin_group.id]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(
            &arcodash_core::hash::generate_api_key(),
        )),
        disabled_at: Set(None),
        is_invitation_pending: Set(false),
        profile_image_url: Set(None),
        details: Set(serde_json::json!({})),
        digest_enabled: Set(false),
        digest_unsubscribe_token: Set(None),
        ..Default::default()
    };

    // `insert_scoped` (a nested `SAVEPOINT`) rather than a bare insert: two replicas booting
    // concurrently both pass the `existing.is_some()` check above and race to create the same
    // row. Without the savepoint the loser's unique-violation would abort the whole connection's
    // transaction on a composition binary; here it just means the other replica won.
    match crate::db::insert_scoped(db, new_user).await {
        Ok(_) => {
            tracing::info!(%email, "created first-admin bootstrap user in the 'admin' group");
            Ok(())
        }
        Err(e) if is_unique_violation(&e) => {
            tracing::info!(%email, "admin bootstrap user created concurrently, skipping");
            Ok(())
        }
        Err(e) => Err(e).context("failed to insert admin bootstrap user"),
    }
}

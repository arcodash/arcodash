//! One-shot offline re-encryption utility for rotating `ENCRYPTION_KEY`. There is no automatic
//! rotation — this walks every row across the four things the key protects and re-encrypts each
//! under the new key: `data_sources.encrypted_options`, `notification_destinations.options`,
//! `organizations.settings` -> `ai.gemini_api_key_encrypted` / `ai.providers.<kind>.key_encrypted`,
//! and `org_embed_settings.embedding_secret`.
//!
//! Runs inside a single DB transaction — a row that decrypts under neither key aborts the whole
//! rotation (rolled back, nothing partially rotated) rather than silently skipping it.
//! `secrets::decrypt_with_either` tries the new key first, so re-running after an aborted or
//! interrupted attempt is safe: rows already rotated on a prior pass decrypt immediately under
//! the new key alone.
//!
//! Usage: `OLD_ENCRYPTION_KEY=<64 hex> NEW_ENCRYPTION_KEY=<64 hex> DATABASE_URL=<url> cargo run
//! --bin rotate_encryption_key` — `DATABASE_URL` is used exactly as set, no rewriting (run it
//! from inside the docker network, or against a port-forwarded/tunneled host, same as any other
//! admin tool connecting straight to the database). Intended to run with the app stopped —
//! nothing here coordinates with a concurrently-running backend/worker reading these columns
//! under the old key mid-rotation.

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use sea_orm::{
    ActiveModelBehavior, ActiveModelTrait, ActiveValue::Set, Database, DatabaseTransaction,
    EntityTrait, IntoActiveModel, TransactionTrait,
};
use serde_json::Value;

use arcodash_core::entities::{
    data_source, notification_destination, org_embed_settings, organization,
};
use arcodash_core::secrets;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let old_key = load_key("OLD_ENCRYPTION_KEY")?;
    let new_key = load_key("NEW_ENCRYPTION_KEY")?;
    if old_key == new_key {
        println!("OLD_ENCRYPTION_KEY and NEW_ENCRYPTION_KEY are identical — nothing to rotate.");
        return Ok(());
    }

    let db_url = std::env::var("DATABASE_URL").context("DATABASE_URL not set")?;
    let db = Database::connect(&db_url)
        .await
        .context("failed to connect to database")?;

    let txn = db.begin().await.context("begin rotation transaction")?;

    let data_sources_rotated = rotate_column::<data_source::Entity, _>(
        &txn,
        &new_key,
        &old_key,
        "data_sources.id",
        |r| r.id,
        |r| r.encrypted_options.clone(),
        |a, ct| a.encrypted_options = Set(Some(ct)),
    )
    .await?;
    let destinations_rotated = rotate_column::<notification_destination::Entity, _>(
        &txn,
        &new_key,
        &old_key,
        "notification_destinations.id",
        |r| r.id,
        |r| r.options.clone(),
        |a, ct| a.options = Set(Some(ct)),
    )
    .await?;
    let (orgs_touched, ai_fields_rotated) = rotate_organizations(&txn, &new_key, &old_key).await?;
    let embed_rotated = rotate_column::<org_embed_settings::Entity, _>(
        &txn,
        &new_key,
        &old_key,
        "org_embed_settings.org_id",
        |r| r.org_id,
        |r| r.embedding_secret.clone(),
        |a, ct| a.embedding_secret = Set(Some(ct)),
    )
    .await?;

    txn.commit().await.context("commit rotation")?;

    println!("Rotation complete:");
    println!("  data_sources.encrypted_options rows rotated:        {data_sources_rotated}");
    println!("  notification_destinations.options rows rotated:     {destinations_rotated}");
    println!(
        "  organizations touched / ai key fields rotated:      {orgs_touched} / {ai_fields_rotated}"
    );
    println!("  org_embed_settings.embedding_secret rows rotated:    {embed_rotated}");
    Ok(())
}

fn load_key(var: &str) -> Result<[u8; 32]> {
    let hex = std::env::var(var).with_context(|| format!("{var} is required but was not set"))?;
    secrets::parse_key_hex(&hex).with_context(|| format!("{var} is not a valid encryption key"))
}

/// Decrypt `ct` under either key, re-encrypt under `new_key`. Named per-row in every caller's
/// error context so a decrypt-under-neither-key failure names exactly which row to investigate.
fn rotate_bytes(ct: &[u8], new_key: &[u8; 32], old_key: &[u8; 32]) -> Result<Vec<u8>> {
    let plaintext = secrets::decrypt_with_either(new_key, old_key, ct)
        .context("decrypts under neither the old nor the new key")?;
    secrets::encrypt(new_key, &plaintext).context("re-encrypt under new key")
}

/// Re-encrypts one nullable bytea column on every row of `E`. `key_label` (`"<table>.<key>"`)
/// names the table in error context and, with `row_key`, the exact row a failure is about.
async fn rotate_column<E, A>(
    txn: &DatabaseTransaction,
    new_key: &[u8; 32],
    old_key: &[u8; 32],
    key_label: &str,
    row_key: fn(&E::Model) -> i64,
    column: fn(&E::Model) -> Option<Vec<u8>>,
    set_column: fn(&mut A, Vec<u8>),
) -> Result<usize>
where
    E: EntityTrait<ActiveModel = A>,
    A: ActiveModelTrait<Entity = E> + ActiveModelBehavior + Send,
    E::Model: IntoActiveModel<A>,
{
    let table = key_label.split('.').next().unwrap_or(key_label);
    let rows = E::find()
        .all(txn)
        .await
        .with_context(|| format!("load {table}"))?;
    let mut rotated = 0;
    for row in rows {
        let Some(ct) = column(&row) else {
            continue;
        };
        let id = row_key(&row);
        let new_ct =
            rotate_bytes(&ct, new_key, old_key).with_context(|| format!("{key_label}={id}"))?;
        let mut active = row.into_active_model();
        set_column(&mut active, new_ct);
        active
            .update(txn)
            .await
            .with_context(|| format!("update {key_label}={id}"))?;
        rotated += 1;
    }
    Ok(rotated)
}

/// `settings.ai.gemini_api_key_encrypted` (legacy flat) and
/// `settings.ai.providers.<kind>.key_encrypted` are base64 text embedded in the JSONB `settings`
/// column, not a raw bytea column like the other three tables — matches
/// `api::handlers::ai_settings`'s own storage shape.
async fn rotate_organizations(
    txn: &DatabaseTransaction,
    new_key: &[u8; 32],
    old_key: &[u8; 32],
) -> Result<(usize, usize)> {
    let rows = organization::Entity::find()
        .all(txn)
        .await
        .context("load organizations")?;
    let (mut orgs_touched, mut fields_rotated) = (0, 0);
    for row in rows {
        let touched = rotate_and_merge_organization(txn, row, new_key, old_key).await?;
        orgs_touched += usize::from(touched > 0);
        fields_rotated += touched;
    }
    Ok((orgs_touched, fields_rotated))
}

/// Rotate one already-loaded organization row's AI key fields and merge-write the result. Split out
/// of `rotate_organizations` so a caller can hold `row` across an intervening write (e.g. a test
/// simulating a concurrent settings writer) — the merge write must pick up whatever landed on the
/// row since `row` was loaded, not just what `row` itself carries.
async fn rotate_and_merge_organization(
    txn: &DatabaseTransaction,
    row: organization::Model,
    new_key: &[u8; 32],
    old_key: &[u8; 32],
) -> Result<usize> {
    let mut settings = row.settings.clone();
    let touched = rotate_ai_key_fields(&mut settings, new_key, old_key, row.id)?;
    if touched > 0 {
        let rotated_ai = settings
            .get("ai")
            .cloned()
            .context("ai settings vanished mid-rotation")?;
        // A merge, not a column write: an operator rotation must not silently revert
        // whatever else a concurrent settings writer (a tenant admin, a background job) has
        // touched since this row was loaded — only the `ai` sub-object this pass rewrote
        // goes back.
        arcodash_core::org_settings::merge_org_settings(
            txn,
            row.id,
            serde_json::json!({ "ai": rotated_ai }),
        )
        .await
        .with_context(|| format!("update organizations.id={}", row.id))?;
    }
    Ok(touched)
}

fn rotate_ai_key_fields(
    settings: &mut Value,
    new_key: &[u8; 32],
    old_key: &[u8; 32],
    org_id: i64,
) -> Result<usize> {
    let Some(ai) = settings.get_mut("ai").and_then(Value::as_object_mut) else {
        return Ok(0);
    };
    let mut touched = 0;

    if let Some(Value::String(ct)) = ai.get_mut("gemini_api_key_encrypted") {
        *ct = rotate_base64_field(ct, new_key, old_key)
            .with_context(|| format!("organizations.id={org_id} ai.gemini_api_key_encrypted"))?;
        touched += 1;
    }

    if let Some(providers) = ai.get_mut("providers").and_then(Value::as_object_mut) {
        for (kind, provider) in providers.iter_mut() {
            if let Some(Value::String(ct)) = provider.get_mut("key_encrypted") {
                *ct = rotate_base64_field(ct, new_key, old_key).with_context(|| {
                    format!("organizations.id={org_id} ai.providers.{kind}.key_encrypted")
                })?;
                touched += 1;
            }
        }
    }

    Ok(touched)
}

fn rotate_base64_field(ct_b64: &str, new_key: &[u8; 32], old_key: &[u8; 32]) -> Result<String> {
    let ct = BASE64
        .decode(ct_b64)
        .context("stored value is not valid base64")?;
    let rotated = rotate_bytes(&ct, new_key, old_key)?;
    Ok(BASE64.encode(rotated))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    #[test]
    fn rotate_base64_field_round_trips_under_the_new_key() {
        let old_key = key(1);
        let new_key = key(2);
        let plaintext = b"sk-some-provider-key";
        let stored = BASE64.encode(secrets::encrypt(&old_key, plaintext).unwrap());

        let rotated_b64 = rotate_base64_field(&stored, &new_key, &old_key).unwrap();
        let rotated_bytes = BASE64.decode(&rotated_b64).unwrap();
        assert_eq!(
            secrets::decrypt(&new_key, &rotated_bytes).unwrap(),
            plaintext
        );
        // Old key must no longer work — it really was re-encrypted, not just copied.
        assert!(secrets::decrypt(&old_key, &rotated_bytes).is_err());
    }

    #[test]
    fn rotate_ai_key_fields_migrates_legacy_flat_and_nested_provider_keys() {
        let old_key = key(1);
        let new_key = key(2);
        let legacy_ct = BASE64.encode(secrets::encrypt(&old_key, b"legacy-gemini-key").unwrap());
        let gemini_ct = BASE64.encode(secrets::encrypt(&old_key, b"nested-gemini-key").unwrap());
        let mut settings = serde_json::json!({
            "ai": {
                "gemini_api_key_encrypted": legacy_ct,
                "providers": {
                    "gemini": { "key_encrypted": gemini_ct, "model": "gemini-3.1-flash-lite" }
                }
            }
        });

        let touched = rotate_ai_key_fields(&mut settings, &new_key, &old_key, 1).unwrap();
        assert_eq!(touched, 2);

        let legacy_rotated = settings["ai"]["gemini_api_key_encrypted"].as_str().unwrap();
        let legacy_bytes = BASE64.decode(legacy_rotated).unwrap();
        assert_eq!(
            secrets::decrypt(&new_key, &legacy_bytes).unwrap(),
            b"legacy-gemini-key"
        );

        let nested_rotated = settings["ai"]["providers"]["gemini"]["key_encrypted"]
            .as_str()
            .unwrap();
        let nested_bytes = BASE64.decode(nested_rotated).unwrap();
        assert_eq!(
            secrets::decrypt(&new_key, &nested_bytes).unwrap(),
            b"nested-gemini-key"
        );
        // A field the rotation doesn't touch must survive untouched.
        assert_eq!(
            settings["ai"]["providers"]["gemini"]["model"],
            "gemini-3.1-flash-lite"
        );
    }

    #[test]
    fn rotate_ai_key_fields_is_a_no_op_when_there_is_no_ai_settings_object() {
        let mut settings = serde_json::json!({});
        let touched = rotate_ai_key_fields(&mut settings, &key(2), &key(1), 1).unwrap();
        assert_eq!(touched, 0);
        assert_eq!(settings, serde_json::json!({}));
    }

    #[test]
    fn rotate_bytes_errors_when_neither_key_decrypts() {
        let ct = secrets::encrypt(&key(9), b"data").unwrap();
        assert!(rotate_bytes(&ct, &key(2), &key(1)).is_err());
    }

    /// A real interleave, not an in-memory snapshot: loads a row exactly like
    /// `rotate_organizations` does, then — before rotation writes anything back — commits a second,
    /// independent `merge_org_settings` write against the same row, standing in for a concurrent
    /// tenant-admin or background settings edit landing in that window.
    /// `rotate_and_merge_organization` is then called with the *stale* row loaded before that
    /// write. Its merge must still pick up the sibling key; a whole-column write (`active.settings
    /// = Set(settings)`) would silently revert it, since that code path never re-reads the row
    /// before writing.
    #[tokio::test]
    async fn rotate_and_merge_organization_does_not_revert_a_write_that_lands_between_load_and_write(
    ) {
        let Ok(db_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let db = Database::connect(&db_url).await.unwrap();
        let txn = db.begin().await.unwrap();

        let old_key = key(1);
        let new_key = key(2);
        let ai_ct_b64 = BASE64.encode(secrets::encrypt(&old_key, b"sk-interleave-secret").unwrap());
        let org = organization::ActiveModel {
            name: Set("rotate-merge-interleave".to_string()),
            slug: Set(format!(
                "rotate-merge-interleave-{}",
                uuid::Uuid::new_v4().simple()
            )),
            settings: Set(serde_json::json!({
                "ai": { "providers": { "gemini": { "key_encrypted": ai_ct_b64 } } }
            })),
            ..Default::default()
        }
        .insert(&txn)
        .await
        .unwrap();

        // The read `rotate_organizations` performs before ever calling into this function.
        let row = organization::Entity::find_by_id(org.id)
            .one(&txn)
            .await
            .unwrap()
            .unwrap();

        // The concurrent write, landing in the window between that read and the call below.
        arcodash_core::org_settings::merge_org_settings(
            &txn,
            org.id,
            serde_json::json!({ "extra": { "status": "active", "plan": "team" } }),
        )
        .await
        .unwrap();

        let touched = rotate_and_merge_organization(&txn, row, &new_key, &old_key)
            .await
            .unwrap();
        assert_eq!(touched, 1);

        let after = organization::Entity::find_by_id(org.id)
            .one(&txn)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after.settings["extra"],
            serde_json::json!({ "status": "active", "plan": "team" }),
            "a settings write landing between load and write must survive the merge, got: {}",
            after.settings
        );
        let ai_ct_after = BASE64
            .decode(
                after.settings["ai"]["providers"]["gemini"]["key_encrypted"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            secrets::decrypt(&new_key, &ai_ct_after).unwrap(),
            b"sk-interleave-secret"
        );

        txn.rollback().await.unwrap();
    }
}

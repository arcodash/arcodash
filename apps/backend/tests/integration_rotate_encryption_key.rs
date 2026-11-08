//! Real (not mocked) exercise of the `rotate_encryption_key` binary: seeds one encrypted row in
//! each of the four places `ENCRYPTION_KEY` protects, actually runs the compiled binary as a
//! subprocess against the test database with OLD/NEW keys, then confirms every seeded row now
//! decrypts under the new key and no longer under the old one.

mod common;
use common::spawn_app;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter};

use arcodash_core::entities::{
    data_source, notification_destination, org_embed_settings, organization,
};
use arcodash_core::secrets;

/// Matches `common::spawn_app`'s hardcoded `encryption_key` — the key everything in this test DB
/// is actually encrypted under before rotation runs.
const OLD_KEY_HEX: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW_KEY_HEX: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[tokio::test]
async fn rotate_encryption_key_binary_re_encrypts_every_row_under_the_new_key() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let old_key = secrets::parse_key_hex(OLD_KEY_HEX).unwrap();
    let new_key = secrets::parse_key_hex(NEW_KEY_HEX).unwrap();

    let org_id = {
        use sea_orm::EntityTrait as _;
        organization::Entity::find()
            .filter(organization::Column::Slug.eq("default"))
            .one(&app.db)
            .await
            .unwrap()
            .unwrap()
            .id
    };

    // data_sources.encrypted_options
    let now = chrono::Utc::now().into();
    let ds_ct = secrets::encrypt(&old_key, br#"{"host":"secret-db-host"}"#).unwrap();
    let ds = data_source::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set("rotate-test-ds".to_string()),
        r#type: Set("pg".to_string()),
        encrypted_options: Set(Some(ds_ct)),
        health_state: Set("unknown".to_string()),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .expect("insert data_source");

    // notification_destinations.options
    let dest_ct = secrets::encrypt(&old_key, br#"{"url":"https://hooks.example/secret"}"#).unwrap();
    let dest = notification_destination::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set("rotate-test-dest".to_string()),
        r#type: Set("webhook".to_string()),
        options: Set(Some(dest_ct)),
        user_id: Set(None),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(&app.db)
    .await
    .expect("insert notification_destination");

    // org_embed_settings.embedding_secret
    let embed_ct = secrets::encrypt(&old_key, b"hmac-secret-bytes").unwrap();
    org_embed_settings::ActiveModel {
        org_id: Set(org_id),
        public_urls_enabled: Set(true),
        embedding_secret: Set(Some(embed_ct)),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(&app.db)
    .await
    .expect("insert org_embed_settings");

    // organizations.settings -> ai.providers.gemini.key_encrypted (base64 in JSONB)
    let ai_ct_b64 = BASE64.encode(secrets::encrypt(&old_key, b"sk-gemini-secret").unwrap());
    let settings = serde_json::json!({
        "ai": { "providers": { "gemini": { "key_encrypted": ai_ct_b64, "model": "gemini-3.1-flash-lite" } } }
    });
    let org_row = organization::Entity::find_by_id(org_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut org_active: organization::ActiveModel = org_row.into();
    org_active.settings = Set(settings);
    org_active
        .update(&app.db)
        .await
        .expect("update organization settings");

    // Actually run the compiled binary — real subprocess, real DB connection, real commit.
    let db_url = std::env::var("TEST_DATABASE_URL").unwrap();
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_rotate_encryption_key"))
        .env("DATABASE_URL", &db_url)
        .env("OLD_ENCRYPTION_KEY", OLD_KEY_HEX)
        .env("NEW_ENCRYPTION_KEY", NEW_KEY_HEX)
        .status()
        .expect("run rotate_encryption_key binary");
    assert!(status.success(), "rotate_encryption_key exited non-zero");

    // data_source: decrypts under new key, not under old.
    let ds_after = data_source::Entity::find_by_id(ds.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let ds_ct_after = ds_after.encrypted_options.unwrap();
    assert_eq!(
        secrets::decrypt(&new_key, &ds_ct_after).unwrap(),
        br#"{"host":"secret-db-host"}"#
    );
    assert!(secrets::decrypt(&old_key, &ds_ct_after).is_err());

    // notification_destination
    let dest_after = notification_destination::Entity::find_by_id(dest.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let dest_ct_after = dest_after.options.unwrap();
    assert_eq!(
        secrets::decrypt(&new_key, &dest_ct_after).unwrap(),
        br#"{"url":"https://hooks.example/secret"}"#
    );
    assert!(secrets::decrypt(&old_key, &dest_ct_after).is_err());

    // org_embed_settings
    let embed_after = org_embed_settings::Entity::find_by_id(org_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let embed_ct_after = embed_after.embedding_secret.unwrap();
    assert_eq!(
        secrets::decrypt(&new_key, &embed_ct_after).unwrap(),
        b"hmac-secret-bytes"
    );
    assert!(secrets::decrypt(&old_key, &embed_ct_after).is_err());

    // organization settings ai key
    let org_after = organization::Entity::find_by_id(org_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let ai_ct_after_b64 = org_after.settings["ai"]["providers"]["gemini"]["key_encrypted"]
        .as_str()
        .unwrap()
        .to_string();
    let ai_ct_after = BASE64.decode(&ai_ct_after_b64).unwrap();
    assert_eq!(
        secrets::decrypt(&new_key, &ai_ct_after).unwrap(),
        b"sk-gemini-secret"
    );
    assert!(secrets::decrypt(&old_key, &ai_ct_after).is_err());
    // A sibling field the rotation doesn't touch must survive.
    assert_eq!(
        org_after.settings["ai"]["providers"]["gemini"]["model"],
        "gemini-3.1-flash-lite"
    );
}

/// A sibling top-level `settings` key present before rotation runs must survive it — the
/// rotation only ever rewrites `ai`. This seeds the sibling key before the subprocess starts, so
/// it does NOT exercise a write landing between the binary's read and its write (this file drives
/// the rotation as an opaque subprocess, with no hook to interleave a write mid-run); see
/// `rotate_and_merge_organization_does_not_revert_a_write_that_lands_between_load_and_write` in
/// `src/bin/rotate_encryption_key.rs` for that — a real interleave needs two direct calls into the
/// rotation function with a write between them, which only an in-process test can drive.
#[tokio::test]
async fn rotate_encryption_key_merges_settings_and_does_not_revert_a_sibling_key() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let old_key = secrets::parse_key_hex(OLD_KEY_HEX).unwrap();
    let new_key = secrets::parse_key_hex(NEW_KEY_HEX).unwrap();

    let org_id = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .id;

    let ai_ct_b64 = BASE64.encode(secrets::encrypt(&old_key, b"sk-gemini-secret-2").unwrap());
    // `extra` sits alongside `ai` at the top level of `settings` — exactly the shape a
    // concurrent, unrelated settings writer (`api/handlers/organization.rs`) would leave behind.
    let settings = serde_json::json!({
        "ai": { "providers": { "gemini": { "key_encrypted": ai_ct_b64 } } },
        "extra": { "enabled": true, "label": "team" },
    });
    let org_row = organization::Entity::find_by_id(org_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut org_active: organization::ActiveModel = org_row.into();
    org_active.settings = Set(settings);
    org_active
        .update(&app.db)
        .await
        .expect("update organization settings");

    let db_url = std::env::var("TEST_DATABASE_URL").unwrap();
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_rotate_encryption_key"))
        .env("DATABASE_URL", &db_url)
        .env("OLD_ENCRYPTION_KEY", OLD_KEY_HEX)
        .env("NEW_ENCRYPTION_KEY", NEW_KEY_HEX)
        .status()
        .expect("run rotate_encryption_key binary");
    assert!(status.success(), "rotate_encryption_key exited non-zero");

    let org_after = organization::Entity::find_by_id(org_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        org_after.settings["extra"],
        serde_json::json!({ "enabled": true, "label": "team" }),
        "an unrelated top-level settings key must survive the rotation untouched, got: {}",
        org_after.settings
    );
    let ai_ct_after = BASE64
        .decode(
            org_after.settings["ai"]["providers"]["gemini"]["key_encrypted"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        secrets::decrypt(&new_key, &ai_ct_after).unwrap(),
        b"sk-gemini-secret-2"
    );
}

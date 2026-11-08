//! Data source business logic: CRUD, test, schema refresh, pause/resume.
//!
//! No HTTP types here. A user sees a data source if they're an admin or it is in scope for one of
//! their groups; [`get_for_user`] returns not-found for both "missing" and "no access" so existence
//! doesn't leak. Mutations require `create_data_source`/`edit_data_source`, checked by the handler.

use std::collections::HashSet;

use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter,
    Statement,
};
use serde::Deserialize;
use serde_json::Value;

use arcodash_core::connectors::registry::ConnectorRegistry;
use arcodash_core::entities::{data_source, data_source_group, data_source_upload, query, user};
use arcodash_core::models::{DataSourceSchema, SchemaTable};
use arcodash_core::secrets;

use crate::authz::authorize::{self, Action, DataSourceVerb};
use crate::authz::data_source::{in_scope, postures};
use crate::authz::permissions::{load_effective_groups, user_has_permission};
use crate::error::ApiError;

/// Decrypt a data source's stored options and remove all fields the connector marks as `secret`.
/// For pre-filling the edit form — secrets are never echoed to the client.
pub fn options_without_secrets(
    registry: &ConnectorRegistry,
    encryption_key: &[u8; 32],
    ds: &data_source::Model,
) -> Result<Value, ApiError> {
    // An unknown connector type returns empty options rather than leaking the stored blob.
    let Some(connector) = registry.get(&ds.r#type) else {
        return Ok(Value::Object(serde_json::Map::new()));
    };
    let mut options = decrypt_options(encryption_key, ds.encrypted_options.as_deref())?;
    let schema = connector.configuration_schema();
    // Fail closed: a connector schema with no `"secret"` list is withheld entirely rather than risk
    // echoing an unmarked secret.
    let Some(secrets) = schema["secret"].as_array() else {
        tracing::warn!(
            connector = %ds.r#type,
            "connector configuration_schema declares no secret field list; withholding options"
        );
        return Ok(Value::Object(serde_json::Map::new()));
    };
    if let Some(obj) = options.as_object_mut() {
        for k in secrets.iter().filter_map(Value::as_str) {
            obj.remove(k);
        }
    }
    Ok(options)
}

/// Decrypt `data_sources.encrypted_options` and parse it as JSON (empty object if the column is
/// `NULL`). `pub(crate)`: `api::handlers::ai_query` needs the full decrypted options, secrets
/// included, to connect.
pub(crate) fn decrypt_options(
    encryption_key: &[u8; 32],
    encrypted: Option<&[u8]>,
) -> Result<Value, ApiError> {
    let Some(bytes) = encrypted else {
        return Ok(Value::Object(serde_json::Map::default()));
    };
    let plaintext = secrets::decrypt(encryption_key, bytes).map_err(|e| {
        // The real cause goes to the log only; the client-facing error stays opaque.
        tracing::error!(error = %e, "failed to decrypt data source options");
        ApiError::internal("failed to decrypt data source options")
    })?;
    serde_json::from_slice(&plaintext)
        .map_err(|_| ApiError::internal("stored data source options are not valid JSON"))
}

/// Encrypt `options` under `encryption_key` for `data_sources.encrypted_options`.
fn encrypt_options(encryption_key: &[u8; 32], options: &Value) -> Result<Vec<u8>, ApiError> {
    let plaintext = serde_json::to_vec(options)
        .map_err(|_| ApiError::internal("failed to serialize data source options"))?;
    secrets::encrypt(encryption_key, &plaintext)
        .map_err(|_| ApiError::internal("failed to encrypt data source options"))
}

/// `GET /api/data_sources` — every source for admins; otherwise only those in scope for one of the
/// user's groups, and none without `view_data_source`.
pub async fn list_for_user(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
) -> Result<Vec<data_source::Model>, ApiError> {
    let all = data_source::Entity::find()
        .filter(data_source::Column::OrgId.eq(org_id))
        .all(db)
        .await?;

    let groups = load_effective_groups(db, user).await?;
    if crate::authz::permissions::is_admin(&groups) {
        return Ok(all);
    }
    if !user_has_permission(user, &groups, "view_data_source") {
        return Ok(Vec::new());
    }

    let dsgs = data_source_group::Entity::find()
        .filter(data_source_group::Column::GroupId.is_in(user.groups.clone()))
        .all(db)
        .await?;
    let postures = postures(&groups);

    Ok(all
        .into_iter()
        .filter(|ds| in_scope(&postures, ds.id, &dsgs))
        .collect())
}

/// `GET /api/data_sources/<id>` — fetch one, access-checked via `authorize`; not-found and
/// no-access are indistinguishable.
pub async fn get_for_user(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    id: i64,
) -> Result<data_source::Model, ApiError> {
    let ds = data_source::Entity::find_by_id(id)
        .one(db)
        .await?
        .filter(|d| d.org_id == org_id)
        .ok_or_else(|| ApiError::not_found("data source not found"))?;

    authorize::authorize(
        db,
        user,
        Action::DataSource {
            verb: DataSourceVerb::View,
            id: ds.id,
        },
    )
    .await?;
    Ok(ds)
}

/// The option keys a connector's `configuration_schema()["properties"]` declares (empty if none).
fn schema_allowed_keys(schema: &Value) -> HashSet<String> {
    schema["properties"]
        .as_object()
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default()
}

/// Restrict `options` to the keys the connector's schema declares, so unrecognized (possibly
/// secret) fields are never stored.
fn strip_to_schema(options: &mut Value, allowed: &HashSet<String>) {
    if let Some(obj) = options.as_object_mut() {
        obj.retain(|k, _| allowed.contains(k));
    }
}

fn unknown_type_error(type_name: &str) -> ApiError {
    ApiError::bad_request(format!("unknown data source type '{type_name}'"))
}

/// Reject `options` if any network target the connector declares resolves to a blocked address (see
/// `net_guard::guard_host`); respects `ARCODASH_ALLOW_PRIVATE_CONNECTORS`.
async fn guard_create_target(
    registry: &ConnectorRegistry,
    type_name: &str,
    options: &Value,
) -> Result<(), ApiError> {
    let Some(connector) = registry.get(type_name) else {
        return Ok(());
    };
    arcodash_core::connectors::guard_targets(&connector.network_targets(options))
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))
}

/// `create`'s validation: resolve the connector, reject an unknown type, and strip
/// `options` to the schema keys. Pure (no I/O). Does not enforce `required` keys, so incomplete
/// drafts stay creatable.
fn validate_create_options(
    registry: &ConnectorRegistry,
    type_name: &str,
    mut options: Value,
) -> Result<Value, ApiError> {
    let connector = registry
        .get(type_name)
        .ok_or_else(|| unknown_type_error(type_name))?;
    strip_to_schema(
        &mut options,
        &schema_allowed_keys(&connector.configuration_schema()),
    );
    Ok(options)
}

/// `update`'s options step: merge `new_options` onto the decrypted existing options and strip to
/// `final_type`'s schema keys, unless the type is unrecognized (then keep as-is). Pure.
fn merge_and_strip_options(
    registry: &ConnectorRegistry,
    encryption_key: &[u8; 32],
    existing_encrypted: Option<&[u8]>,
    final_type: &str,
    new_options: Value,
) -> Result<Value, ApiError> {
    let connector = registry.get(final_type);
    let secret_fields: HashSet<String> = connector
        .as_ref()
        .and_then(|c| c.configuration_schema()["secret"].as_array().cloned())
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect();

    let mut merged = decrypt_options(encryption_key, existing_encrypted)?;
    if let (Some(base), Some(incoming)) = (merged.as_object_mut(), new_options.as_object()) {
        for (k, v) in incoming {
            // A blank secret means "leave it as it is"; secrets are never echoed back.
            if v.as_str() == Some("") && secret_fields.contains(k) {
                continue;
            }
            base.insert(k.clone(), v.clone());
        }
    } else {
        merged = new_options;
    }
    if let Some(connector) = connector {
        strip_to_schema(
            &mut merged,
            &schema_allowed_keys(&connector.configuration_schema()),
        );
    }
    Ok(merged)
}

/// For `csv`/`sqlite` with an `upload_id`, confirm the upload belongs to `org_id` (otherwise a
/// guessed UUID could resolve another org's bytes).
///
/// Run before persisting the row: the request transaction commits regardless of response status.
pub async fn verify_upload_ownership(
    db: &impl ConnectionTrait,
    org_id: i64,
    options: &Value,
    type_name: &str,
) -> Result<Option<uuid::Uuid>, ApiError> {
    if !matches!(type_name, "csv" | "sqlite") {
        return Ok(None);
    }
    let Some(upload_id) = options.get("upload_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let uuid =
        uuid::Uuid::parse_str(upload_id).map_err(|_| ApiError::bad_request("invalid upload_id"))?;
    data_source_upload::Entity::find_by_id(uuid)
        .one(db)
        .await?
        .filter(|r| r.org_id == org_id)
        .ok_or_else(|| ApiError::forbidden("upload not found for this org"))?;
    Ok(Some(uuid))
}

/// Sets `data_source_upload.data_source_id` so the orphan sweep leaves it alone; call only after
/// [`verify_upload_ownership`].
pub async fn link_upload(
    db: &impl ConnectionTrait,
    upload_id: uuid::Uuid,
    ds_id: i64,
) -> Result<(), ApiError> {
    let row = data_source_upload::Entity::find_by_id(upload_id)
        .one(db)
        .await?
        .ok_or_else(|| ApiError::internal("upload row disappeared after ownership check"))?;
    let mut active: data_source_upload::ActiveModel = row.into();
    active.data_source_id = Set(Some(ds_id));
    active.update(db).await?;
    Ok(())
}

/// `POST /api/data_sources` — create. Options are validated, stripped, SSRF-guarded
/// (`guard_create_target`), then encrypted.
pub async fn create(
    db: &impl ConnectionTrait,
    registry: &ConnectorRegistry,
    encryption_key: &[u8; 32],
    org_id: i64,
    name: String,
    type_name: String,
    options: Value,
) -> Result<data_source::Model, ApiError> {
    let options = validate_create_options(registry, &type_name, options)?;
    guard_create_target(registry, &type_name, &options).await?;
    let encrypted = encrypt_options(encryption_key, &options)?;

    let model = data_source::ActiveModel {
        org_id: Set(org_id),
        name: Set(name),
        r#type: Set(type_name),
        encrypted_options: Set(Some(encrypted)),
        ..Default::default()
    };

    let model = model.insert(db).await?;
    probe_health_on_save(db, registry, encryption_key, &model).await;
    Ok(model)
}

/// Passive connectivity probe on create/update via [`test_by_id`], which records health; probe
/// errors never block a valid save. `user_id: 0` is harmless: `csv`/`sqlite` only need `org_id`,
/// and `query_results` ignores `opts`.
async fn probe_health_on_save(
    db: &impl ConnectionTrait,
    registry: &ConnectorRegistry,
    encryption_key: &[u8; 32],
    ds: &data_source::Model,
) {
    let _ = test_by_id(db, registry, encryption_key, ds, 0).await;
}

/// Best-effort health write shared by every real `test_connection`; never propagates failures.
/// `checked_at` (when the attempt started) is also the recency guard, so a slower, earlier-started
/// check can't overwrite a newer observation.
async fn write_health_from_test(
    db: &impl ConnectionTrait,
    ds_id: i64,
    checked_at: chrono::DateTime<chrono::Utc>,
    result: &Result<(), ApiError>,
) {
    let backend = db.get_database_backend();
    let update = match result {
        Ok(()) => Statement::from_sql_and_values(
            backend,
            "UPDATE data_sources SET health_state = 'ok', health_error = NULL, \
             health_checked_at = $1 WHERE id = $2 \
             AND (health_checked_at IS NULL OR health_checked_at <= $1)",
            [checked_at.into(), ds_id.into()],
        ),
        Err(e) => Statement::from_sql_and_values(
            backend,
            "UPDATE data_sources SET health_state = 'failing', health_error = $1, \
             health_checked_at = $2 WHERE id = $3 \
             AND (health_checked_at IS NULL OR health_checked_at <= $2)",
            [e.message.clone().into(), checked_at.into(), ds_id.into()],
        ),
    };
    if let Err(e) = db.execute_raw(update).await {
        tracing::warn!(data_source_id = ds_id, error = %e, "failed to write data source health");
    }
}

/// `update`'s pre-write computation: the connector type the row ends up on and, when `new_options`
/// is given, its merged options. Exposed so callers can run the upload-ownership check first.
pub fn plan_update(
    registry: &ConnectorRegistry,
    encryption_key: &[u8; 32],
    existing: &data_source::Model,
    type_name: Option<&str>,
    new_options: Option<&Value>,
) -> Result<(String, Option<Value>), ApiError> {
    if let Some(t) = type_name.filter(|t| registry.get(t).is_none()) {
        return Err(unknown_type_error(t));
    }
    let final_type = type_name.unwrap_or(&existing.r#type).to_string();
    let merged = new_options
        .map(|new_options| {
            merge_and_strip_options(
                registry,
                encryption_key,
                existing.encrypted_options.as_deref(),
                &final_type,
                new_options.clone(),
            )
        })
        .transpose()?;
    Ok((final_type, merged))
}

/// `POST /api/data_sources/<id>` — update name/type/options; `None` fields are unchanged. An
/// incoming `type_name` must be a known connector; an unchanged type is not re-validated, so a
/// stale row isn't bricked.
pub async fn update(
    db: &impl ConnectionTrait,
    registry: &ConnectorRegistry,
    encryption_key: &[u8; 32],
    existing: data_source::Model,
    name: Option<String>,
    type_name: Option<String>,
    options: Option<Value>,
) -> Result<data_source::Model, ApiError> {
    let (final_type, merged) = plan_update(
        registry,
        encryption_key,
        &existing,
        type_name.as_deref(),
        options.as_ref(),
    )?;
    // Guard the fully merged options, not just the incoming ones; an update omitting the host would
    // otherwise skip the guard.
    if let Some(merged) = &merged {
        guard_create_target(registry, &final_type, merged).await?;
    }

    let mut active: data_source::ActiveModel = existing.into();
    if let Some(name) = name {
        active.name = Set(name);
    }
    if type_name.is_some() {
        active.r#type = Set(final_type);
    }
    if let Some(merged) = merged {
        active.encrypted_options = Set(Some(encrypt_options(encryption_key, &merged)?));
    }

    let model = active.update(db).await?;
    probe_health_on_save(db, registry, encryption_key, &model).await;
    Ok(model)
}

pub async fn delete(
    db: &impl ConnectionTrait,
    existing: data_source::Model,
) -> Result<(), ApiError> {
    let refs = query::Entity::find()
        .filter(query::Column::DataSourceId.eq(existing.id))
        .count(db)
        .await?;
    if refs > 0 {
        return Err(ApiError::conflict(format!(
            "{refs} queries still use this data source"
        )));
    }
    data_source::Entity::delete_by_id(existing.id)
        .exec(db)
        .await?;
    Ok(())
}

/// Bound on the backend's own interactive `test_connection`/`get_schema` calls, which have no
/// external wrapper and could hang on a connector whose internal timeout misses a failure mode.
pub(crate) fn interactive_connector_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(arcodash_core::connectors::query_timeout_secs())
}

/// Caps a connector error at one readable line; several connectors bubble raw multi-line response
/// bodies.
fn readable_connector_error(e: &anyhow::Error) -> String {
    const MAX_CHARS: usize = 300;
    // `to_string()` shows only the outermost context layer; the chain carries the underlying cause.
    let full = e
        .chain()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ");
    let collapsed = full.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return "unknown error".to_string();
    }
    if collapsed.chars().count() > MAX_CHARS {
        format!("{}…", collapsed.chars().take(MAX_CHARS).collect::<String>())
    } else {
        collapsed
    }
}

/// `POST /api/data_sources/test` (inline form) — resolve the connector and call `test_connection`.
///
/// Errors are readable and never include raw options or secrets (see [`readable_connector_error`]).
/// `org_id`/`user_id` are injected into `options` via `inject_caller`; load-bearing for
/// `csv`/`sqlite` and `query_results`.
pub async fn test(
    registry: &ConnectorRegistry,
    type_name: &str,
    org_id: i64,
    user_id: i64,
    options: &Value,
) -> Result<(), ApiError> {
    let connector = registry
        .get(type_name)
        .ok_or_else(|| unknown_type_error(type_name))?;

    let options = arcodash_core::connectors::query_results::inject_caller(
        options.clone(),
        type_name,
        org_id,
        user_id,
    );
    match tokio::time::timeout(
        interactive_connector_timeout(),
        connector.test_connection(&options),
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(ApiError::bad_request(format!(
            "Connection failed: {}",
            readable_connector_error(&e)
        ))),
        Err(_) => Err(ApiError::bad_request("Connection failed: timed out")),
    }
}

/// `POST /api/data_sources/test` (by-id form) — like [`test`] but uses the saved options and
/// records the outcome as the source's health.
pub async fn test_by_id(
    db: &impl ConnectionTrait,
    registry: &ConnectorRegistry,
    encryption_key: &[u8; 32],
    ds: &data_source::Model,
    user_id: i64,
) -> Result<(), ApiError> {
    let options = decrypt_options(encryption_key, ds.encrypted_options.as_deref())?;
    let checked_at = chrono::Utc::now();
    let result = test(registry, &ds.r#type, ds.org_id, user_id, &options).await;
    write_health_from_test(db, ds.id, checked_at, &result).await;
    result
}

/// Queue a profile job for the data source. `Ok(false)` when one is already pending.
pub async fn enqueue_profile(
    state: &crate::state::AppState,
    org_id: i64,
    data_source_id: i64,
    data: arcodash_core::models::ProfileData,
) -> Result<bool, ApiError> {
    let mut conn = state.redis.clone();
    arcodash_core::queue::enqueue_profile(
        &mut conn,
        state.queue.as_ref(),
        org_id,
        data_source_id,
        data,
    )
    .await
    .map_err(|e| ApiError::internal(format!("failed to enqueue profile job: {e}")))
}

/// Live schema for `query_results` sources (never persisted), computed per request for exactly what
/// `user_id` may see so no table from an unviewable query shows up.
pub async fn live_schema_for_query_results(
    registry: &ConnectorRegistry,
    encryption_key: &[u8; 32],
    ds: &data_source::Model,
    user_id: i64,
) -> Result<DataSourceSchema, ApiError> {
    let connector = registry
        .get(&ds.r#type)
        .ok_or_else(|| unknown_type_error(&ds.r#type))?;
    let options = arcodash_core::connectors::query_results::inject_caller(
        decrypt_options(encryption_key, ds.encrypted_options.as_deref())?,
        &ds.r#type,
        ds.org_id,
        user_id,
    );
    let tables = match tokio::time::timeout(
        interactive_connector_timeout(),
        connector.get_schema(&options),
    )
    .await
    {
        Ok(Ok(tables)) => tables,
        Ok(Err(e)) => {
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_GATEWAY,
                format!("schema lookup failed: {e:#}"),
            ))
        }
        Err(_) => {
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_GATEWAY,
                "schema lookup failed: timed out",
            ))
        }
    };
    Ok(DataSourceSchema {
        tables: tables
            .into_iter()
            .map(|t| SchemaTable {
                name: t.name,
                columns: t.columns,
            })
            .collect(),
    })
}

/// Cached `data_sources.schema` JSON in the current shape; legacy bare column-name strings are
/// upgraded on read.
#[must_use]
pub fn normalized_schema_json(raw: Option<Value>) -> Option<Value> {
    let raw = raw?;
    match DataSourceSchema::deserialize(&raw) {
        Ok(schema) => serde_json::to_value(schema).ok().or(Some(raw)),
        Err(_) => Some(raw),
    }
}

pub async fn pause(
    db: &impl ConnectionTrait,
    ds: data_source::Model,
    reason: Option<String>,
) -> Result<data_source::Model, ApiError> {
    let mut active: data_source::ActiveModel = ds.into();
    active.paused_at = Set(Some(chrono::Utc::now().into()));
    active.pause_reason = Set(reason);
    Ok(active.update(db).await?)
}

pub async fn resume(
    db: &impl ConnectionTrait,
    ds: data_source::Model,
) -> Result<data_source::Model, ApiError> {
    let mut active: data_source::ActiveModel = ds.into();
    active.paused_at = Set(None);
    active.pause_reason = Set(None);
    Ok(active.update(db).await?)
}

/// A paused source takes no new executions; the operator's reason is echoed back. Not an access
/// check: call after execute access is known, right before running against the source.
pub async fn require_not_paused(
    db: &impl ConnectionTrait,
    data_source_id: i64,
) -> Result<(), ApiError> {
    data_source::Entity::find_by_id(data_source_id)
        .one(db)
        .await?
        .and_then(|ds| ds.pause_refusal())
        .map_or(Ok(()), |message| Err(ApiError::conflict(message)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An earlier-started attempt reporting after a later one must not overwrite the newer
    /// observation. Needs Postgres (skips without `TEST_DATABASE_URL`).
    #[tokio::test]
    async fn write_health_from_test_keeps_the_newer_observation_when_the_older_write_lands_later() {
        let Ok(db_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let db = arcodash_core::db::connect(&db_url).await.unwrap();

        let mut org = arcodash_core::test_support::new_organization();
        org.slug = sea_orm::ActiveValue::Set(format!("health-guard-{}", uuid::Uuid::new_v4()));
        let org = org.insert(&db).await.unwrap();
        let ds = arcodash_core::test_support::new_data_source(org.id)
            .insert(&db)
            .await
            .unwrap();

        let later = chrono::Utc::now();
        let earlier = later - chrono::Duration::seconds(30);

        write_health_from_test(&db, ds.id, later, &Ok(())).await;
        let stale_err = ApiError::internal("stale failure from a slow, earlier-started attempt");
        write_health_from_test(&db, ds.id, earlier, &Err(stale_err)).await;

        let reloaded = data_source::Entity::find_by_id(ds.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded.health_state, "ok",
            "an earlier-started write landing later must not clobber the newer observation"
        );
    }

    /// Merges `patch` onto stored `pg` options `{"host": "h", "password": "stored"}`.
    fn merge_into_stored_pg_options(patch: Value) -> Value {
        let key = [3u8; 32];
        let existing = encrypt_options(
            &key,
            &serde_json::json!({"host": "h", "password": "stored"}),
        )
        .unwrap();
        merge_and_strip_options(
            &ConnectorRegistry::default(),
            &key,
            Some(&existing),
            "pg",
            patch,
        )
        .unwrap()
    }

    #[test]
    fn merge_and_strip_options_blank_secret_keeps_stored_value() {
        let merged =
            merge_into_stored_pg_options(serde_json::json!({"host": "h2", "password": ""}));
        assert_eq!(merged["password"], serde_json::json!("stored"));
        assert_eq!(merged["host"], serde_json::json!("h2"));
    }

    #[test]
    fn merge_and_strip_options_null_secret_overwrites() {
        let merged =
            merge_into_stored_pg_options(serde_json::json!({"host": "h", "password": null}));
        assert_eq!(merged["password"], serde_json::Value::Null);
    }

    #[test]
    fn merge_and_strip_options_blank_non_secret_field_overwrites() {
        let merged = merge_into_stored_pg_options(serde_json::json!({"host": ""}));
        assert_eq!(merged["host"], serde_json::json!(""));
    }

    #[test]
    fn merge_and_strip_options_unknown_connector_type_treats_nothing_as_secret() {
        let key = [3u8; 32];
        let registry = ConnectorRegistry::new();
        let existing = encrypt_options(&key, &serde_json::json!({"password": "stored"})).unwrap();
        let merged = merge_and_strip_options(
            &registry,
            &key,
            Some(&existing),
            "not_a_real_connector_type",
            serde_json::json!({"password": ""}),
        )
        .unwrap();
        assert_eq!(merged["password"], serde_json::json!(""));
    }

    #[test]
    fn encrypt_then_decrypt_options_round_trips() {
        let key = [3u8; 32];
        let options = serde_json::json!({"host": "localhost", "password": "secret"});
        let encrypted = encrypt_options(&key, &options).unwrap();
        let decrypted = decrypt_options(&key, Some(&encrypted)).unwrap();
        assert_eq!(decrypted, options);
    }

    #[test]
    fn decrypt_options_returns_empty_object_for_none() {
        let key = [3u8; 32];
        let decrypted = decrypt_options(&key, None).unwrap();
        assert_eq!(decrypted, Value::Object(Default::default()));
    }

    #[test]
    fn normalized_schema_json_upgrades_bare_column_names() {
        let out = normalized_schema_json(Some(serde_json::json!({
            "tables": [{"name": "t", "columns": ["a", "b"]}]
        })))
        .unwrap();
        assert_eq!(
            out,
            serde_json::json!({"tables": [{"name": "t", "columns": [{"name": "a"}, {"name": "b"}]}]})
        );
    }

    #[test]
    fn normalized_schema_json_defaults_missing_tables() {
        // `tables` is `#[serde(default)]`, so an object lacking it parses as an empty schema.
        let raw = serde_json::json!({"unexpected": true});
        assert_eq!(
            normalized_schema_json(Some(raw)),
            Some(serde_json::json!({"tables": []}))
        );
    }

    #[test]
    fn normalized_schema_json_handles_none() {
        assert_eq!(normalized_schema_json(None), None);
    }

    #[test]
    fn readable_connector_error_collapses_multiline_and_truncates() {
        let long_line = "x".repeat(400);
        let raw = anyhow::anyhow!(
            "ClickHouse error: Code: 60.\nDB::Exception: Table default.{long_line} doesn't \
             exist.\n(version 24.1.1)\n"
        );
        let msg = readable_connector_error(&raw);
        assert!(!msg.contains('\n'), "must be a single line, got: {msg}");
        assert!(
            msg.chars().count() <= 301,
            "must be capped, got {} chars",
            msg.chars().count()
        );
        assert!(msg.ends_with('…'));
    }

    #[test]
    fn readable_connector_error_passes_short_messages_through() {
        let raw = anyhow::anyhow!("failed to connect to Postgres data source");
        assert_eq!(
            readable_connector_error(&raw),
            "failed to connect to Postgres data source"
        );
    }

    #[test]
    fn readable_connector_error_joins_the_full_context_chain() {
        let leaf = anyhow::anyhow!("connection refused (os error 61)");
        let wrapped = leaf.context("inner").context("outer");
        let msg = readable_connector_error(&wrapped);
        assert!(msg.contains("outer"), "must contain outer context: {msg}");
        assert!(msg.contains("inner"), "must contain inner context: {msg}");
        assert!(
            msg.contains("connection refused (os error 61)"),
            "must contain leaf cause: {msg}"
        );
    }

    #[test]
    fn options_without_secrets_fails_closed_for_unknown_connector_type() {
        // An unregistered `type` must fail closed to an empty object, never echo the decrypted
        // blob.
        let key = [3u8; 32];
        let secret_options =
            serde_json::json!({"host": "internal.example", "password": "super-secret"});
        let plaintext = serde_json::to_vec(&secret_options).unwrap();
        let encrypted = secrets::encrypt(&key, &plaintext).unwrap();

        let ds = ds_model("totally-unknown-connector-type", Some(encrypted));

        let registry = ConnectorRegistry::new();
        let result = options_without_secrets(&registry, &key, &ds).unwrap();
        assert_eq!(
            result,
            Value::Object(Default::default()),
            "unknown connector type must yield an empty object, never the decrypted blob"
        );
    }

    /// A connector whose `configuration_schema()` has no `"secret"` key at all.
    struct NoSecretDeclaredConnector;

    #[async_trait::async_trait]
    impl arcodash_core::connectors::QueryRunner for NoSecretDeclaredConnector {
        fn type_name(&self) -> &'static str {
            "no-secret-declared"
        }

        fn configuration_schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {"host": {"type": "string"}}})
        }

        fn network_targets(&self, _opts: &Value) -> Vec<arcodash_core::connectors::NetworkTarget> {
            vec![]
        }

        async fn test_connection(&self, _opts: &Value) -> anyhow::Result<()> {
            Ok(())
        }

        async fn get_schema(
            &self,
            _opts: &Value,
        ) -> anyhow::Result<Vec<arcodash_core::connectors::TableSchema>> {
            Ok(vec![])
        }

        async fn run_query(
            &self,
            _opts: &Value,
            _query: &str,
        ) -> anyhow::Result<arcodash_core::connectors::QueryExecutionResult> {
            anyhow::bail!("not exercised by this test")
        }
    }

    fn ds_model(r#type: &str, encrypted_options: Option<Vec<u8>>) -> data_source::Model {
        data_source::Model {
            id: 1,
            org_id: 1,
            name: "Test Source".to_string(),
            r#type: r#type.to_string(),
            encrypted_options,
            scheduled_queue_name: None,
            schema: None,
            schema_refreshed_at: None,
            paused_at: None,
            pause_reason: None,
            health_state: "unknown".to_string(),
            health_error: None,
            health_checked_at: None,
            ai_profile: None,
            ai_profile_built_at: None,
            ai_profile_last_error: None,
            created_at: chrono::Utc::now().into(),
            updated_at: chrono::Utc::now().into(),
        }
    }

    #[test]
    fn options_without_secrets_removes_declared_secret_and_keeps_other_fields() {
        let key = [3u8; 32];
        let options = serde_json::json!({"host": "db.internal", "password": "hunter2"});
        let encrypted = secrets::encrypt(&key, &serde_json::to_vec(&options).unwrap()).unwrap();
        let ds = ds_model("pg", Some(encrypted));

        let result = options_without_secrets(&pg_registry(), &key, &ds).unwrap();
        assert_eq!(result["host"], "db.internal");
        assert!(
            result.get("password").is_none(),
            "declared secret field must be stripped: {result:?}"
        );
    }

    #[test]
    fn options_without_secrets_fails_closed_when_schema_declares_no_secret_list() {
        let key = [3u8; 32];
        let options = serde_json::json!({"host": "db.internal"});
        let encrypted = secrets::encrypt(&key, &serde_json::to_vec(&options).unwrap()).unwrap();
        let ds = ds_model("no-secret-declared", Some(encrypted));

        let mut registry = ConnectorRegistry::new();
        registry.register(std::sync::Arc::new(NoSecretDeclaredConnector));

        let result = options_without_secrets(&registry, &key, &ds).unwrap();
        assert_eq!(
            result,
            Value::Object(Default::default()),
            "a connector with no \"secret\" list must withhold everything, not echo the blob: {result:?}"
        );
    }

    /// A registry with just the `pg` connector, enough for schema validation without a database.
    fn pg_registry() -> ConnectorRegistry {
        let mut registry = ConnectorRegistry::new();
        registry.register(std::sync::Arc::new(
            arcodash_core::connectors::postgres::PostgresConnector::new(),
        ));
        registry
    }

    #[test]
    fn create_rejects_unknown_type() {
        let registry = pg_registry();
        let err = validate_create_options(&registry, "not-a-real-type", serde_json::json!({}))
            .unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn plan_update_rejects_an_unknown_type() {
        let registry = pg_registry();
        let key = [3u8; 32];
        let existing = ds_model("pg", None);
        let err = plan_update(&registry, &key, &existing, Some("oracle"), None).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn create_strips_a_non_schema_key() {
        let registry = pg_registry();
        let options = serde_json::json!({
            "host": "db.internal", "user": "u", "dbname": "d",
            "not_in_schema": "should be dropped"
        });
        let stripped = validate_create_options(&registry, "pg", options).unwrap();
        assert!(stripped.get("not_in_schema").is_none());
        assert_eq!(stripped["host"], "db.internal");
    }

    #[test]
    fn create_allows_an_incomplete_but_known_type_draft() {
        // Create does not enforce `required` schema keys, so a draft without `host` is creatable.
        let registry = pg_registry();
        let options = serde_json::json!({"user": "u", "dbname": "d"});
        let stripped = validate_create_options(&registry, "pg", options).unwrap();
        assert_eq!(stripped["user"], "u");
    }

    /// A registry with `pg`, `mssql` (host field named `server`) and `json` (URL-shaped).
    fn guard_target_registry() -> ConnectorRegistry {
        let mut registry = pg_registry();
        registry.register(std::sync::Arc::new(
            arcodash_core::connectors::mssql::MssqlConnector,
        ));
        registry.register(std::sync::Arc::new(
            arcodash_core::connectors::http_connectors::JsonConnector,
        ));
        registry
    }

    /// Stands in for Snowflake's `network_targets` with the vendor suffix stripped (the real
    /// wildcard DNS makes a blocked-address check unobservable), proving that a host assembled from
    /// a non-`host` field like `account` is still guarded.
    struct FragmentHostConnector;

    #[async_trait::async_trait]
    impl arcodash_core::connectors::QueryRunner for FragmentHostConnector {
        fn type_name(&self) -> &'static str {
            "fragment-host-test"
        }

        fn configuration_schema(&self) -> Value {
            serde_json::json!({"secret": []})
        }

        fn network_targets(&self, opts: &Value) -> Vec<arcodash_core::connectors::NetworkTarget> {
            opts["account"]
                .as_str()
                .map(|account| {
                    vec![arcodash_core::connectors::NetworkTarget::Host(
                        account.to_string(),
                    )]
                })
                .unwrap_or_default()
        }

        async fn test_connection(&self, _opts: &Value) -> anyhow::Result<()> {
            Ok(())
        }

        async fn get_schema(
            &self,
            _opts: &Value,
        ) -> anyhow::Result<Vec<arcodash_core::connectors::TableSchema>> {
            Ok(vec![])
        }

        async fn run_query(
            &self,
            _opts: &Value,
            _query: &str,
        ) -> anyhow::Result<arcodash_core::connectors::QueryExecutionResult> {
            anyhow::bail!("not exercised by this test")
        }
    }

    #[tokio::test]
    async fn guard_create_target_blocks_a_host_field_pointing_at_the_metadata_ip_unless_allowed() {
        let _env = arcodash_core::test_support::private_net_env(None);
        let registry = guard_target_registry();
        // user/dbname must be present so `PostgresOptions` parses; otherwise no `network_targets`
        // exist and the assert proves nothing.
        let options = serde_json::json!({
            "host": "169.254.169.254",
            "port": 5432,
            "user": "u",
            "dbname": "d"
        });
        let err = guard_create_target(&registry, "pg", &options)
            .await
            .unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);

        std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
        assert!(guard_create_target(&registry, "pg", &options).await.is_ok());
    }

    #[tokio::test]
    async fn guard_create_target_blocks_a_server_field_pointing_at_a_private_address() {
        let _env = arcodash_core::test_support::private_net_env(None);
        // mssql names its host field `server`; user/db must be present so `MssqlOptions` parses.
        let registry = guard_target_registry();
        let options = serde_json::json!({
            "server": "10.0.0.1",
            "port": 1433,
            "user": "u",
            "db": "d"
        });
        assert!(guard_create_target(&registry, "mssql", &options)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn guard_create_target_blocks_a_url_field_pointing_at_loopback() {
        let _env = arcodash_core::test_support::private_net_env(None);
        // URL connectors and the `api_endpoint` override store a full URL.
        let registry = guard_target_registry();
        let options = serde_json::json!({"url": "http://127.0.0.1:9050/data.json"});
        assert!(guard_create_target(&registry, "json", &options)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn guard_create_target_allows_a_public_host() {
        let _env = arcodash_core::test_support::private_net_env(None);
        let registry = guard_target_registry();
        // user/dbname included so this resolves to `Host("8.8.8.8")` rather than passing vacuously.
        let options = serde_json::json!({
            "host": "8.8.8.8",
            "port": 5432,
            "user": "u",
            "dbname": "d"
        });
        assert!(guard_create_target(&registry, "pg", &options).await.is_ok());
    }

    #[tokio::test]
    async fn guard_create_target_allows_an_incomplete_draft_with_no_network_field() {
        let _env = arcodash_core::test_support::hold_private_net_env();
        // A draft missing its host has nothing to guard yet and must stay creatable.
        let registry = guard_target_registry();
        let options = serde_json::json!({"user": "u", "dbname": "d"});
        assert!(guard_create_target(&registry, "pg", &options).await.is_ok());
    }

    /// `guard_create_target` must see a hostname fragment nested in a field of any name (like
    /// Snowflake's `account`), rejecting a blocked address at create time.
    #[tokio::test]
    async fn guard_create_target_blocks_an_account_fragment_assembling_to_a_blocked_host() {
        let _env = arcodash_core::test_support::private_net_env(None);
        let mut registry = guard_target_registry();
        registry.register(std::sync::Arc::new(FragmentHostConnector));
        let options = serde_json::json!({"account": "169.254.169.254"});
        let err = guard_create_target(&registry, "fragment-host-test", &options)
            .await
            .unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn update_strips_a_stale_key_from_the_merged_result() {
        let registry = pg_registry();
        let key = [9u8; 32];
        let existing = encrypt_options(
            &key,
            &serde_json::json!({"host": "old-host", "stale_field": "leftover"}),
        )
        .unwrap();
        let merged = merge_and_strip_options(
            &registry,
            &key,
            Some(&existing),
            "pg",
            serde_json::json!({"host": "new-host"}),
        )
        .unwrap();
        assert_eq!(merged["host"], "new-host");
        assert!(
            merged.get("stale_field").is_none(),
            "a key dropped from the connector's schema must not survive a merge"
        );
    }

    #[test]
    fn update_of_an_unknown_typed_legacy_row_does_not_panic_and_stays_editable() {
        let registry = pg_registry();
        let key = [9u8; 32];
        let existing = encrypt_options(&key, &serde_json::json!({"anything": "goes"})).unwrap();
        let merged = merge_and_strip_options(
            &registry,
            &key,
            Some(&existing),
            "totally-unknown-legacy-type",
            serde_json::json!({"another": "field"}),
        )
        .unwrap();
        assert_eq!(merged["anything"], "goes");
        assert_eq!(merged["another"], "field");
    }

    /// A connector whose `test_connection`/`get_schema` never resolve, standing in for a driver
    /// that hangs past its own timeout.
    struct HangingConnector;

    #[async_trait::async_trait]
    impl arcodash_core::connectors::QueryRunner for HangingConnector {
        fn type_name(&self) -> &'static str {
            "hanging-test"
        }

        fn configuration_schema(&self) -> Value {
            serde_json::json!({"secret": []})
        }

        fn network_targets(&self, _opts: &Value) -> Vec<arcodash_core::connectors::NetworkTarget> {
            vec![]
        }

        async fn test_connection(&self, _opts: &Value) -> anyhow::Result<()> {
            std::future::pending().await
        }

        async fn get_schema(
            &self,
            _opts: &Value,
        ) -> anyhow::Result<Vec<arcodash_core::connectors::TableSchema>> {
            std::future::pending().await
        }

        async fn run_query(
            &self,
            _opts: &Value,
            _query: &str,
        ) -> anyhow::Result<arcodash_core::connectors::QueryExecutionResult> {
            anyhow::bail!("not exercised by this test")
        }
    }

    // One combined test to avoid a cross-thread race on `QUERY_TIMEOUT_SECS`; uses a real short
    // sleep because `tokio`'s `test-util` isn't enabled.
    #[tokio::test]
    async fn test_connection_and_get_schema_time_out_instead_of_hanging_forever() {
        std::env::set_var("QUERY_TIMEOUT_SECS", "1");
        let mut registry = ConnectorRegistry::new();
        registry.register(std::sync::Arc::new(HangingConnector));

        let err = test(&registry, "hanging-test", 1, 1, &serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(
            err.message.contains("timed out"),
            "expected a timeout error, got: {}",
            err.message
        );

        let key = [4u8; 32];
        let encrypted = encrypt_options(&key, &serde_json::json!({})).unwrap();
        let ds = ds_model("hanging-test", Some(encrypted));
        let err = live_schema_for_query_results(&registry, &key, &ds, 1)
            .await
            .unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_GATEWAY);
        assert!(
            err.message.contains("timed out"),
            "expected a timeout error, got: {}",
            err.message
        );

        std::env::remove_var("QUERY_TIMEOUT_SECS");
    }

    #[test]
    fn is_admin_detects_admin_group() {
        let admin_group =
            arcodash_core::test_support::group_model(1, 1, "admin", vec!["admin".to_string()]);
        let regular_group = arcodash_core::test_support::group_model(
            2,
            1,
            "default",
            vec!["view_query".to_string()],
        );
        assert!(crate::authz::permissions::is_admin(std::slice::from_ref(
            &admin_group
        )));
        assert!(!crate::authz::permissions::is_admin(&[regular_group]));
        assert!(crate::authz::permissions::is_admin(&[admin_group]));
    }
}

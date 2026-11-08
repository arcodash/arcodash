//! Minimal fixtures/factories for tests. These build `ActiveModel`s with sensible defaults
//! pre-filled so callers only need to override the fields relevant to their test.
//!
//! Builders return `ActiveModel`s (not yet inserted); callers `.insert(db)` them as needed
//! once a real `DatabaseConnection` is available (e.g. in integration tests run against a
//! throwaway Postgres instance).

use chrono::Utc;
use sea_orm::ActiveValue::{NotSet, Set};
use serde_json::json;

use crate::entities::{
    access_permission, dashboard, data_source, data_source_group, group, organization, query,
    query_snippet, user, visualization, widget,
};
use crate::queue::{JobMessage, Queue, QueueName};

/// Build an `organizations` row: `name="Test Org"`, `slug="test-org"`, empty settings.
#[must_use]
pub fn new_organization() -> organization::ActiveModel {
    organization::ActiveModel {
        id: NotSet,
        name: Set("Test Org".to_string()),
        slug: Set("test-org".to_string()),
        settings: Set(json!({})),
        created_at: NotSet,
        updated_at: NotSet,
    }
}

/// Build a `users` row scoped to `org_id`.
///
/// `groups` defaults to empty; `api_key_hash` defaults to the digest of a fixed test key (callers
/// needing uniqueness across multiple users in the same test should override it).
#[must_use]
pub fn new_user(org_id: i64) -> user::ActiveModel {
    user::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set("Test User".to_string()),
        email: Set("test-user@example.com".to_string()),
        password_hash: Set(None),
        groups: Set(vec![]),
        api_key_hash: Set(crate::hash::hash_api_key("test-api-key")),
        disabled_at: Set(None),
        is_invitation_pending: Set(false),
        profile_image_url: Set(None),
        details: Set(json!({})),
        created_at: NotSet,
        updated_at: NotSet,
        digest_enabled: Set(false),
        digest_unsubscribe_token: Set(None),
        ..Default::default()
    }
}

/// Build a `groups` row scoped to `org_id` with the given name + permissions.
#[must_use]
pub fn new_group(org_id: i64, name: &str, permissions: Vec<String>) -> group::ActiveModel {
    group::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set(name.to_string()),
        permissions: Set(permissions),
        r#type: Set("regular".to_string()),
        created_at: NotSet,
        updated_at: NotSet,
        data_source_access_restricted: Set(false),
    }
}

/// Build a `data_sources` row scoped to `org_id` (type defaults to "pg" for the Postgres
/// connector).
#[must_use]
pub fn new_data_source(org_id: i64) -> data_source::ActiveModel {
    data_source::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set("Test Data Source".to_string()),
        r#type: Set("pg".to_string()),
        encrypted_options: Set(None),
        scheduled_queue_name: Set(None),
        schema: Set(None),
        schema_refreshed_at: Set(None),
        paused_at: Set(None),
        pause_reason: Set(None),
        // `NotSet` lets the DB column default apply.
        health_state: NotSet,
        health_error: NotSet,
        health_checked_at: NotSet,
        ai_profile: NotSet,
        ai_profile_built_at: NotSet,
        ai_profile_last_error: NotSet,
        created_at: NotSet,
        updated_at: NotSet,
    }
}

/// Build a `queries` row. Requires the owning org/data-source/user ids since `queries` has
/// NOT NULL FKs to all three.
#[must_use]
pub fn new_query(org_id: i64, data_source_id: i64, user_id: i64) -> query::ActiveModel {
    query::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        data_source_id: Set(data_source_id),
        name: Set("Test Query".to_string()),
        description: Set(None),
        query: Set("SELECT 1".to_string()),
        query_hash: Set(crate::hash::gen_query_hash("SELECT 1", &json!({}))),
        // Unique per call: `api_key` sits under a UNIQUE index, and this builder runs many
        // times against the same test database across the whole suite.
        api_key: Set(uuid::Uuid::new_v4().to_string()),
        user_id: Set(user_id),
        last_modified_by_id: Set(None),
        latest_query_data_id: Set(None),
        is_archived: Set(false),
        visibility: NotSet,
        options: Set(json!({})),
        tags: Set(None),
        last_execution_status: NotSet,
        last_execution_at: Set(None),
        row_count: Set(None),
        last_execution_error: Set(None),
        created_at: NotSet,
        updated_at: NotSet,
        published_name: Set(None),
        published_description: Set(None),
        published_query: Set(None),
        published_query_hash: Set(None),
        published_data_source_id: Set(None),
        published_options: Set(None),
        published_latest_query_data_id: Set(None),
        published_change_id: Set(None),
        published_at: Set(None),
        working_version: Set(0),
        unpublished_at: NotSet,
    }
}

/// Copy a query's draft columns onto its `published_*` columns, as if it had just been published
/// (`published_change_id` becomes non-NULL).
///
/// For tests that need a published-query fixture without going through the real publish service.
#[must_use]
pub fn seed_published(mut q: query::ActiveModel, change_id: Option<i64>) -> query::ActiveModel {
    q.published_name = Set(Some(q.name.clone().unwrap()));
    q.published_description = Set(q.description.clone().unwrap());
    q.published_query = Set(Some(q.query.clone().unwrap()));
    q.published_query_hash = Set(Some(q.query_hash.clone().unwrap()));
    q.published_data_source_id = Set(Some(q.data_source_id.clone().unwrap()));
    q.published_options = Set(Some(q.options.clone().unwrap()));
    q.published_latest_query_data_id = Set(q.latest_query_data_id.clone().unwrap());
    q.published_change_id = Set(change_id);
    q.published_at = Set(Some(Utc::now().into()));
    q
}

/// Build a `query_snippets` row scoped to `org_id`/`user_id`.
#[must_use]
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "`${1:column}` is editor snippet placeholder syntax, not a format string"
)]
pub fn new_query_snippet(org_id: i64, user_id: i64) -> query_snippet::ActiveModel {
    query_snippet::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        trigger: Set("test-trigger".to_string()),
        description: Set(None),
        snippet: Set("SELECT ${1:column} FROM ${2:table}".to_string()),
        user_id: Set(user_id),
        created_at: NotSet,
        updated_at: NotSet,
    }
}

/// Build a `dashboards` row scoped to `org_id`/`user_id`.
#[must_use]
pub fn new_dashboard(org_id: i64, user_id: i64) -> dashboard::ActiveModel {
    dashboard::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        slug: Set("test-dashboard".to_string()),
        name: Set("Test Dashboard".to_string()),
        user_id: Set(user_id),
        layout: Set(json!([])),
        dashboard_filters_enabled: Set(false),
        is_archived: Set(false),
        visibility: NotSet,
        tags: Set(None),
        options: Set(json!({})),
        published_snapshot: Set(None),
        created_at: NotSet,
        updated_at: NotSet,
    }
}

/// Insert a published dashboard with the given widgets.
///
/// Inserts via [`new_dashboard`] plus the widgets (`dashboard_id`/`id` left
/// unset — assigned on insert), then sets `published_snapshot` to the arrangement that just went
/// in, so the dashboard reads as already-published with a snapshot matching its live widgets.
pub async fn seed_published_dashboard(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    user_id: i64,
    widgets: Vec<widget::ActiveModel>,
) -> dashboard::Model {
    use sea_orm::{ActiveModelTrait, EntityTrait};

    let dash = new_dashboard(org_id, user_id)
        .insert(db)
        .await
        .expect("dashboard insert failed");

    let mut snapshot_widgets = Vec::with_capacity(widgets.len());
    for mut w in widgets {
        w.dashboard_id = Set(dash.id);
        let inserted = w.insert(db).await.expect("widget insert failed");
        let query_id = match inserted.visualization_id {
            Some(viz_id) => visualization::Entity::find_by_id(viz_id)
                .one(db)
                .await
                .expect("visualization lookup failed")
                .map(|v| v.query_id),
            None => None,
        };
        snapshot_widgets.push(json!({
            "id": inserted.id,
            "visualization_id": inserted.visualization_id,
            "query_id": query_id,
            "text": inserted.text,
            "width": inserted.width,
            "options": inserted.options,
        }));
    }

    let snapshot = json!({
        "name": dash.name,
        "layout": dash.layout,
        "options": dash.options,
        "tags": dash.tags.clone().unwrap_or_default(),
        "dashboard_filters_enabled": dash.dashboard_filters_enabled,
        "widgets": snapshot_widgets,
    });

    let mut active: dashboard::ActiveModel = dash.into();
    active.published_snapshot = Set(Some(snapshot));
    active.update(db).await.expect("dashboard update failed")
}

// ---------------------------------------------------------------------------------------
// In-memory `Model` fixtures (no `id`/timestamps auto-assignment via the DB) for authz unit
// tests, which operate on already-fetched data and never touch a `DatabaseConnection`.
// ---------------------------------------------------------------------------------------

/// Build a `users::Model` directly (no DB round trip), scoped to `org_id` and member of
/// `group_ids`. Useful for authz unit tests that check permissions/access over plain data.
#[must_use]
pub fn user_model(id: i64, org_id: i64, group_ids: Vec<i64>) -> user::Model {
    user::Model {
        id,
        org_id,
        name: "Test User".to_string(),
        email: format!("test-user-{id}@example.com"),
        password_hash: None,
        groups: group_ids,
        api_key_hash: crate::hash::hash_api_key(&format!("test-api-key-{id}")),
        disabled_at: None,
        is_invitation_pending: false,
        profile_image_url: None,
        details: json!({}),
        created_at: Utc::now().into(),
        updated_at: Utc::now().into(),
        digest_enabled: false,
        digest_unsubscribe_token: None,
        digest_hour: None,
        digest_timezone: None,
        session_token_version: 0,
        digest_last_sent_at: None,
    }
}

/// Build a `groups::Model` directly (no DB round trip) with the given permissions.
#[must_use]
pub fn group_model(id: i64, org_id: i64, name: &str, permissions: Vec<String>) -> group::Model {
    group_model_with_restriction(id, org_id, name, permissions, false)
}

/// [`group_model`], but with an explicit `data_source_access_restricted` posture — for tests
/// exercising the restricted-group reach path.
#[must_use]
pub fn group_model_with_restriction(
    id: i64,
    org_id: i64,
    name: &str,
    permissions: Vec<String>,
    data_source_access_restricted: bool,
) -> group::Model {
    group::Model {
        id,
        org_id,
        name: name.to_string(),
        permissions,
        r#type: "regular".to_string(),
        created_at: Utc::now().into(),
        updated_at: Utc::now().into(),
        data_source_access_restricted,
    }
}

/// Build a `data_source_groups::Model` directly (no DB round trip).
#[must_use]
pub fn data_source_group_model(
    id: i64,
    data_source_id: i64,
    group_id: i64,
) -> data_source_group::Model {
    data_source_group::Model {
        id,
        data_source_id,
        group_id,
        created_at: Utc::now().into(),
        updated_at: Utc::now().into(),
    }
}

/// Build a user-grant `access_permissions::Model` directly (no DB round trip).
#[must_use]
pub fn access_permission_user(
    id: i64,
    object_type: &str,
    object_id: i64,
    access_type: &str,
    grantor_id: i64,
    grantee_id: i64,
) -> access_permission::Model {
    access_permission::Model {
        id,
        object_type: object_type.to_string(),
        object_id,
        access_type: access_type.to_string(),
        grantor_id,
        grantee_id: Some(grantee_id),
        grantee_group_id: None,
        created_at: Utc::now().into(),
        updated_at: Utc::now().into(),
    }
}

/// Build a group-grant `access_permissions::Model` directly (no DB round trip).
#[must_use]
pub fn access_permission_group(
    id: i64,
    object_type: &str,
    object_id: i64,
    access_type: &str,
    grantor_id: i64,
    grantee_group_id: i64,
) -> access_permission::Model {
    access_permission::Model {
        id,
        object_type: object_type.to_string(),
        object_id,
        access_type: access_type.to_string(),
        grantor_id,
        grantee_id: None,
        grantee_group_id: Some(grantee_group_id),
        created_at: Utc::now().into(),
        updated_at: Utc::now().into(),
    }
}

static PRIVATE_NET_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds [`PRIVATE_NET_ENV_LOCK`] and restores the `net_guard::ALLOW_PRIVATE_ENV` value seen at
/// acquisition on drop.
pub struct PrivateNetEnv {
    previous: Option<String>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for PrivateNetEnv {
    fn drop(&mut self) {
        set_private_net_env(self.previous.as_deref());
    }
}

fn set_private_net_env(value: Option<&str>) {
    match value {
        Some(v) => std::env::set_var(crate::net_guard::ALLOW_PRIVATE_ENV, v),
        None => std::env::remove_var(crate::net_guard::ALLOW_PRIVATE_ENV),
    }
}

/// Locks `m`, ignoring poisoning: a panicking test must not cascade into every later one.
fn lock_ignoring_poison(m: &'static std::sync::Mutex<()>) -> std::sync::MutexGuard<'static, ()> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The env is process-global and tests run on parallel threads, so every test that sets, unsets
/// or depends on `ALLOW_PRIVATE_ENV` must hold this for its whole body.
#[must_use]
pub fn hold_private_net_env() -> PrivateNetEnv {
    let lock = lock_ignoring_poison(&PRIVATE_NET_ENV_LOCK);
    PrivateNetEnv {
        previous: std::env::var(crate::net_guard::ALLOW_PRIVATE_ENV).ok(),
        _lock: lock,
    }
}

/// Like [`hold_private_net_env`], with the var set to `value` (`None` removes it).
#[must_use]
pub fn private_net_env(value: Option<&str>) -> PrivateNetEnv {
    let held = hold_private_net_env();
    set_private_net_env(value);
    held
}

static DECRYPT_COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The decrypt-failure metric is process-global, so every test that triggers a failed decrypt or
/// asserts on that counter must hold this for its whole body.
pub fn hold_decrypt_counter() -> std::sync::MutexGuard<'static, ()> {
    lock_ignoring_poison(&DECRYPT_COUNTER_LOCK)
}

static MAX_QUERY_ROWS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `MAX_QUERY_ROWS` is process-global env and is folded into the query hash, so every test that
/// mutates it or computes/compares a query hash must hold this for its whole body.
///
/// When combined with another env lock, take this one first.
pub fn hold_max_query_rows_env() -> std::sync::MutexGuard<'static, ()> {
    lock_ignoring_poison(&MAX_QUERY_ROWS_ENV_LOCK)
}

/// A connection to `TEST_DATABASE_URL`, or `None` (after logging the skip) when it is unset.
pub async fn test_db() -> Option<sea_orm::DatabaseConnection> {
    let Ok(database_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("skipping: TEST_DATABASE_URL not set");
        return None;
    };
    Some(crate::db::connect(&database_url).await.unwrap())
}

/// [`test_db`] plus a Valkey connection to `TEST_REDIS_URL` (required once the DB is set) and that
/// URL.
pub async fn test_db_and_valkey() -> Option<(
    sea_orm::DatabaseConnection,
    redis::aio::ConnectionManager,
    String,
)> {
    let db = test_db().await?;
    let redis_url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL required");
    let valkey = crate::valkey_client::connect(&redis_url).await.unwrap();
    Some((db, valkey, redis_url))
}

/// A [`Queue`] that records every enqueued message instead of touching Valkey.
#[derive(Default)]
pub struct RecordingQueue {
    pub sent: std::sync::Mutex<Vec<(QueueName, JobMessage)>>,
}

#[async_trait::async_trait]
impl Queue for RecordingQueue {
    async fn enqueue(&self, queue: QueueName, job: JobMessage) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push((queue, job));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::ActiveValue;

    #[test]
    fn builders_set_expected_defaults() {
        let org = new_organization();
        assert_eq!(org.slug, ActiveValue::Set("test-org".to_string()));

        let user = new_user(1);
        assert_eq!(user.org_id, ActiveValue::Set(1));
        assert_eq!(
            user.email,
            ActiveValue::Set("test-user@example.com".to_string())
        );

        let group = new_group(1, "default", vec!["view_query".to_string()]);
        assert_eq!(group.name, ActiveValue::Set("default".to_string()));

        let ds = new_data_source(1);
        assert_eq!(ds.r#type, ActiveValue::Set("pg".to_string()));

        let q = new_query(1, 2, 3);
        assert_eq!(q.data_source_id, ActiveValue::Set(2));
        assert_eq!(q.user_id, ActiveValue::Set(3));

        let dash = new_dashboard(1, 3);
        assert_eq!(dash.user_id, ActiveValue::Set(3));

        let snippet = new_query_snippet(1, 3);
        assert_eq!(snippet.org_id, ActiveValue::Set(1));
        assert_eq!(snippet.user_id, ActiveValue::Set(3));
    }
}

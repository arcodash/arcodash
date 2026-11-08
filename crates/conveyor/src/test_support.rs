//! Shared test fixtures (real Postgres + Redis, real conveyor schema applied via this crate's own
//! throwaway migrator) for the inline `#[cfg(test)]` modules across this crate.

use chrono::{DateTime, Utc};
use redis::aio::ConnectionManager;
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use sea_orm_migration::MigratorTrait;

use crate::entities::schedule as schedule_entity;
use crate::schema::Migrator;

pub(crate) async fn setup() -> Option<(DatabaseConnection, ConnectionManager, String)> {
    let database_url = std::env::var("TEST_DATABASE_URL").ok()?;
    let redis_url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL required");
    let db = sea_orm::Database::connect(&database_url).await.unwrap();
    Migrator::up(&db, None).await.unwrap();
    let client = redis::Client::open(redis_url.clone()).unwrap();
    let valkey = client.get_connection_manager().await.unwrap();
    Some((db, valkey, redis_url))
}

/// A random, practically-unique `entity_id` — cheap test isolation without seeding a parent row
/// (conveyor's tables have no domain FKs).
pub(crate) fn unique_entity_id() -> i64 {
    (uuid::Uuid::new_v4().as_u128() % (i64::MAX as u128 / 2)) as i64
}

pub(crate) struct InsertTask<'a> {
    pub task_type: &'a str,
    pub entity_id: Option<i64>,
    pub schedule_id: Option<i64>,
    pub payload: Option<serde_json::Value>,
    pub resource_key: Option<&'a str>,
    pub resource_cap: Option<i64>,
    pub status: &'a str,
    pub attempt: i32,
    pub waiting_on_task_id: Option<i64>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub enqueued_at: Option<DateTime<Utc>>,
}

impl Default for InsertTask<'_> {
    fn default() -> Self {
        InsertTask {
            task_type: "toy",
            entity_id: None,
            schedule_id: None,
            payload: None,
            resource_key: None,
            resource_cap: None,
            status: "pending",
            attempt: 1,
            waiting_on_task_id: None,
            lease_expires_at: None,
            created_at: None,
            started_at: None,
            enqueued_at: None,
        }
    }
}

pub(crate) async fn insert_task_full(db: &DatabaseConnection, t: InsertTask<'_>) -> i64 {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO conveyor_tasks \
             (task_type, entity_id, schedule_id, payload, resource_key, resource_cap, status, \
              attempt, waiting_on_task_id, lease_expires_at, started_at, created_at, enqueued_at, \
              triggered_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, COALESCE($12, now()), $13, \
                     'enqueue') \
             RETURNING id",
            [
                t.task_type.into(),
                t.entity_id.into(),
                t.schedule_id.into(),
                t.payload.into(),
                t.resource_key.map(|s| s.to_string()).into(),
                t.resource_cap.into(),
                t.status.into(),
                t.attempt.into(),
                t.waiting_on_task_id.into(),
                t.lease_expires_at.into(),
                t.started_at.into(),
                t.created_at.into(),
                t.enqueued_at.into(),
            ],
        ))
        .await
        .unwrap()
        .unwrap();
    row.try_get("", "id").unwrap()
}

pub(crate) async fn insert_task(
    db: &DatabaseConnection,
    task_type: &str,
    entity_id: Option<i64>,
    resource_key: Option<&str>,
    resource_cap: Option<i64>,
    status: &str,
) -> i64 {
    insert_task_full(
        db,
        InsertTask {
            task_type,
            entity_id,
            resource_key,
            resource_cap,
            status,
            ..Default::default()
        },
    )
    .await
}

pub(crate) async fn insert_schedule(
    db: &DatabaseConnection,
    task_type: &str,
    entity_id: Option<i64>,
    spec: serde_json::Value,
    overlap_policy: &str,
    next_run_at: DateTime<Utc>,
    consecutive_failures: i32,
) -> i64 {
    let model = schedule_entity::ActiveModel {
        task_type: Set(task_type.to_string()),
        entity_id: Set(entity_id),
        payload: Set(serde_json::json!({})),
        spec: Set(spec),
        timezone: Set("UTC".to_string()),
        enabled: Set(true),
        overlap_policy: Set(overlap_policy.to_string()),
        next_run_at: Set(Some(next_run_at.into())),
        consecutive_failures: Set(consecutive_failures),
        ..Default::default()
    };
    model.insert(db).await.unwrap().id
}

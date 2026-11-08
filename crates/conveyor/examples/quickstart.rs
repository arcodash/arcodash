use arcodash_conveyor::schedule_spec::ScheduleSpec;
use arcodash_conveyor::{
    ConveyorBuilder, EnqueueRequest, OverlapPolicy, ScheduleRequest, TaskContext, TaskOutcome,
    TaskResult, TypedTaskHandler,
};
use async_trait::async_trait;
use sea_orm_migration::MigratorTrait;
use serde::Deserialize;

#[derive(Deserialize)]
struct SendEmail {
    to: String,
    subject: String,
}

struct SendEmailHandler;

#[async_trait]
impl TypedTaskHandler for SendEmailHandler {
    type Payload = SendEmail;

    async fn run(&self, ctx: TaskContext, p: SendEmail) -> TaskResult {
        println!(
            "task {} attempt {}: mail {} '{}'",
            ctx.task_id.0, ctx.attempt, p.to, p.subject
        );
        Ok(TaskOutcome::Success { result_ref: None })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db = sea_orm::Database::connect("postgres://app:app@localhost:5432/app").await?;
    arcodash_conveyor::schema::Migrator::up(&db, None).await?;

    let redis_url = "redis://localhost:6379".to_string();
    let valkey = redis::Client::open(redis_url.as_str())?
        .get_connection_manager()
        .await?;

    let conveyor = ConveyorBuilder::new(db, valkey, redis_url)
        .register("send_email", SendEmailHandler)
        .tier("email", &["send_email"], 4)
        .build();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let runner = {
        let conveyor = conveyor.clone();
        tokio::spawn(async move { conveyor.run(shutdown_rx).await })
    };

    let queued = conveyor
        .enqueue(EnqueueRequest {
            task_type: "send_email".into(),
            payload: serde_json::json!({ "to": "ada@example.com", "subject": "Hi" }),
            tenant_id: None,
            entity_id: None,
            resource_key: None,
            resource_cap: None,
        })
        .await?;
    println!(
        "enqueued task {} (queued: {})",
        queued.task_id.0, queued.queued
    );

    conveyor
        .upsert_schedule(ScheduleRequest {
            task_type: "send_email".into(),
            entity_id: Some(42),
            payload: serde_json::json!({ "to": "team@example.com", "subject": "Daily report" }),
            spec: ScheduleSpec::At {
                time: "09:00".into(),
                days: Some(vec!["mon".into(), "fri".into()]),
            },
            timezone: "Europe/London".into(),
            overlap_policy: OverlapPolicy::Skip,
            tenant_id: None,
            resource_key: None,
            resource_cap: None,
        })
        .await?;

    tokio::signal::ctrl_c().await?;
    shutdown_tx.send(true)?;
    runner.await?;
    Ok(())
}

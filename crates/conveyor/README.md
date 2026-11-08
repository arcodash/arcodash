# conveyor

**A durable task queue and scheduler for Rust, backed by Postgres and Redis/Valkey.**

conveyor runs background work for async Rust services. Every task is a row in Postgres, so it survives restarts and you can query it. A Redis (or Valkey) list moves tasks to workers with low latency. You register a handler per task type, group task types into tiers with their own concurrency limit, and get back one `Conveyor` handle. Use it to enqueue tasks, create recurring schedules, and read back status.

It is the task engine inside [arcodash](https://www.arcodash.com). It knows nothing about arcodash, so you can use it in any Tokio service that already talks to Postgres and Redis.

```text
 enqueue() ──► conveyor_tasks (Postgres) ──RPUSH──► arcodash:queue:<type> (Redis) ──BLPOP──► handler
                    ▲      │                                                           │
     scheduler ─────┘      └──── lease / heartbeat / sweeper ◄─── outcome ◄────────────┘
 (conveyor_schedules)
```

## Features

- **Postgres holds every task.** A task row moves through `pending → running → success | failed |
  timed_out | cancelled | skipped`. Each step is a compare-and-swap in SQL, so two workers can never both claim the same task.
- **Tasks are delivered quickly over Redis.** Workers block on `BLPOP`, so a task starts running as soon as it is pushed. Nothing polls the database for work.
- **Concurrency is set per tier.** A tier is a named group of task types with one concurrency limit. Slow work cannot take the slots that fast work needs.
- **You can cap concurrency per resource.** Give tasks a `resource_key`, for example one database connection or one tenant, and a cap. Tasks over the cap wait as `pending` and start when a slot frees up.
- **At most one task per entity is in flight.** If a task with the same `(task_type, entity_id)`
  is already pending or running, `enqueue` refuses the new one with `AlreadyInflight`.
- **Recurring schedules.** Three kinds: fixed intervals, 6-field cron expressions, and an `at` shorthand (`"09:30"` on given weekdays). All of them understand time zones.
- **Failing schedules back off and then pause.** After each consecutive failure, the next run waits longer, doubling from 60 s up to 1 h. After a set number of consecutive failures (10 by default), the schedule pauses itself.
- **Crash recovery.** A running task holds a lease that a heartbeat keeps renewing. If the worker dies, a sweeper reclaims its task. The sweeper also re-pushes tasks that never reached their queue.
- **One scheduler across the fleet.** Every process can call `run()`. A leader lock in Redis makes sure only one of them fires schedules and runs the sweeper at a time.
- **Tasks can wait on other tasks.** A handler can return `WaitOn { task_id }`. Its task then pauses without holding a worker slot. When the other task finishes, the paused task runs again with that task's result.
- **Graceful shutdown.** On shutdown, each tier stops taking new work. It then waits for running tasks to finish, up to a grace period you set per tier.
- **Built-in maintenance.** Finished tasks are deleted after a retention window, and an hourly stats table is rolled up for you.

## Installation

conveyor is not published to crates.io yet. Add it as a git dependency:

```toml
[dependencies]
arcodash-conveyor = { git = "https://github.com/arcodash/arcodash", package = "arcodash-conveyor" }
```

### Requirements

- Rust (stable) on Tokio
- PostgreSQL
- Redis or Valkey. Workers use `BLPOP` and the scheduler uses pub/sub, so each worker opens its own connections.
- [SeaORM](https://www.sea-ql.org/SeaORM/) `DatabaseConnection` and a `redis`
  `aio::ConnectionManager`. These are the handles conveyor is built with.

## Quick start

```rust
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
        println!("task {} attempt {}: mail {} '{}'", ctx.task_id.0, ctx.attempt, p.to, p.subject);
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

    // Consumers + scheduler run until the shutdown signal flips to `true`.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let runner = {
        let conveyor = conveyor.clone();
        tokio::spawn(async move { conveyor.run(shutdown_rx).await })
    };

    // One-off task.
    conveyor
        .enqueue(EnqueueRequest {
            task_type: "send_email".into(),
            payload: serde_json::json!({ "to": "ada@example.com", "subject": "Hi" }),
            tenant_id: None,
            entity_id: None,
            resource_key: None,
            resource_cap: None,
        })
        .await?;

    // Recurring task: Mondays and Fridays at 09:00 London time.
    conveyor
        .upsert_schedule(ScheduleRequest {
            task_type: "send_email".into(),
            entity_id: Some(42),
            payload: serde_json::json!({ "to": "team@example.com", "subject": "Daily report" }),
            spec: ScheduleSpec::At { time: "09:00".into(), days: Some(vec!["mon".into(), "fri".into()]) },
            timezone: "Europe/London".into(),
            overlap_policy: OverlapPolicy::Skip,
            tenant_id: None,
            resource_key: None,
            resource_cap: None,
        })
        .await?;

    tokio::signal::ctrl_c().await?;
    shutdown_tx.send(true)?;
    runner.await?; // returns once every tier has drained
    Ok(())
}
```

The same program is in [`examples/quickstart.rs`](examples/quickstart.rs):

```bash
cargo run -p arcodash-conveyor --example quickstart
```

It connects to `postgres://app:app@localhost:5432/app` and `redis://localhost:6379`. Edit those two
URLs in the file to point at your own Postgres and Redis.

## Concepts

### Task types and handlers

A task type is a string, such as `"send_email"`. It names both the Redis queue (`arcodash:queue:send_email`) and the handler that runs the task. To write a handler, implement one of these two traits:

| Trait | Payload | Use when |
| --- | --- | --- |
| `TypedTaskHandler` | Any `DeserializeOwned` type | Most handlers. The JSON payload is decoded for you, and a payload that fails to decode marks the task `failed`. |
| `TaskHandler` | Raw `serde_json::Value` | The handler has to look at the payload shape itself. |

Every handler receives a `TaskContext`: `task_id`, `task_type`, `attempt`, `tenant_id` and `entity_id`. It returns a `TaskResult`, which is a `Result<TaskOutcome, anyhow::Error>`:

| Outcome | Effect |
| --- | --- |
| `Success { result_ref }` | The task becomes `success`. `result_ref` is an optional JSON pointer to wherever you stored the result. |
| `Failed { error }` | The task becomes `failed` with that error message. |
| `Cancelled` | The task becomes `cancelled`. |
| `WaitOn { task_id }` | The task pauses until `task_id` finishes, then runs again (see below). |
| `Err(_)` or a panic | The error is caught and the task becomes `failed`. The worker keeps running. |

Failed tasks are **not retried automatically**. To try again, enqueue the task again or let its schedule fire again. This keeps every attempt visible as a separate row.

### Tiers

```rust
ConveyorBuilder::new(db, valkey, redis_url)
    .register("render_report", RenderReport)
    .register("send_email", SendEmail)
    .tier("email", &["send_email"], 8)                        // 8 concurrent, default drain grace
    .tier_with_drain("reports", &["render_report"], 2, 1800)  // 2 concurrent, 30 min drain grace
    .build();
```

- Each tier has one concurrency limit, shared by all of its task types.
- Each task type has its own consumer loop and its own blocking Redis connection.
- `build()` stops the program with a panic if the configuration is inconsistent, so mistakes show
  up at startup instead of in production. It checks for:
  - a task type registered twice,
  - a tier that names a task type with no handler,
  - a task type that appears in two tiers,
  - a registered task type that no tier runs.
- A process that only enqueues tasks, such as an API server, can build a `Conveyor` with no tiers and never call `run()`.

### Enqueueing

`Conveyor::enqueue` does two things. It inserts a `pending` row, then pushes the task onto Redis, unless the resource cap is already full. It returns
`Enqueued { task_id, queued }`. `queued == false` means the task is waiting over its resource cap, and the sweeper will push it once a slot frees.

| Field | Meaning |
| --- | --- |
| `task_type` | Which handler runs the task. |
| `payload` | JSON passed to the handler. |
| `entity_id` | Optional. If set, only one `pending` or `running` task per `(task_type, entity_id)` is allowed. A second `enqueue` fails with `AlreadyInflight`, which you can check with `err.downcast_ref::<AlreadyInflight>()`. `None` never conflicts. |
| `tenant_id` | Optional label for multi-tenant callers. Stored on the row and passed to the handler. |
| `resource_key` / `resource_cap` | Optional limit on concurrency. At most `resource_cap` tasks that share a `resource_key` are queued or running at a time. Without a cap, the default is `CONVEYOR_DEFAULT_RESOURCE_CAP` (4). The cap is soft: two leaders counting at the same moment can go over it by one. |

### Schedules

A schedule creates one task each time it fires. Its `spec` is serialized as tagged JSON:

| Spec | JSON | Notes |
| --- | --- | --- |
| `Interval { every_secs }` | `{"kind":"interval","every_secs":300}` | Minimum 60 s. Up to 10 s of random jitter is added so schedules with the same interval don't all fire at once. |
| `Cron { expr }` | `{"kind":"cron","expr":"0 30 9 * * MON-FRI"}` | 6-field cron expression, where the first field is seconds. |
| `At { time, days }` | `{"kind":"at","time":"09:30","days":["mon","thu"]}` | Shorthand that becomes a cron expression. Leave out `days` to run daily. |

`timezone` is an IANA name, such as `"America/New_York"`. Daylight-saving changes are handled.

- `schedule()` always creates a new schedule.
- `upsert_schedule()` is keyed on `(task_type, entity_id)`, so you can call it on every boot without creating duplicates.
  - It never un-pauses a schedule that someone paused.
  - It resets the failure counter and any auto-pause only when the `spec` changed.
- `pause_schedule_until`, `resume_schedule` and `cancel_schedule` cover the rest of the lifecycle.
  `resume_schedule` clears an auto-pause.

**Overlap.** If a schedule fires while its previous task is still pending or running, `OverlapPolicy::Skip` records a `skipped` row and does not run it. The row keeps the skipped run visible. `OverlapPolicy::Coalesce` is reserved and currently behaves like `Skip`.

### Waiting on another task

If a handler returns `TaskOutcome::WaitOn { task_id }`, its task becomes a waiter. It stays `running`, but it gives up its lease and its worker slot. When the task it waits on reaches any final state, the waiter is pushed back onto its queue. It then runs again with a payload of `{"done_task_id": …, "done_task_status": "success"}`. This lets you build fan-out and join flows without a worker sitting idle while it waits.

### Status and diagnostics

- `status(task_id)` returns the task's state, attempt, error and start and finish times.
- `live()` returns a snapshot that is capped in size:
  - the depth of each queue,
  - worker heartbeats,
  - running tasks,
  - pending tasks that look stuck,
  - the size of the dead-letter queue.

  It is meant for an admin page. Don't use it to list every task.
- An envelope whose task type has no registered handler is moved to `arcodash:queue:dead`, and its
  row is marked `failed`. It is not retried.

## How it works

1. **Claim.** A consumer pops a task with `BLPOP`. It then runs
   `UPDATE … SET status='running' WHERE status='pending'`. If that update changes no row, another
   worker already claimed the task, and the message is dropped.
2. **Lease.** While the handler runs, a heartbeat renews `lease_expires_at` every `CONVEYOR_LEASE_SECS / 3` seconds. Each handler call is also limited to `CONVEYOR_TASK_TIMEOUT_SECS`. A handler that runs longer ends as `timed_out`.
3. **Finalize.** Every final state goes through one function. It moves the row to its final status with a single compare-and-swap, which also frees its `(task_type, entity_id)` slot. It then updates schedule bookkeeping, wakes any waiters, and nudges the scheduler.
4. **Schedule.** The leader sleeps until the next schedule is due. Three things wake it early: a pub/sub message, a finished task, or the `CONVEYOR_MAX_SLEEP_SECS` limit. It then claims the due schedules with `FOR UPDATE SKIP LOCKED` and plans one task row for each.
5. **Sweep.** Every `CONVEYOR_SWEEP_INTERVAL_SECS`, the leader:
   - marks running tasks whose lease expired as `failed`,
   - pushes stale `pending` tasks again (crashed before enqueue, or waiting over the cap),
   - marks tasks enqueued longer ago than `CONVEYOR_ABANDONED_PENDING_SECS` as `timed_out`,
   - releases waiters whose task finished without waking them.
6. **Maintain.** Once an hour, the leader deletes finished tasks older than the retention window and rolls up `task_stats_hourly`.

## Database schema

conveyor owns three tables and creates no foreign keys into your schema:

| Table | Purpose |
| --- | --- |
| `conveyor_tasks` | One row per task attempt: status, payload, lease, worker, timings, error and result. |
| `conveyor_schedules` | Recurring schedules: spec, time zone, next run, overlap policy, pause and failure state. |
| `task_stats_hourly` | Hourly counts and durations for each task type. |

To apply them, use one of these:

- **Standalone:** `arcodash_conveyor::schema::Migrator::up(&db, None)`. It records its own history in `conveyor_seaql_migrations`, so it never touches your app's `seaql_migrations` table.
- **Inside your own SeaORM migrator:** include the migrations from `arcodash_conveyor::schema`, such as `CreateConveyorTables`. Every index is created with `IF NOT EXISTS`, so both migrators can run against the same database.

## Configuration

Every setting is an optional environment variable read at runtime. A value that can't be parsed
logs an error and falls back to the default; it never stops the process.

| Variable | Default | Description |
| --- | --- | --- |
| `CONVEYOR_LEASE_SECS` | `90` | Lease on a running task. The heartbeat renews it every lease/3 seconds; the sweeper reclaims the task once it expires. |
| `CONVEYOR_TASK_TIMEOUT_SECS` | `21600` | Longest a single handler call may run before it ends as `timed_out`. |
| `CONVEYOR_SWEEP_INTERVAL_SECS` | `60` | How often the leader runs the sweeper. |
| `CONVEYOR_STALE_PENDING_SECS` | `60` | How old a `pending` task must be before the sweeper pushes it again. |
| `CONVEYOR_ABANDONED_PENDING_SECS` | `3600` | How long an enqueued task can go unclaimed before it ends as `timed_out`. Must be longer than your longest real queue wait. |
| `CONVEYOR_DEFAULT_RESOURCE_CAP` | `4` | Cap used when a task sets `resource_key` but no `resource_cap`. |
| `CONVEYOR_AUTO_PAUSE_THRESHOLD` | `10` | Consecutive failures before a schedule pauses itself. |
| `CONVEYOR_MAX_SLEEP_SECS` | `120` | Longest the scheduler sleeps between checks. |
| `CONVEYOR_CLAIM_LIMIT` | `100` | Most due schedules claimed in one check. |
| `CONVEYOR_SWEEP_BATCH_LIMIT` | `2000` | Most rows one sweep query handles. The rest wait for the next pass. |
| `CONVEYOR_TASK_RETENTION_DAYS` | `14` | Days to keep finished tasks. |
| `CONVEYOR_PRUNE_BATCH_LIMIT` | `5000` | Rows deleted per `DELETE` statement while pruning. |
| `CONVEYOR_PRUNE_MAX_PER_CYCLE` | `100000` | Most rows deleted in one maintenance cycle. |
| `CONVEYOR_LIVE_SNAPSHOT_LIMIT` | `500` | Most rows per category in `live()`. |
| `WORKER_ID` | `$HOSTNAME`, then `$HOST`, then a random id | Worker identity recorded on claimed tasks and heartbeats. Set it, or `HOSTNAME`, to a value that is stable and different on every replica. |

Per-tier settings, such as concurrency and drain grace, are set in code on `ConveyorBuilder`.

## Observability

conveyor logs with [`tracing`](https://docs.rs/tracing). It also keeps process-wide `AtomicU64` counters that you can expose through your own metrics endpoint:

| Counter | Counts |
| --- | --- |
| `SWEEPER_RECLAIMED_EXPIRED_LEASE` | Running tasks failed after their worker stopped heartbeating. |
| `SWEEPER_RECLAIMED_STALE_PENDING` | Pending tasks the sweeper pushed again. |
| `SWEEPER_RECLAIMED_ABANDONED_PENDING` | Enqueued tasks that were never claimed and ended as `timed_out`. |
| `SWEEPER_RELEASED_ORPHANED_WAITER` | Waiters released after a missed wake-up. |
| `HEARTBEAT_LEASE_LOST` | Heartbeats that found their lease was already gone. |
| `FINALIZE_LOST_RACE` | Outcomes thrown away because another path had already finalized the task. |
| `SCHEDULE_AUTO_PAUSED` | Schedules that paused themselves after repeated failures. |
| `LEADER_ACQUIRE_CONTENDED` / `LEADER_ACQUIRE_FAILED` | Leader-lock attempts lost to another process, or failed because of Redis errors. |

## Guarantees and limits

- **Delivery.** A task row is claimed at most once. A task whose worker crashes ends as `failed`; it is not run a second time. If you need retries, enqueue the task again.
- **Ordering.** Each task type's queue is first-in, first-out. Across tiers and workers, there is no ordering.
- **Resource caps** are soft. They can go over by one when two leaders count at the same moment.
- **Payloads** are stored as `jsonb` and copied into the Redis envelope, so keep them small. Store large inputs elsewhere and pass a reference.
- **Redis keys** use the `arcodash:` prefix. If several unrelated apps share one Redis, give each its own logical database.

## Development

The tests run against a real Postgres and Redis:

```bash
docker compose -f docker-compose.testing.yml up -d --wait postgres valkey   # Postgres :5433, Redis :6380
TEST_DATABASE_URL=postgres://arcodash:arcodash@localhost:5433/arcodash_test \
TEST_REDIS_URL=redis://localhost:6380 \
  cargo test -p arcodash-conveyor -- --test-threads=1
```

If `TEST_DATABASE_URL` is not set, the tests that need a database are skipped, and the pure unit tests still run (`schedule_spec`, builder validation, config parsing).

## License

[AGPL-3.0](../../LICENSE.md), part of the [arcodash](https://github.com/arcodash/arcodash)
project. Contributions are welcome; see [CONTRIBUTING.md](../../CONTRIBUTING.md).

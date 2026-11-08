//! Daily schema-only profile refresh for sources in orgs that used AI recently. Each source runs
//! in the hour `id % 24`; metered connectors are excluded because even schema metadata is billed.

use arcodash_core::models::{is_metered, ProfileData};
use arcodash_core::queue::enqueue_profile;
use sea_orm::{DbBackend, FromQueryResult, Statement};

use crate::handlers::execute_query::ExecuteQueryContext;

#[derive(Debug, FromQueryResult)]
struct Candidate {
    id: i64,
    org_id: i64,
    r#type: String,
}

fn due(rows: Vec<Candidate>, hour: u32) -> Vec<Candidate> {
    rows.into_iter()
        .filter(|c| c.id.rem_euclid(24) == i64::from(hour) && !is_metered(&c.r#type))
        .collect()
}

/// Returns how many jobs were newly enqueued (sources with one already pending are skipped).
pub async fn enqueue_daily_schema_passes(
    ctx: &ExecuteQueryContext,
    hour: u32,
) -> anyhow::Result<usize> {
    let rows = Candidate::find_by_statement(Statement::from_string(
        DbBackend::Postgres,
        "SELECT ds.id, ds.org_id, ds.type FROM data_sources ds \
         WHERE ds.paused_at IS NULL AND ds.type <> 'query_results' \
           AND EXISTS (SELECT 1 FROM ai_usage u WHERE u.org_id = ds.org_id \
                       AND u.created_at > now() - interval '30 days')",
    ))
    .all(&ctx.db)
    .await?;

    let mut redis = ctx.redis.clone();
    let mut n = 0;
    for c in due(rows, hour) {
        match enqueue_profile(
            &mut redis,
            ctx.queue.as_ref(),
            c.org_id,
            c.id,
            ProfileData::None,
        )
        .await
        {
            Ok(true) => n += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, data_source_id = c.id, "daily schema pass enqueue failed");
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcodash_core::entities::{ai_usage, data_source};
    use arcodash_core::queue::{JobMessage, Queue, QueueName};
    use sea_orm::prelude::DateTimeWithTimeZone;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    use std::sync::{Arc, Mutex};

    fn cand(id: i64, ty: &str) -> Candidate {
        Candidate {
            id,
            org_id: 1,
            r#type: ty.into(),
        }
    }

    #[test]
    fn due_picks_hour_and_drops_metered() {
        let rows = vec![
            cand(5, "pg"),
            cand(29, "mysql"),
            cand(6, "pg"),
            cand(5 + 48, "bigquery"),
            cand(5 + 72, "redshift"),
        ];
        let ids: Vec<i64> = due(rows, 5).into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![5, 29]);
    }

    #[derive(Default)]
    struct Capture(Mutex<Vec<JobMessage>>);

    #[async_trait::async_trait]
    impl Queue for Capture {
        async fn enqueue(&self, _q: QueueName, job: JobMessage) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(job);
            Ok(())
        }
    }

    async fn insert_ds(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        ty: &str,
        paused: bool,
        hour: i64,
    ) -> data_source::Model {
        let now = DateTimeWithTimeZone::from(chrono::Utc::now());
        // Burn ids until one lands in the wanted hour.
        loop {
            let ds = data_source::ActiveModel {
                org_id: Set(org_id),
                name: Set(format!("daily-{}", uuid::Uuid::new_v4())),
                r#type: Set(ty.to_string()),
                paused_at: Set(paused.then_some(now)),
                created_at: Set(now),
                updated_at: Set(now),
                ..Default::default()
            }
            .insert(db)
            .await
            .unwrap();
            if ds.id.rem_euclid(24) == hour {
                return ds;
            }
            data_source::Entity::delete_by_id(ds.id)
                .exec(db)
                .await
                .unwrap();
        }
    }

    async fn use_ai(db: &sea_orm::DatabaseConnection, org_id: i64, days_ago: i64) {
        ai_usage::ActiveModel {
            org_id: Set(org_id),
            provider: Set("test".into()),
            model: Set("test".into()),
            feature: Set("ask".into()),
            status: Set("ok".into()),
            created_at: Set(DateTimeWithTimeZone::from(
                chrono::Utc::now() - chrono::Duration::days(days_ago),
            )),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn enqueues_only_active_unpaused_non_metered_sources() {
        use crate::scheduler::test_support::*;
        let (Ok(db_url), Ok(redis_url)) = (
            std::env::var("TEST_DATABASE_URL"),
            std::env::var("TEST_REDIS_URL"),
        ) else {
            eprintln!("skipping: TEST_DATABASE_URL/TEST_REDIS_URL not set");
            return;
        };
        // Backend suites TRUNCATE data_sources; the advisory lock keeps the seeded rows alive.
        let _lock = acquire_shared_test_db_lock(&db_url).await;
        let db = arcodash_core::db::connect(&db_url).await.unwrap();
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();
        let capture = Arc::new(Capture::default());
        let mut ctx = test_ctx(db.clone(), redis.clone());
        ctx.queue = capture.clone();

        let hour = 7;
        let active = seed_org(&db, &format!("org-daily-{}", uuid::Uuid::new_v4())).await;
        let stale = seed_org(&db, &format!("org-daily-{}", uuid::Uuid::new_v4())).await;
        use_ai(&db, active.id, 5).await;
        use_ai(&db, stale.id, 40).await;
        let pg = insert_ds(&db, active.id, "pg", false, hour).await;
        let bq = insert_ds(&db, active.id, "bigquery", false, hour).await;
        let paused = insert_ds(&db, active.id, "pg", true, hour).await;
        let old = insert_ds(&db, stale.id, "pg", false, hour).await;

        let mut conn = redis.clone();
        let keys: Vec<String> = [&pg, &bq, &paused, &old]
            .iter()
            .map(|d| arcodash_core::models::ai_profile_pending_key(d.id))
            .collect();
        let _: () = redis::AsyncCommands::del(&mut conn, &keys).await.unwrap();

        enqueue_daily_schema_passes(&ctx, hour as u32)
            .await
            .unwrap();

        let mine = [pg.id, bq.id, paused.id, old.id];
        let got: Vec<(i64, ProfileData)> = capture
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|j| match j {
                JobMessage::ProfileDataSource {
                    data_source_id,
                    data,
                    ..
                } if mine.contains(data_source_id) => Some((*data_source_id, *data)),
                _ => None,
            })
            .collect();
        let _: () = redis::AsyncCommands::del(&mut conn, &keys).await.unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, pg.id);
        assert!(matches!(got[0].1, ProfileData::None));
    }
}

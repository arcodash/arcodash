// `async_trait`'s `MigrationTrait` impls can't name `SchemaManager`'s lifetime (E0195).
#![allow(elided_lifetimes_in_paths)]

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::DatabaseConnection;
use std::collections::HashSet;

pub mod m20260716_000000_baseline;
pub mod m20260717_000001_task_stats_hourly;
pub mod m20260722_000001_users_email_ci_unique;
pub mod m20260722_000002_snippets_destinations_perms;
pub mod m20260725_000001_hash_api_keys;
pub mod m20260729_000001_data_source_uploads;
pub mod m20260729_000002_ai_usage;
pub mod m20260730_000001_schedule_entity_cleanup;
pub mod m20260804_000001_data_source_health_monitor_reason;
pub mod m20260805_000001_digest_hour_timezone;
pub mod m20260806_000001_query_unpublish;
pub mod m20260809_000001_viewer_drop_view_users;
pub mod m20260811_000001_session_token_version;
pub mod m20260812_000001_seed_viewers_group;
pub mod m20260812_000002_runs_enqueued_at;
pub mod m20260814_000001_org_fk_cascade;
pub mod m20260822_000001_digest_last_sent_at;
pub mod m20260830_000001_conveyor_null_entity_indexes;
pub mod m20260907_000001_conveyor_live_index_resource_key;
pub mod m20260907_000002_conveyor_drop_job_id;
pub mod m20260910_000001_notification_deliveries_stale_retrying_index;
pub mod m20260910_000002_conveyor_tasks_sweep_indexes;
pub mod m20260915_000001_groups_restricted_default;
mod m20260930_000001_data_source_ai_profile;
mod m20261004_000001_monitor_observations;
mod m20261005_000001_data_source_ai_profile_last_error;
mod m20261009_000001_ai_usage_cost;
mod m20261009_000002_dashboard_narratives;
mod m20261013_000001_dashboard_narrative_body;
pub mod track;

pub async fn assert_track(db: &DatabaseConnection) -> Result<(), String> {
    if track::current(db)
        .await
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return track::claim(db, track::OSS).await;
    }

    // Unmarked: only claim if nothing in the ledger contradicts "this is an oss database".
    // A version this crate has no migration file for means something else (another migrator)
    // wrote it, so stamping "oss" here would be a guess, not a fact — and the whole point of
    // the marker is to never be wrong.
    let own: HashSet<String> = Migrator::migrations()
        .iter()
        .map(|m| m.name().to_string())
        .collect();
    let applied = track::ledger_versions(db)
        .await
        .map_err(|e| e.to_string())?;
    let has_foreign_migrations = applied.iter().any(|v| !own.contains(v));
    let overridden = track::overridden_for(
        std::env::var("ARCODASH_SCHEMA_TRACK").ok().as_deref(),
        track::OSS,
    );
    if has_foreign_migrations && !overridden {
        return Err(adoption_veto_message());
    }

    track::claim(db, track::OSS).await
}

fn adoption_veto_message() -> String {
    "refusing to migrate: this database has migration ledger entries arcodash-migration has no\n\
files for, and is not yet marked with a migration track. It is not an oss-track database.\n\
\n\
DATABASE_URL is pointing at the wrong database. Applying oss migrations here would stamp it\n\
'oss' when it isn't, and this migrator would then permanently refuse it afterwards.\n\
\n\
If this really is the oss database, claim it explicitly for this one run:\n\
  ARCODASH_SCHEMA_TRACK=oss arcodash-migration up"
        .to_string()
}

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260716_000000_baseline::Migration),
            Box::new(m20260717_000001_task_stats_hourly::Migration),
            Box::new(m20260722_000001_users_email_ci_unique::Migration),
            Box::new(m20260722_000002_snippets_destinations_perms::Migration),
            Box::new(m20260725_000001_hash_api_keys::Migration),
            Box::new(m20260729_000001_data_source_uploads::Migration),
            Box::new(m20260729_000002_ai_usage::Migration),
            Box::new(m20260730_000001_schedule_entity_cleanup::Migration),
            Box::new(m20260804_000001_data_source_health_monitor_reason::Migration),
            Box::new(m20260805_000001_digest_hour_timezone::Migration),
            Box::new(m20260806_000001_query_unpublish::Migration),
            Box::new(m20260809_000001_viewer_drop_view_users::Migration),
            Box::new(m20260811_000001_session_token_version::Migration),
            Box::new(m20260812_000001_seed_viewers_group::Migration),
            Box::new(m20260812_000002_runs_enqueued_at::Migration),
            Box::new(m20260814_000001_org_fk_cascade::Migration),
            Box::new(m20260822_000001_digest_last_sent_at::Migration),
            Box::new(m20260830_000001_conveyor_null_entity_indexes::Migration),
            Box::new(m20260907_000001_conveyor_live_index_resource_key::Migration),
            Box::new(m20260907_000002_conveyor_drop_job_id::Migration),
            Box::new(m20260910_000001_notification_deliveries_stale_retrying_index::Migration),
            Box::new(m20260910_000002_conveyor_tasks_sweep_indexes::Migration),
            Box::new(m20260915_000001_groups_restricted_default::Migration),
            Box::new(m20260930_000001_data_source_ai_profile::Migration),
            Box::new(m20261004_000001_monitor_observations::Migration),
            Box::new(m20261005_000001_data_source_ai_profile_last_error::Migration),
            Box::new(m20261009_000001_ai_usage_cost::Migration),
            Box::new(m20261009_000002_dashboard_narratives::Migration),
            Box::new(m20261013_000001_dashboard_narrative_body::Migration),
        ]
    }
}

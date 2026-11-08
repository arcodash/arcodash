//! The single baseline migration: creates the full schema on a fresh database, then seeds the
//! default org and its builtin groups/roles.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};

use arcodash_core::entities::organization;
use arcodash_core::org_seed;

/// Every table the baseline creates, in reverse creation order — used by `down` to tear the
/// schema back down. `CASCADE` handles foreign-key and sequence dependencies.
const TABLES: &[&str] = &[
    "widgets",
    "visualizations",
    "users",
    "schedules",
    "runs",
    "roles",
    "query_snippets",
    "query_results",
    "queries",
    "organizations",
    "org_embed_settings",
    "notification_destinations",
    "notification_deliveries",
    "monitors",
    "monitor_subscriptions",
    "mail_deliveries",
    "groups",
    "group_roles",
    "favorites",
    "events",
    "escalation_steps",
    "embeds",
    "data_sources",
    "data_source_groups",
    "dashboards",
    "conveyor_tasks",
    "conveyor_schedules",
    "changes",
    "api_keys",
    "access_permissions",
];

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(include_str!("m20260716_000000_baseline.sql"))
            .await?;

        // Admin user bootstrap is an application-level first-run concern, not seeded here.
        db.execute_unprepared(
            "INSERT INTO organizations (name, slug, settings) \
             VALUES ('Default', 'default', '{}') \
             ON CONFLICT (slug) DO NOTHING",
        )
        .await?;

        let org = organization::Entity::find()
            .filter(organization::Column::Slug.eq("default"))
            .one(db)
            .await?
            .expect("'default' org row was just inserted (or already existed) above");

        org_seed::seed_builtin_groups(db, org.id).await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if std::env::var("ARCODASH_ALLOW_DESTRUCTIVE_DOWN").as_deref() != Ok("1") {
            return Err(DbErr::Custom(
                "refusing to drop every table: set ARCODASH_ALLOW_DESTRUCTIVE_DOWN=1 to proceed"
                    .into(),
            ));
        }
        let db = manager.get_connection();
        for table in TABLES {
            db.execute_unprepared(&format!("DROP TABLE IF EXISTS public.{table} CASCADE"))
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::{MigrationTrait, SchemaManager};

    use super::Migration;

    #[tokio::test]
    async fn down_refuses_without_the_destructive_env_var() {
        let Ok(database_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        std::env::remove_var("ARCODASH_ALLOW_DESTRUCTIVE_DOWN");
        let conn = arcodash_core::db::connect(&database_url).await.unwrap();
        let manager = SchemaManager::new(&conn);
        let err = Migration
            .down(&manager)
            .await
            .expect_err("down() must refuse to run without the env var set");
        assert!(err.to_string().contains("ARCODASH_ALLOW_DESTRUCTIVE_DOWN"));
    }
}

//! `DescribeDataSource` handler: one LLM call that fills missing column descriptions in
//! `data_sources.ai_profile`, merged only if the profile is unchanged since it was read.
//! Descriptions are model output that later re-enters prompts, so they are flattened and capped
//! on merge. Prompts and sample values are never logged.

use arcodash_core::ai::describe::{
    build_describe_prompt, describe_prompt_lists_columns, merge_descriptions, needs_descriptions,
    SchemaDescriptions,
};
use arcodash_core::ai::fallback::run_fallback_chain;
use arcodash_core::ai::narrative::report_unreachable;
use arcodash_core::ai::profile_values::{load_ai_profile, save_profile_values, split_profile};
use arcodash_core::ai::settings::{
    fallback_enabled, include_sample_values, resolve_provider_chain, ResolvedProvider,
};
use arcodash_core::ai::{AiError, AiProvider, AiProviderKind};
use arcodash_core::connectors::TableSchema;
use arcodash_core::entities::{data_source, organization};
use arcodash_core::models::{ai_profile_describing_key, DataSourceAiProfile, DataSourceSchema};
use arcodash_core::org_storage::OrgStorage;
use sea_orm::{ConnectionTrait, DatabaseBackend, EntityTrait, Statement};

use crate::ai_sink::DbAttemptSink;
use crate::handlers::execute_query::ExecuteQueryContext;

/// In-flight guard: one describe call per source at a time.
const GUARD_TTL_SECS: u64 = 600;
/// After a failed or empty run the guard is stretched to this, so a schema the model cannot
/// describe costs one prompt per window.
const FAILURE_BACKOFF_SECS: u64 = 21_600;

pub async fn handle(ctx: &ExecuteQueryContext, org_id: i64, data_source_id: i64) {
    let Some(ai) = ctx.ai.as_ref() else {
        return;
    };
    if let Err(e) = run(ctx, ai, org_id, data_source_id).await {
        tracing::warn!(org_id, data_source_id, error = %e, "describe_data_source failed");
    }
}

async fn run(
    ctx: &ExecuteQueryContext,
    ai: &crate::ai_sink::AiRuntime,
    org_id: i64,
    data_source_id: i64,
) -> anyhow::Result<()> {
    let (org, ds) = load(ctx, org_id, data_source_id).await?;
    if !ctx.entitlements.ai_features(&org) {
        return Ok(());
    }
    let chain = resolve_provider_chain(&org, &ai.config)?;
    describe_if_needed(ctx, &ai.http, ai.provider_lookup, &org, &ds, chain).await;
    Ok(())
}

async fn load(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    data_source_id: i64,
) -> anyhow::Result<(organization::Model, data_source::Model)> {
    let ds = data_source::Entity::find_by_id(data_source_id)
        .one(&ctx.db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("data source {data_source_id} not found"))?;
    // The worker's DB role bypasses RLS: this check is the only tenant boundary.
    if ds.org_id != org_id {
        anyhow::bail!("data source {data_source_id} does not belong to org {org_id}");
    }
    let org = organization::Entity::find_by_id(org_id)
        .one(&ctx.db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("org {org_id} not found"))?;
    Ok((org, ds))
}

/// Takes the per-source guard, makes one `describe_schema` call, and merges the result. Every
/// failure is swallowed with an id-only warning and stretches the guard to the failure back-off.
pub async fn describe_if_needed<F>(
    ctx: &ExecuteQueryContext,
    http: &reqwest::Client,
    provider_lookup: F,
    org: &organization::Model,
    ds: &data_source::Model,
    chain: Vec<ResolvedProvider>,
) where
    F: Fn(AiProviderKind, Option<String>) -> Box<dyn AiProvider>,
{
    let Some(raw_profile) = &ds.ai_profile else {
        return;
    };
    let storage = match ctx.storage.resolve(&ctx.db, org.id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(data_source_id = ds.id, error = %e, "describe: storage unavailable");
            return;
        }
    };
    let profile = match load_ai_profile(&storage, ds).await {
        Ok(Some(p)) => p,
        Ok(None) => return,
        Err(e) => {
            report_unreachable(&*ctx.storage, &ctx.db, org.id, &e).await;
            tracing::warn!(data_source_id = ds.id, error = %e, "describe: profile values unavailable");
            return;
        }
    };
    let Some(schema) = ds
        .schema
        .clone()
        .and_then(|v| serde_json::from_value::<DataSourceSchema>(v).ok())
    else {
        return;
    };
    if chain.is_empty() || !needs_descriptions(&profile) {
        return;
    }
    let tables: Vec<TableSchema> = schema
        .tables
        .into_iter()
        .map(|t| TableSchema {
            name: t.name,
            columns: t.columns,
        })
        .collect();
    let include_samples = include_sample_values(&org.settings);

    let mut redis = ctx.redis.clone();
    let reserved: Result<Option<String>, _> = redis::cmd("SET")
        .arg(ai_profile_describing_key(ds.id))
        .arg(1)
        .arg("NX")
        .arg("EX")
        .arg(GUARD_TTL_SECS)
        .query_async(&mut redis)
        .await;
    if !matches!(reserved, Ok(Some(_))) {
        return;
    }

    let prompt = build_describe_prompt(&tables, &profile, include_samples);
    // Remaining columns may never fit the prompt; the long guard keeps repeat jobs from
    // re-checking.
    if !describe_prompt_lists_columns(&prompt) {
        back_off(ctx, ds.id).await;
        return;
    }
    let sink = DbAttemptSink::new(&ctx.db, org.clone(), None);
    let (kind, outcome) = run_fallback_chain(
        http,
        &chain,
        fallback_enabled(&org.settings),
        "describe_schema",
        &prompt,
        provider_lookup,
        |provider, http, api_key, model, prompt| {
            provider.describe_schema(http, api_key, model, prompt)
        },
        |d: &SchemaDescriptions| (d.tokens_in, d.tokens_out),
        &sink,
    )
    .await;
    let descriptions = match outcome {
        Ok(d) => d,
        Err(e) => {
            if let AiError::BudgetExceeded { .. } = e {
                tracing::info!(
                    org_id = org.id,
                    data_source_id = ds.id,
                    "AI budget reached; skipping schema descriptions"
                );
            } else {
                tracing::warn!(
                    data_source_id = ds.id,
                    provider = kind.as_str(),
                    error = ?e,
                    "describe_schema failed"
                );
            }
            back_off(ctx, ds.id).await;
            return;
        }
    };

    let mut merged = profile;
    if merge_descriptions(&mut merged, &descriptions, include_samples) == 0 {
        back_off(ctx, ds.id).await;
        return;
    }
    if let Err(e) = store(ctx, &storage, ds, raw_profile, &merged).await {
        tracing::warn!(data_source_id = ds.id, error = %e, "storing descriptions failed");
    }
}

async fn back_off(ctx: &ExecuteQueryContext, data_source_id: i64) {
    let mut redis = ctx.redis.clone();
    let _: Result<i64, _> = redis::cmd("EXPIRE")
        .arg(ai_profile_describing_key(data_source_id))
        .arg(FAILURE_BACKOFF_SECS)
        .query_async(&mut redis)
        .await;
}

/// Compare-and-set on the whole stored profile: a rebuild in the meantime wins.
async fn store(
    ctx: &ExecuteQueryContext,
    storage: &OrgStorage,
    ds: &data_source::Model,
    raw_profile: &serde_json::Value,
    merged: &DataSourceAiProfile,
) -> anyhow::Result<()> {
    let (stripped, values) = split_profile(merged);
    // Descriptions never change values, so an existing object is left alone (a concurrent rebuild
    // may have replaced it); only a row still holding inline values moves them out.
    let location = match &ds.ai_profile_values_location {
        Some(l) => l.clone(),
        None => match save_profile_values(storage, ds.id, &values).await {
            Ok(l) => l,
            Err(e) => {
                report_unreachable(&*ctx.storage, &ctx.db, ds.org_id, &e).await;
                return Err(e.into());
            }
        },
    };
    let new = serde_json::to_value(&stripped)?;
    let res = ctx
        .db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "UPDATE data_sources SET ai_profile = $1, ai_profile_values_location = $2 \
             WHERE id = $3 AND org_id = $4 AND ai_profile = $5",
            [
                new.into(),
                location.into(),
                ds.id.into(),
                ds.org_id.into(),
                raw_profile.clone().into(),
            ],
        ))
        .await?;
    if res.rows_affected() == 0 {
        tracing::debug!(
            data_source_id = ds.id,
            "profile changed during describe; descriptions dropped"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::test_support::acquire_shared_test_db_lock;
    use crate::scheduler::test_support::{seed_data_source, seed_org, test_ctx};
    use arcodash_core::ai::describe::{ColumnDescription, TableDescriptions};
    use arcodash_core::entities::ai_usage;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, QueryFilter};
    use std::sync::{Arc, Mutex};

    struct RecProvider {
        prompts: Arc<Mutex<Vec<String>>>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl AiProvider for RecProvider {
        fn kind(&self) -> AiProviderKind {
            AiProviderKind::Gemini
        }
        fn default_model(&self) -> &'static str {
            "fake-model"
        }
        fn curated_models(&self) -> &'static [&'static str] {
            &[]
        }
        async fn test_key(&self, _: &reqwest::Client, _: &str, _: &str) -> Result<(), AiError> {
            unimplemented!()
        }
        async fn generate_sql(
            &self,
            _: &reqwest::Client,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<arcodash_core::ai::SqlGeneration, AiError> {
            unimplemented!()
        }
        async fn suggest_visualization(
            &self,
            _: &reqwest::Client,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<arcodash_core::ai::viz::VizGeneration, AiError> {
            unimplemented!()
        }
        async fn suggest_monitors(
            &self,
            _: &reqwest::Client,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<arcodash_core::ai::monitor_suggestions::MonitorSuggestionGeneration, AiError>
        {
            unimplemented!()
        }
        async fn describe_schema(
            &self,
            _: &reqwest::Client,
            _: &str,
            _: &str,
            prompt: &str,
        ) -> Result<SchemaDescriptions, AiError> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            if self.fail {
                return Err(AiError::GenerationFailed("boom".into()));
            }
            // Answers `status` plus every wide-table column (`c<digits>`) listed in the prompt.
            let mut columns = vec![ColumnDescription {
                column: "status".into(),
                description: "Fulfilment state\nof the order".into(),
            }];
            for tok in prompt.split(|ch: char| !ch.is_alphanumeric()) {
                if tok.len() > 1
                    && tok.starts_with('c')
                    && tok[1..].bytes().all(|b| b.is_ascii_digit())
                {
                    columns.push(ColumnDescription {
                        column: tok.into(),
                        description: "wide column".into(),
                    });
                }
            }
            Ok(SchemaDescriptions {
                tables: vec![TableDescriptions {
                    table: "orders".into(),
                    columns,
                }],
                tokens_in: Some(7),
                tokens_out: Some(3),
            })
        }
    }

    fn lookup(
        prompts: &Arc<Mutex<Vec<String>>>,
        fail: bool,
    ) -> impl Fn(AiProviderKind, Option<String>) -> Box<dyn AiProvider> {
        let prompts = prompts.clone();
        move |_, _| {
            Box::new(RecProvider {
                prompts: prompts.clone(),
                fail,
            })
        }
    }

    struct Env {
        ctx: ExecuteQueryContext,
        org: organization::Model,
        ds: data_source::Model,
        raw: serde_json::Value,
        /// Backend suites TRUNCATE data_sources; the advisory lock keeps the seeded row alive.
        _lock: sea_orm::DatabaseConnection,
    }

    async fn env() -> Option<Env> {
        let (Ok(db_url), Ok(redis_url)) = (
            std::env::var("TEST_DATABASE_URL"),
            std::env::var("TEST_REDIS_URL"),
        ) else {
            eprintln!("skipping: TEST_DATABASE_URL/TEST_REDIS_URL not set");
            return None;
        };
        let lock = acquire_shared_test_db_lock(&db_url).await;
        let db = arcodash_core::db::connect(&db_url).await.unwrap();
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();
        let ctx = test_ctx(db.clone(), redis.clone());
        let org = seed_org(&db, &format!("org-describe-{}", uuid::Uuid::new_v4())).await;
        let raw = serde_json::json!({
            "tables": [{ "name": "orders", "sampled_rows": 10, "columns": [{
                "name": "status", "null_frac": 0.0, "distinct_in_sample": 2,
                "samples": ["zebra_shipped_7781"]
            }]}],
            "relationships": [],
            "schema_refreshed_at_seen": "2026-01-01T00:00:00Z"
        });
        let schema = serde_json::json!({ "tables": [{
            "name": "orders",
            "columns": [{ "name": "status", "type_name": "text" }]
        }]});
        let ds = seed_data_source(&db, org.id).await;
        let mut a: data_source::ActiveModel = ds.into();
        a.ai_profile = Set(Some(raw.clone()));
        a.schema = Set(Some(schema));
        let ds = a.update(&db).await.unwrap();
        let mut conn = redis.clone();
        let _: () = redis::AsyncCommands::del(&mut conn, ai_profile_describing_key(ds.id))
            .await
            .unwrap();
        Some(Env {
            ctx,
            org,
            ds,
            raw,
            _lock: lock,
        })
    }

    impl Env {
        async fn describe(&self, chain: Vec<ResolvedProvider>, fail: bool) -> Vec<String> {
            let prompts = Arc::new(Mutex::new(Vec::new()));
            describe_if_needed(
                &self.ctx,
                &reqwest::Client::new(),
                lookup(&prompts, fail),
                &self.org,
                &self.ds,
                chain,
            )
            .await;
            let sent = prompts.lock().unwrap().clone();
            sent
        }

        async fn row(&self) -> data_source::Model {
            data_source::Entity::find_by_id(self.ds.id)
                .one(&self.ctx.db)
                .await
                .unwrap()
                .unwrap()
        }

        async fn loaded(&self) -> DataSourceAiProfile {
            let storage = self
                .ctx
                .storage
                .resolve(&self.ctx.db, self.org.id)
                .await
                .unwrap();
            load_ai_profile(&storage, &self.row().await)
                .await
                .unwrap()
                .unwrap()
        }

        async fn stored(&self) -> serde_json::Value {
            data_source::Entity::find_by_id(self.ds.id)
                .one(&self.ctx.db)
                .await
                .unwrap()
                .unwrap()
                .ai_profile
                .unwrap()
        }
    }

    fn one() -> Vec<ResolvedProvider> {
        vec![ResolvedProvider {
            kind: AiProviderKind::Gemini,
            api_key: "k".into(),
            model: Some("m".into()),
            base_url: None,
        }]
    }

    #[tokio::test]
    async fn describes_once_merges_and_meters_usage() {
        let Some(e) = env().await else { return };
        assert_eq!(e.describe(one(), false).await.len(), 1);

        let p: DataSourceAiProfile = serde_json::from_value(e.stored().await).unwrap();
        assert_eq!(
            p.tables[0].columns[0].description.as_deref(),
            Some("Fulfilment state of the order")
        );
        assert!(p.descriptions_used_samples);
        assert!(p.tables[0].columns[0].samples.is_empty());
        assert_eq!(
            e.loaded().await.tables[0].columns[0].samples,
            vec!["zebra_shipped_7781"]
        );

        let usage = ai_usage::Entity::find()
            .filter(ai_usage::Column::OrgId.eq(e.org.id))
            .filter(ai_usage::Column::Feature.eq("describe_schema"))
            .all(&e.ctx.db)
            .await
            .unwrap();
        assert_eq!(usage.len(), 1);

        // The guard is still held, so a second job is a no-op.
        assert!(e.describe(one(), false).await.is_empty());
    }

    #[tokio::test]
    async fn describe_keeps_values_out_of_postgres() {
        let Some(e) = env().await else { return };
        assert_eq!(e.describe(one(), false).await.len(), 1);

        let row = e.row().await;
        let raw = row.ai_profile.unwrap();
        assert!(!raw.to_string().contains("zebra_shipped_7781"));
        let loc = format!("{}.json", e.ds.id);
        assert_eq!(
            row.ai_profile_values_location.as_deref(),
            Some(loc.as_str())
        );
        let col = &e.loaded().await.tables[0].columns[0];
        assert_eq!(
            col.description.as_deref(),
            Some("Fulfilment state of the order")
        );
        assert_eq!(col.samples, vec!["zebra_shipped_7781"]);
    }

    #[tokio::test]
    async fn no_provider_makes_no_call() {
        let Some(e) = env().await else { return };
        assert!(e.describe(vec![], false).await.is_empty());
        assert_eq!(e.stored().await, e.raw);
    }

    #[tokio::test]
    async fn failed_run_backs_off_beyond_the_in_flight_guard() {
        let Some(e) = env().await else { return };
        assert_eq!(e.describe(one(), true).await.len(), 1);
        assert_eq!(e.stored().await, e.raw);
        let ttl = guard_ttl(&e).await;
        assert!(ttl > 600, "ttl {ttl}");
    }

    async fn guard_ttl(e: &Env) -> i64 {
        let mut conn = e.ctx.redis.clone();
        redis::cmd("TTL")
            .arg(ai_profile_describing_key(e.ds.id))
            .query_async(&mut conn)
            .await
            .unwrap()
    }

    async fn set_profile(e: &mut Env, raw: serde_json::Value) {
        let mut a: data_source::ActiveModel = e.ds.clone().into();
        a.ai_profile = Set(Some(raw.clone()));
        e.ds = a.update(&e.ctx.db).await.unwrap();
        e.raw = raw;
    }

    async fn clear_guard(e: &Env) {
        let mut conn = e.ctx.redis.clone();
        let _: () = redis::AsyncCommands::del(&mut conn, ai_profile_describing_key(e.ds.id))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn later_runs_prompt_only_the_remaining_columns() {
        use arcodash_core::ai::describe::MAX_COLUMNS_PER_TABLE;
        let Some(mut e) = env().await else { return };
        let cols: Vec<_> = (0..MAX_COLUMNS_PER_TABLE + 5)
            .map(|i| {
                serde_json::json!({
                    "name": format!("c{i}"), "null_frac": 0.0, "distinct_in_sample": 2, "samples": []
                })
            })
            .collect();
        let mut raw = e.raw.clone();
        raw["tables"][0]["columns"] = serde_json::Value::Array(cols);
        set_profile(&mut e, raw).await;

        let first = e.describe(one(), false).await;
        assert_eq!(first.len(), 1);
        clear_guard(&e).await;
        e.ds = data_source::Entity::find_by_id(e.ds.id)
            .one(&e.ctx.db)
            .await
            .unwrap()
            .unwrap();
        let second = e.describe(one(), false).await;
        assert_eq!(second.len(), 1);
        let last = MAX_COLUMNS_PER_TABLE + 4;
        assert!(second[0].contains(&format!("c{last}")), "{}", second[0]);
        assert!(!second[0].contains("c0,"), "{}", second[0]);
        let table_line = second[0]
            .lines()
            .rfind(|l| l.starts_with("orders("))
            .unwrap();
        assert_eq!(table_line.matches(", ").count(), 4, "{table_line}");

        let p: DataSourceAiProfile = serde_json::from_value(e.stored().await).unwrap();
        assert!(p.tables[0].columns.iter().all(|c| c.description.is_some()));
        clear_guard(&e).await;
    }

    #[tokio::test]
    async fn run_that_describes_nothing_backs_off() {
        let Some(mut e) = env().await else { return };
        let mut raw = e.raw.clone();
        raw["tables"][0]["columns"] = serde_json::json!([{
            "name": "total", "null_frac": 0.0, "distinct_in_sample": 2, "samples": []
        }]);
        set_profile(&mut e, raw).await;
        assert_eq!(e.describe(one(), false).await.len(), 1);
        let p: DataSourceAiProfile = serde_json::from_value(e.stored().await).unwrap();
        assert!(p.tables[0].columns[0].description.is_none());
        let ttl = guard_ttl(&e).await;
        assert!(ttl > 600, "ttl {ttl}");
        clear_guard(&e).await;
    }

    #[tokio::test]
    async fn full_merge_keeps_the_short_guard() {
        let Some(e) = env().await else { return };
        assert_eq!(e.describe(one(), false).await.len(), 1);
        let ttl = guard_ttl(&e).await;
        assert!((1..=600).contains(&ttl), "ttl {ttl}");
    }

    #[tokio::test]
    async fn exhausted_budget_makes_no_call_and_backs_off() {
        let Some(e) = env().await else { return };
        ConnectionTrait::execute_unprepared(
            &e.ctx.db,
            &format!(
                "UPDATE organizations SET settings = jsonb_set(settings, '{{ai}}', \
                 '{{\"monthly_budget_usd\": 1.0}}'::jsonb) WHERE id = {}",
                e.org.id
            ),
        )
        .await
        .unwrap();
        ai_usage::ActiveModel {
            org_id: Set(e.org.id),
            provider: Set("gemini".into()),
            model: Set("m".into()),
            feature: Set("ask".into()),
            status: Set("ok".into()),
            cost_usd: Set(Some(2.0)),
            ..Default::default()
        }
        .insert(&e.ctx.db)
        .await
        .unwrap();

        let usage_rows = || async {
            ai_usage::Entity::find()
                .filter(ai_usage::Column::OrgId.eq(e.org.id))
                .all(&e.ctx.db)
                .await
                .unwrap()
                .len()
        };
        let before = usage_rows().await;
        assert!(e.describe(one(), false).await.is_empty());
        assert_eq!(usage_rows().await, before);
        assert_eq!(e.stored().await, e.raw);
        let ttl = guard_ttl(&e).await;
        assert!(ttl > 600, "ttl {ttl}");
        clear_guard(&e).await;
    }

    #[tokio::test]
    async fn profile_without_columns_makes_no_call() {
        let Some(mut e) = env().await else { return };
        let mut raw = e.raw.clone();
        raw["tables"][0]["columns"] = serde_json::json!([]);
        set_profile(&mut e, raw).await;
        assert!(e.describe(one(), false).await.is_empty());
    }

    #[tokio::test]
    async fn prompt_listing_nothing_makes_no_call_and_backs_off() {
        let Some(mut e) = env().await else { return };
        let mut raw = e.raw.clone();
        raw["tables"][0]["columns"][0]["name"] =
            serde_json::Value::String("n".repeat(arcodash_core::ai::describe::MAX_PROMPT_CHARS));
        set_profile(&mut e, raw).await;
        assert!(e.describe(one(), false).await.is_empty());
        let ttl = guard_ttl(&e).await;
        assert!(ttl > 600, "ttl {ttl}");
        clear_guard(&e).await;
    }

    #[tokio::test]
    async fn samples_off_keeps_values_out_of_the_prompt() {
        let Some(mut e) = env().await else { return };
        let mut settings = e.org.settings.clone();
        settings["ai"] = serde_json::json!({ "include_sample_values": false });
        e.org.settings = settings;
        let sent = e.describe(one(), false).await;
        assert_eq!(sent.len(), 1);
        assert!(!sent[0].contains("zebra_shipped_7781"), "{}", sent[0]);
        let p: DataSourceAiProfile = serde_json::from_value(e.stored().await).unwrap();
        assert!(!p.descriptions_used_samples);
        assert!(p.tables[0].columns[0].description.is_some());
    }

    #[tokio::test]
    async fn cross_org_message_is_refused() {
        let Some(e) = env().await else { return };
        let err = load(&e.ctx, e.org.id + 1_000_000, e.ds.id).await;
        assert!(err.is_err());
    }

    /// Real provider through the worker path; keys come from the environment.
    #[tokio::test]
    #[ignore = "requires a real provider key (GEMINI_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY)"]
    async fn live_provider_describes_through_the_worker_path() {
        let Some(e) = env().await else {
            panic!("test stack required")
        };
        let var = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
        // Several env keys with no stored order resolve to an empty chain, so use the first one.
        let g = var("GEMINI_API_KEY");
        let o = var("OPENAI_API_KEY").filter(|_| g.is_none());
        let a = var("ANTHROPIC_API_KEY").filter(|_| g.is_none() && o.is_none());
        let model = |n: &str, d: &str| std::env::var(n).unwrap_or_else(|_| d.to_string());
        let chain = arcodash_core::ai::settings::resolve_provider_chain_with(
            &serde_json::json!({}),
            g.as_deref(),
            o.as_deref(),
            a.as_deref(),
            &model("GEMINI_MODEL", "gemini-3.8-flash"),
            &model("OPENAI_MODEL", "gpt-5.6-terra"),
            &model("ANTHROPIC_MODEL", "claude-sonnet-5-5"),
            &"00".repeat(32),
        )
        .unwrap();
        assert!(!chain.is_empty(), "no provider key in the environment");
        let kind = chain[0].kind;
        describe_if_needed(
            &e.ctx,
            &reqwest::Client::new(),
            arcodash_core::ai::provider_for,
            &e.org,
            &e.ds,
            chain,
        )
        .await;
        let p: DataSourceAiProfile = serde_json::from_value(e.stored().await).unwrap();
        let d = p.tables[0].columns[0].description.clone();
        println!("{} -> status: {d:?}", kind.as_str());
        assert!(d.is_some_and(|d| !d.trim().is_empty()));
        let usage = ai_usage::Entity::find()
            .filter(ai_usage::Column::OrgId.eq(e.org.id))
            .filter(ai_usage::Column::Status.eq("ok"))
            .all(&e.ctx.db)
            .await
            .unwrap();
        assert_eq!(usage.len(), 1);
    }
}

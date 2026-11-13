//! Admin action: nulls customer data still held inline in Postgres once a verified copy exists in
//! object storage. Every row is re-verified immediately before its inline columns are cleared;
//! rows without a verified copy are left untouched and rows are never deleted.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use anyhow::Context;
use chrono::{DateTime, Utc};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, Statement, TransactionTrait,
};

use arcodash_core::ai::narrative::load_narrative_content;
use arcodash_core::ai::profile_values::{split_profile, ProfileValues};
use arcodash_core::entities::dashboard_narrative;
use arcodash_core::models::DataSourceAiProfile;
use arcodash_core::observation_store::load_observations;
use arcodash_core::org_storage::{OrgStorage, OrgStorageResolver, StorageArea};
use arcodash_core::result_store::decode_result;

use crate::storage_backfill::line_matches;

/// Rows examined per database round trip when the caller does not choose.
pub const DEFAULT_BATCH: i64 = 100;

/// Per-area outcome.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct AreaReport {
    /// Rows whose stored copy matches the inline data right now.
    pub verified: usize,
    /// Rows whose inline columns were cleared (always 0 in a dry run).
    pub cleared: usize,
    /// Rows holding a copy that is missing, unreadable or different from the inline data.
    pub not_verified: usize,
    /// Rows holding inline data and no copy yet.
    pub no_copy: usize,
    /// Rows removed or emptied while the run was examining them.
    pub vanished: usize,
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct ClearReport {
    pub results: AreaReport,
    pub observations: AreaReport,
    pub narratives: AreaReport,
    pub mail: AreaReport,
    pub ai_profiles: AreaReport,
}

impl fmt::Display for ClearReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (name, a) in [
            ("results", &self.results),
            ("observations", &self.observations),
            ("narratives", &self.narratives),
            ("mail", &self.mail),
            ("ai_profiles", &self.ai_profiles),
        ] {
            writeln!(
                f,
                "{name}: verified={} cleared={} not_verified={} no_copy={} vanished={}",
                a.verified, a.cleared, a.not_verified, a.no_copy, a.vanished
            )?;
        }
        Ok(())
    }
}

/// Per-run cache of each org's storage; `None` marks an org whose storage cannot be read.
struct Orgs<'a> {
    db: &'a DatabaseConnection,
    resolver: &'a dyn OrgStorageResolver,
    map: HashMap<i64, Option<OrgStorage>>,
}

impl<'a> Orgs<'a> {
    fn new(db: &'a DatabaseConnection, resolver: &'a dyn OrgStorageResolver) -> Self {
        Self {
            db,
            resolver,
            map: HashMap::new(),
        }
    }

    async fn get(&mut self, org_id: i64) -> Option<&OrgStorage> {
        if !self.map.contains_key(&org_id) {
            let resolved = match self.resolver.resolve(self.db, org_id).await {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(org_id, error = %e, "clear inline data: org storage unavailable");
                    None
                }
            };
            self.map.insert(org_id, resolved);
        }
        self.map.get(&org_id).and_then(Option::as_ref)
    }
}

/// Counts (and, with `confirm`, clears) every inline copy that is verified against object storage.
/// A dry run changes nothing.
///
/// # Errors
///
/// Database failure while selecting or updating rows.
pub async fn clear_inline_data(
    db: &DatabaseConnection,
    resolver: &dyn OrgStorageResolver,
    confirm: bool,
    batch: i64,
) -> anyhow::Result<ClearReport> {
    clear_inline_data_scoped(db, resolver, confirm, batch, None).await
}

/// [`clear_inline_data`] restricted to one organization when `org_id` is set.
pub(crate) async fn clear_inline_data_scoped(
    db: &DatabaseConnection,
    resolver: &dyn OrgStorageResolver,
    confirm: bool,
    batch: i64,
    org_id: Option<i64>,
) -> anyhow::Result<ClearReport> {
    let batch = batch.max(1);
    let mut orgs = Orgs::new(db, resolver);
    let run = Run {
        db,
        confirm,
        batch,
        org_id,
    };
    Ok(ClearReport {
        results: run.results(&mut orgs).await?,
        observations: run.observations(&mut orgs).await?,
        narratives: run.narratives(&mut orgs).await?,
        mail: run.mail(&mut orgs).await?,
        ai_profiles: run.ai_profiles(&mut orgs).await?,
    })
}

struct Run<'a> {
    db: &'a DatabaseConnection,
    confirm: bool,
    batch: i64,
    org_id: Option<i64>,
}

fn apply(report: &mut AreaReport, verified: bool, cleared: bool) {
    if verified {
        report.verified += 1;
        if cleared {
            report.cleared += 1;
        }
    } else {
        report.not_verified += 1;
    }
}

impl Run<'_> {
    async fn results(&self, orgs: &mut Orgs<'_>) -> anyhow::Result<AreaReport> {
        let mut rep = AreaReport::default();
        let mut cursor = 0i64;
        loop {
            let rows = self
                .db
                .query_all_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT id, org_id, data_source_id, query_hash, data_location \
                     FROM query_results \
                     WHERE id > $1 AND data IS NOT NULL \
                       AND ($3::bigint IS NULL OR org_id = $3) \
                     ORDER BY id LIMIT $2",
                    [cursor.into(), self.batch.into(), self.org_id.into()],
                ))
                .await
                .context("select inline results")?;
            let Some(last) = rows.last() else {
                return Ok(rep);
            };
            cursor = last.try_get("", "id")?;
            for r in &rows {
                let id: i64 = r.try_get("", "id")?;
                let org_id: i64 = r.try_get("", "org_id")?;
                let ds: i64 = r.try_get("", "data_source_id")?;
                let hash: String = r.try_get("", "query_hash")?;
                let location: Option<String> = r.try_get("", "data_location")?;
                if location.is_none() {
                    rep.no_copy += 1;
                    continue;
                }
                let Some(storage) = orgs.get(org_id).await else {
                    rep.not_verified += 1;
                    continue;
                };
                let (verified, cleared) = self
                    .clear_result(storage, id, org_id, ds, &hash)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(query_result_id = id, error = %e, "clear inline data: result check failed");
                        (false, false)
                    });
                apply(&mut rep, verified, cleared);
            }
        }
    }

    /// Verifies one result under the writers' key lock and, when confirming, nulls its data.
    async fn clear_result(
        &self,
        storage: &OrgStorage,
        id: i64,
        org_id: i64,
        ds: i64,
        hash: &str,
    ) -> anyhow::Result<(bool, bool)> {
        let txn = self.db.begin().await?;
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT pg_advisory_xact_lock(hashtext($1))",
            [format!("{org_id}:{ds}:{hash}").into()],
        ))
        .await?;
        let row = txn
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT data, data_location FROM query_results \
                 WHERE id = $1 AND data IS NOT NULL AND data_location IS NOT NULL",
                [id.into()],
            ))
            .await?;
        let Some(row) = row else {
            return Ok((false, false));
        };
        let inline: serde_json::Value = row.try_get("", "data")?;
        let location: String = row.try_get("", "data_location")?;
        let stored = storage
            .get(StorageArea::Results, &location)
            .await
            .map_or(None, |bytes| decode_result(&bytes).ok());
        if stored.as_ref() != Some(&inline) {
            return Ok((false, false));
        }
        if !self.confirm {
            return Ok((true, false));
        }
        let updated = txn
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE query_results SET data = NULL \
                 WHERE id = $1 AND data_location = $2 AND data IS NOT NULL",
                [id.into(), location.into()],
            ))
            .await?;
        txn.commit().await?;
        Ok((true, updated.rows_affected() == 1))
    }

    async fn observations(&self, orgs: &mut Orgs<'_>) -> anyhow::Result<AreaReport> {
        struct Obs {
            id: i64,
            observed_at: DateTime<Utc>,
            value: Option<f64>,
            explanation: Option<String>,
        }
        let mut rep = AreaReport::default();
        let mut cursor = 0i64;
        loop {
            let rows = self
                .db
                .query_all_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT id, org_id, monitor_id, observed_at, value, explanation \
                     FROM monitor_observations \
                     WHERE id > $1 AND (value IS NOT NULL OR explanation IS NOT NULL) \
                       AND ($3::bigint IS NULL OR org_id = $3) \
                     ORDER BY id LIMIT $2",
                    [cursor.into(), self.batch.into(), self.org_id.into()],
                ))
                .await
                .context("select inline observations")?;
            let Some(last) = rows.last() else {
                return Ok(rep);
            };
            cursor = last.try_get("", "id")?;
            let mut groups: BTreeMap<(i64, i64), Vec<Obs>> = BTreeMap::new();
            for r in &rows {
                groups
                    .entry((r.try_get("", "org_id")?, r.try_get("", "monitor_id")?))
                    .or_default()
                    .push(Obs {
                        id: r.try_get("", "id")?,
                        observed_at: r.try_get("", "observed_at")?,
                        value: r.try_get("", "value")?,
                        explanation: r.try_get("", "explanation")?,
                    });
            }
            for ((org_id, monitor_id), group) in groups {
                let (Some(from), Some(to)) = (
                    group.iter().map(|o| o.observed_at).min(),
                    group.iter().map(|o| o.observed_at).max(),
                ) else {
                    continue;
                };
                let stored = match orgs.get(org_id).await {
                    Some(storage) => load_observations(storage, monitor_id, from, to).await.ok(),
                    None => None,
                };
                for o in &group {
                    let matches = stored.as_ref().is_some_and(|s| {
                        line_matches(s.get(&o.id), o.value, o.explanation.as_deref())
                    });
                    if !matches {
                        rep.not_verified += 1;
                        continue;
                    }
                    let cleared = self.confirm && {
                        let n = self
                            .db
                            .execute_raw(Statement::from_sql_and_values(
                                DbBackend::Postgres,
                                "UPDATE monitor_observations SET value = NULL, explanation = NULL \
                                 WHERE id = $1 AND value IS NOT DISTINCT FROM $2 \
                                   AND explanation IS NOT DISTINCT FROM $3",
                                [o.id.into(), o.value.into(), o.explanation.clone().into()],
                            ))
                            .await?;
                        n.rows_affected() == 1
                    };
                    apply(&mut rep, true, cleared);
                }
            }
        }
    }

    async fn narratives(&self, orgs: &mut Orgs<'_>) -> anyhow::Result<AreaReport> {
        let mut rep = AreaReport::default();
        let mut cursor = 0i64;
        loop {
            let mut q = dashboard_narrative::Entity::find()
                .filter(dashboard_narrative::Column::Id.gt(cursor))
                .filter(dashboard_narrative::Column::Summary.is_not_null());
            if let Some(org) = self.org_id {
                q = q.filter(dashboard_narrative::Column::OrgId.eq(org));
            }
            let rows = q
                .order_by_asc(dashboard_narrative::Column::Id)
                .limit(u64::try_from(self.batch).unwrap_or(1))
                .all(self.db)
                .await
                .context("select inline narratives")?;
            let Some(last) = rows.last() else {
                return Ok(rep);
            };
            cursor = last.id;
            for n in &rows {
                if n.data_location.is_none() {
                    rep.no_copy += 1;
                    continue;
                }
                let Some(storage) = orgs.get(n.org_id).await else {
                    rep.not_verified += 1;
                    continue;
                };
                let (verified, cleared) =
                    self.clear_narrative(storage, n.id).await.unwrap_or_else(|e| {
                        tracing::warn!(narrative_id = n.id, error = %e, "clear inline data: narrative check failed");
                        (false, false)
                    });
                apply(&mut rep, verified, cleared);
            }
        }
    }

    /// Re-reads the row, compares its inline content with the stored object and nulls it.
    async fn clear_narrative(&self, storage: &OrgStorage, id: i64) -> anyhow::Result<(bool, bool)> {
        let Some(n) = dashboard_narrative::Entity::find_by_id(id)
            .one(self.db)
            .await?
        else {
            return Ok((false, false));
        };
        let (Some(summary), Some(_)) = (n.summary.clone(), n.data_location.as_ref()) else {
            return Ok((false, false));
        };
        let inline = load_narrative_content(storage, &n).await?;
        let mut stored_row = n;
        stored_row.summary = None;
        let stored = load_narrative_content(storage, &stored_row).await;
        if inline.is_none() || !matches!(&stored, Ok(s) if *s == inline) {
            return Ok((false, false));
        }
        if !self.confirm {
            return Ok((true, false));
        }
        let updated = self
            .db
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE dashboard_narratives \
                 SET summary = NULL, highlights = NULL, body = NULL, digest = NULL \
                 WHERE id = $1 AND data_location IS NOT NULL AND summary = $2",
                [id.into(), summary.into()],
            ))
            .await?;
        Ok((true, updated.rows_affected() == 1))
    }

    async fn mail(&self, orgs: &mut Orgs<'_>) -> anyhow::Result<AreaReport> {
        let mut rep = AreaReport::default();
        let mut cursor = 0i64;
        loop {
            let rows = self
                .db
                .query_all_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT id, org_id, body_location FROM mail_deliveries \
                     WHERE id > $1 AND body_html IS NOT NULL \
                       AND ($3::bigint IS NULL OR org_id = $3) \
                     ORDER BY id LIMIT $2",
                    [cursor.into(), self.batch.into(), self.org_id.into()],
                ))
                .await
                .context("select inline mail bodies")?;
            let Some(last) = rows.last() else {
                return Ok(rep);
            };
            cursor = last.try_get("", "id")?;
            for r in &rows {
                let id: i64 = r.try_get("", "id")?;
                let org_id: i64 = r.try_get("", "org_id")?;
                let location: Option<String> = r.try_get("", "body_location")?;
                if location.is_none() {
                    rep.no_copy += 1;
                    continue;
                }
                let Some(storage) = orgs.get(org_id).await else {
                    rep.not_verified += 1;
                    continue;
                };
                let (verified, cleared) = self.clear_mail(storage, id).await.unwrap_or_else(|e| {
                    tracing::warn!(delivery_id = id, error = %e, "clear inline data: mail check failed");
                    (false, false)
                });
                apply(&mut rep, verified, cleared);
            }
        }
    }

    async fn clear_mail(&self, storage: &OrgStorage, id: i64) -> anyhow::Result<(bool, bool)> {
        let row = self
            .db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT body_html, body_location FROM mail_deliveries \
                 WHERE id = $1 AND body_html IS NOT NULL AND body_location IS NOT NULL",
                [id.into()],
            ))
            .await?;
        let Some(row) = row else {
            return Ok((false, false));
        };
        let inline: String = row.try_get("", "body_html")?;
        let location: String = row.try_get("", "body_location")?;
        let matches = storage
            .get(StorageArea::Emails, &location)
            .await
            .is_ok_and(|b| b.as_ref() == inline.as_bytes());
        if !matches {
            return Ok((false, false));
        }
        if !self.confirm {
            return Ok((true, false));
        }
        let updated = self
            .db
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE mail_deliveries SET body_html = NULL \
                 WHERE id = $1 AND body_location = $2 AND body_html = $3",
                [id.into(), location.into(), inline.into()],
            ))
            .await?;
        Ok((true, updated.rows_affected() == 1))
    }

    async fn ai_profiles(&self, orgs: &mut Orgs<'_>) -> anyhow::Result<AreaReport> {
        let mut rep = AreaReport::default();
        let mut cursor = 0i64;
        loop {
            let rows = self
                .db
                .query_all_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT id, org_id FROM data_sources \
                     WHERE id > $1 AND ai_profile IS NOT NULL \
                       AND ($3::bigint IS NULL OR org_id = $3) \
                     ORDER BY id LIMIT $2",
                    [cursor.into(), self.batch.into(), self.org_id.into()],
                ))
                .await
                .context("select AI profiles")?;
            let Some(last) = rows.last() else {
                return Ok(rep);
            };
            cursor = last.try_get("", "id")?;
            for r in &rows {
                let id: i64 = r.try_get("", "id")?;
                let org_id: i64 = r.try_get("", "org_id")?;
                let Some(storage) = orgs.get(org_id).await else {
                    rep.not_verified += 1;
                    continue;
                };
                match self.clear_ai_profile(storage, id).await {
                    Ok(Profile::NothingInline) => {}
                    Ok(Profile::Vanished) => rep.vanished += 1,
                    Ok(Profile::NoCopy) => rep.no_copy += 1,
                    Ok(Profile::Checked { verified, cleared }) => {
                        apply(&mut rep, verified, cleared);
                    }
                    Err(e) => {
                        tracing::warn!(data_source_id = id, error = %e, "clear inline data: AI profile check failed");
                        rep.not_verified += 1;
                    }
                }
            }
        }
    }

    async fn clear_ai_profile(&self, storage: &OrgStorage, id: i64) -> anyhow::Result<Profile> {
        let txn = self.db.begin().await?;
        let lock = if self.confirm { " FOR UPDATE" } else { "" };
        let row = txn
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                format!(
                    "SELECT ai_profile, ai_profile_values_location FROM data_sources \
                     WHERE id = $1 AND ai_profile IS NOT NULL{lock}"
                ),
                [id.into()],
            ))
            .await?;
        let Some(row) = row else {
            return Ok(Profile::Vanished);
        };
        let raw: serde_json::Value = row.try_get("", "ai_profile")?;
        let location: Option<String> = row.try_get("", "ai_profile_values_location")?;
        let Ok(profile) = serde_json::from_value::<DataSourceAiProfile>(raw.clone()) else {
            return Ok(Profile::NothingInline);
        };
        let (stripped, values) = split_profile(&profile);
        if values.0.is_empty() {
            return Ok(Profile::NothingInline);
        }
        let Some(location) = location else {
            return Ok(Profile::NoCopy);
        };
        let stored = storage
            .get(StorageArea::AiProfiles, &location)
            .await
            .map_or(None, |bytes| {
                serde_json::from_slice::<ProfileValues>(&bytes).ok()
            });
        if stored.as_ref() != Some(&values) {
            return Ok(Profile::Checked {
                verified: false,
                cleared: false,
            });
        }
        if !self.confirm {
            return Ok(Profile::Checked {
                verified: true,
                cleared: false,
            });
        }
        let updated = txn
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE data_sources SET ai_profile = $2 \
                 WHERE id = $1 AND ai_profile_values_location = $3 AND ai_profile = $4",
                [
                    id.into(),
                    serde_json::to_value(&stripped)?.into(),
                    location.into(),
                    raw.into(),
                ],
            ))
            .await?;
        txn.commit().await?;
        Ok(Profile::Checked {
            verified: true,
            cleared: updated.rows_affected() == 1,
        })
    }
}

enum Profile {
    /// The profile holds no sampled values inline.
    NothingInline,
    /// Inline values and no stored copy.
    NoCopy,
    /// The row was removed or its profile emptied since the batch was read.
    Vanished,
    Checked {
        verified: bool,
        cleared: bool,
    },
}

/// Parsed `clear-inline-data` arguments.
#[derive(Debug, PartialEq, Eq)]
pub struct ClearArgs {
    pub confirm: bool,
    pub batch: i64,
}

/// Parses `[--confirm] [--batch N]`.
///
/// # Errors
///
/// An unknown flag or a missing or non-positive batch size.
pub fn parse_clear_args(args: &[String]) -> anyhow::Result<ClearArgs> {
    let mut out = ClearArgs {
        confirm: false,
        batch: DEFAULT_BATCH,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--confirm" => out.confirm = true,
            "--batch" => {
                out.batch = it
                    .next()
                    .and_then(|v| v.parse::<i64>().ok())
                    .filter(|n| *n > 0)
                    .context("--batch needs a positive number")?;
            }
            other => anyhow::bail!(
                "unknown argument `{other}`; usage: clear-inline-data [--confirm] [--batch N]"
            ),
        }
    }
    Ok(out)
}

/// Runs the `clear-inline-data` command against the configured database and storage, printing the
/// report. Without `--confirm` nothing is changed.
///
/// # Errors
///
/// Invalid arguments, missing configuration, or database failure.
pub async fn run_command(
    resolver: Option<std::sync::Arc<dyn OrgStorageResolver>>,
    args: &[String],
) -> anyhow::Result<()> {
    let args = parse_clear_args(args)?;
    let _ = dotenvy::dotenv();
    let config = arcodash_core::config::Config::from_env()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(&config.rust_log)
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let db = arcodash_core::db::connect(&config.database_url).await?;
    crate::assert_table_rls_bypass_role(&db, "organizations").await?;
    let resolver = match resolver {
        Some(r) => r,
        None => std::sync::Arc::new(arcodash_core::org_storage::DefaultStorageResolver::new(
            arcodash_core::storage::ObjectStore::from_env()
                .context("failed to initialize object storage")?,
        )),
    };
    let report = clear_inline_data(&db, resolver.as_ref(), args.confirm, args.batch).await?;
    if args.confirm {
        println!("Cleared inline data:");
    } else {
        println!("Dry run, nothing changed. Re-run with --confirm to clear verified rows:");
    }
    print!("{report}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arcodash_core::ai::narrative::NarrativeContent;
    use arcodash_core::ai::profile_values::load_ai_profile;
    use arcodash_core::entities::{data_source, mail_delivery, query_result};
    use arcodash_core::mail_notify::load_mail_body;
    use arcodash_core::result_store::load_result;
    use arcodash_core::test_support::blob::MemoryBlobStore;
    use serde_json::json;

    use super::*;
    use crate::scheduler::test_support::*;
    use crate::storage_backfill::backfill_batch;
    use crate::storage_backfill::tests::{
        fixture, json_of, seed_mail, seed_narrative, seed_observation, seed_profile, Fixture,
        PROFILE_JSON,
    };

    struct Seeded {
        result: i64,
        monitor: i64,
        observation: i64,
        narrative: i64,
        mail: i64,
    }

    /// One row per area, backfilled.
    async fn seed_backfilled(f: &Fixture) -> Seeded {
        let result = seed_query_result(&f.ctx.db, f.org_id, f.ds_id, Utc::now(), json!({"a": 1}))
            .await
            .id;
        set_latest_query_data(&f.ctx.db, f.query_id, result).await;
        let monitor = seed_monitor(&f.ctx.db, f.org_id, f.query_id, f.user_id, json!({}))
            .await
            .id;
        let observation = seed_observation(f, monitor, Some(3.5), Some("why")).await;
        let narrative = seed_narrative(f, "sum").await;
        let mail = seed_mail(f, "<p>hi</p>", "pending", 0).await;
        seed_profile(f, f.ds_id).await;
        backfill_batch(&f.ctx, 10_000).await.unwrap();
        Seeded {
            result,
            monitor,
            observation,
            narrative,
            mail,
        }
    }

    async fn run(f: &Fixture, confirm: bool) -> ClearReport {
        clear_inline_data_scoped(
            &f.ctx.db,
            f.ctx.storage.as_ref(),
            confirm,
            2,
            Some(f.org_id),
        )
        .await
        .unwrap()
    }

    /// Every row of the fixture org in the five tables, as JSON.
    async fn snapshot(f: &Fixture) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for t in [
            "query_results",
            "monitor_observations",
            "dashboard_narratives",
            "mail_deliveries",
            "data_sources",
        ] {
            let r = query_row(
                &f.ctx.db,
                &format!(
                    "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) AS rows \
                     FROM {t} t WHERE org_id = $1"
                ),
                [f.org_id.into()],
            )
            .await;
            out.push(r.try_get("", "rows").unwrap());
        }
        out
    }

    async fn is_null(f: &Fixture, table: &str, col: &str, id: i64) -> bool {
        query_row(
            &f.ctx.db,
            &format!("SELECT ({col} IS NULL) AS n FROM {table} WHERE id = $1"),
            [id.into()],
        )
        .await
        .try_get("", "n")
        .unwrap()
    }

    #[tokio::test]
    async fn clear_inline_dry_run_changes_nothing() {
        let Some(f) = fixture(Arc::new(MemoryBlobStore::default()), false).await else {
            return;
        };
        seed_backfilled(&f).await;
        let orphan = seed_query_result(&f.ctx.db, f.org_id, f.ds_id, Utc::now(), json!({"o": 1}))
            .await
            .id;
        let before = snapshot(&f).await;

        let report = run(&f, false).await;

        let one = AreaReport {
            verified: 1,
            ..AreaReport::default()
        };
        assert_eq!(report.results.verified, 1);
        assert_eq!(report.results.no_copy, 1, "result {orphan}");
        assert_eq!(report.observations, one);
        assert_eq!(report.narratives, one);
        assert_eq!(report.mail, one);
        assert_eq!(report.ai_profiles, one);
        assert_eq!(report.results.cleared, 0);
        assert_eq!(snapshot(&f).await, before);
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "one scenario seeding verified, damaged and copy-less rows in every area"
    )]
    async fn clear_inline_only_touches_verified_rows() {
        let store = Arc::new(MemoryBlobStore::default());
        let Some(f) = fixture(store, false).await else {
            return;
        };
        let ok = seed_backfilled(&f).await;
        let storage = f.ctx.storage.resolve(&f.ctx.db, f.org_id).await.unwrap();

        // Second row per area whose copy is damaged after the backfill.
        let q2 = seed_query(&f.ctx.db, f.org_id, f.ds_id).await;
        let lost_result =
            seed_query_result(&f.ctx.db, f.org_id, f.ds_id, Utc::now(), json!({"b": 2})).await;
        set_latest_query_data(&f.ctx.db, q2.id, lost_result.id).await;
        let tampered = seed_narrative(&f, "tampered").await;
        let lost_mail = seed_mail(&f, "<p>lost</p>", "pending", 0).await;
        let ds2 = seed_data_source(&f.ctx.db, f.org_id).await;
        seed_profile(&f, ds2.id).await;
        backfill_batch(&f.ctx, 10_000).await.unwrap();

        let loc: String = query_row(
            &f.ctx.db,
            "SELECT data_location FROM query_results WHERE id = $1",
            [lost_result.id.into()],
        )
        .await
        .try_get("", "data_location")
        .unwrap();
        storage.delete(StorageArea::Results, &loc).await.unwrap();
        let content = NarrativeContent {
            summary: "other".into(),
            highlights: json!([]),
            body: None,
            digest: json!({}),
        };
        let n = dashboard_narrative::Entity::find_by_id(tampered)
            .one(&f.ctx.db)
            .await
            .unwrap()
            .unwrap();
        storage
            .put(
                StorageArea::Narratives,
                n.data_location.as_deref().unwrap(),
                serde_json::to_vec(&content).unwrap().into(),
                "application/json",
            )
            .await
            .unwrap();
        storage
            .delete(StorageArea::Emails, &format!("{lost_mail}.html"))
            .await
            .unwrap();
        storage
            .delete(StorageArea::AiProfiles, &format!("{}.json", ds2.id))
            .await
            .unwrap();

        // Rows added after the backfill have no copy at all.
        let pointerless =
            seed_query_result(&f.ctx.db, f.org_id, f.ds_id, Utc::now(), json!({"c": 3}))
                .await
                .id;
        let new_narrative = seed_narrative(&f, "new").await;
        let new_mail = seed_mail(&f, "<p>new</p>", "pending", 0).await;
        let ds3 = seed_data_source(&f.ctx.db, f.org_id).await;
        seed_profile(&f, ds3.id).await;
        let new_obs = seed_observation(&f, ok.monitor, Some(9.0), None).await;

        let report = run(&f, true).await;

        let area = |cleared, not_verified, no_copy| AreaReport {
            verified: cleared,
            cleared,
            not_verified,
            no_copy,
            vanished: 0,
        };
        assert_eq!(report.results, area(1, 1, 1));
        assert_eq!(report.observations, area(1, 1, 0));
        assert_eq!(report.narratives, area(1, 1, 1));
        assert_eq!(report.mail, area(1, 1, 1));
        assert_eq!(report.ai_profiles, area(1, 1, 1));

        assert!(is_null(&f, "query_results", "data", ok.result).await);
        assert!(is_null(&f, "monitor_observations", "value", ok.observation).await);
        assert!(is_null(&f, "monitor_observations", "explanation", ok.observation).await);
        for col in ["summary", "highlights", "body", "digest"] {
            assert!(
                is_null(&f, "dashboard_narratives", col, ok.narrative).await,
                "{col}"
            );
        }
        assert!(is_null(&f, "mail_deliveries", "body_html", ok.mail).await);
        let r = query_row(
            &f.ctx.db,
            "SELECT ai_profile FROM data_sources WHERE id = $1",
            [f.ds_id.into()],
        )
        .await;
        let stripped: serde_json::Value = r.try_get("", "ai_profile").unwrap();
        assert!(stripped["tables"][0]["columns"][0]["samples"]
            .as_array()
            .is_none_or(Vec::is_empty));
        assert!(stripped["tables"][0]["columns"][0]["min"].is_null());

        for id in [lost_result.id, pointerless] {
            assert!(
                !is_null(&f, "query_results", "data", id).await,
                "result {id}"
            );
        }
        assert!(!is_null(&f, "monitor_observations", "value", new_obs).await);
        for id in [tampered, new_narrative] {
            assert!(!is_null(&f, "dashboard_narratives", "summary", id).await);
        }
        for id in [lost_mail, new_mail] {
            assert!(!is_null(&f, "mail_deliveries", "body_html", id).await);
        }
        for id in [ds2.id, ds3.id] {
            let r = query_row(
                &f.ctx.db,
                "SELECT ai_profile FROM data_sources WHERE id = $1",
                [id.into()],
            )
            .await;
            assert_eq!(
                r.try_get::<serde_json::Value>("", "ai_profile").unwrap(),
                json_of(PROFILE_JSON)
            );
        }
    }

    #[tokio::test]
    async fn clear_inline_rows_still_read_from_storage() {
        let Some(f) = fixture(Arc::new(MemoryBlobStore::default()), false).await else {
            return;
        };
        let s = seed_backfilled(&f).await;
        let db = &f.ctx.db;
        let storage = f.ctx.storage.resolve(db, f.org_id).await.unwrap();
        let day = chrono::Duration::days(1);
        let (from, to) = (Utc::now() - day, Utc::now() + day);

        let read = || async {
            let qr = query_result::Entity::find_by_id(s.result)
                .one(db)
                .await
                .unwrap()
                .unwrap();
            let result = load_result(db, db, f.ctx.storage.as_ref(), &qr)
                .await
                .unwrap();
            let n = dashboard_narrative::Entity::find_by_id(s.narrative)
                .one(db)
                .await
                .unwrap()
                .unwrap();
            let narrative = load_narrative_content(&storage, &n).await.unwrap();
            let m = mail_delivery::Entity::find_by_id(s.mail)
                .one(db)
                .await
                .unwrap()
                .unwrap();
            let mail = load_mail_body(&storage, &m).await.unwrap();
            let ds = data_source::Entity::find_by_id(f.ds_id)
                .one(db)
                .await
                .unwrap()
                .unwrap();
            let profile = load_ai_profile(&storage, &ds).await.unwrap();
            let obs = load_observations(&storage, s.monitor, from, to)
                .await
                .unwrap();
            (
                result,
                narrative,
                mail,
                profile,
                obs[&s.observation].clone(),
            )
        };
        let before = read().await;
        assert_eq!(before.0, Some(json!({"rows": [{"a": 1}]})));
        assert_eq!(before.2, "<p>hi</p>");
        assert_eq!(before.4.value, Some(3.5));

        let report = run(&f, true).await;
        assert_eq!(report.results.cleared, 1);
        assert_eq!(report.ai_profiles.cleared, 1);
        assert!(is_null(&f, "query_results", "data", s.result).await);

        let after = read().await;
        assert_eq!(after.0, before.0);
        assert_eq!(after.1, before.1);
        assert_eq!(after.2, before.2);
        assert_eq!(after.3, before.3);
        assert_eq!(after.4, before.4);
        assert!(after
            .3
            .is_some_and(|p| p.tables[0].columns[0].samples == ["a", "b"]));
    }

    #[test]
    fn clear_args_parse() {
        let a =
            |v: &[&str]| parse_clear_args(&v.iter().map(ToString::to_string).collect::<Vec<_>>());
        assert_eq!(
            a(&[]).unwrap(),
            ClearArgs {
                confirm: false,
                batch: DEFAULT_BATCH
            }
        );
        assert_eq!(
            a(&["--confirm", "--batch", "7"]).unwrap(),
            ClearArgs {
                confirm: true,
                batch: 7
            }
        );
        assert!(a(&["--batch"]).is_err());
        assert!(a(&["--batch", "0"]).is_err());
        assert!(a(&["--nope"]).is_err());
    }
}

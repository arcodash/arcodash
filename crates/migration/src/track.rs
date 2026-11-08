use sea_orm_migration::sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, Statement};
use std::collections::HashSet;

pub const OSS: &str = "oss";

async fn ensure_table(db: &DatabaseConnection) -> Result<(), DbErr> {
    // Guard-owned rather than a migration: a migration in the shared chain runs under both
    // migrators and can't tell which one is driving it.
    db.execute_unprepared(
        "CREATE TABLE IF NOT EXISTS arcodash_schema_track (\
            id smallint PRIMARY KEY CHECK (id = 1), \
            track text NOT NULL)",
    )
    .await?;
    Ok(())
}

pub async fn current(db: &DatabaseConnection) -> Result<Option<String>, DbErr> {
    ensure_table(db).await?;
    let row = db
        .query_one_raw(Statement::from_string(
            db.get_database_backend(),
            "SELECT track FROM arcodash_schema_track WHERE id = 1".to_string(),
        ))
        .await?;
    row.map(|r| r.try_get_by::<String, _>("track")).transpose()
}

pub async fn claim(db: &DatabaseConnection, track: &str) -> Result<(), String> {
    current(db).await.map_err(|e| e.to_string())?;
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT INTO arcodash_schema_track (id, track) VALUES (1, $1) ON CONFLICT (id) DO NOTHING",
        [track.into()],
    ))
    .await
    .map_err(|e| e.to_string())?;
    match current(db).await.map_err(|e| e.to_string())? {
        Some(stored) if stored == track => Ok(()),
        Some(stored) => Err(mismatch_message(&stored, track)),
        None => Err(format!(
            "failed to claim schema track '{track}': no row present after insert"
        )),
    }
}

fn mismatch_message(stored: &str, wanted: &str) -> String {
    format!(
        "refusing to migrate: this database belongs to the '{stored}' migration track, but this\n\
migrator applies the '{wanted}' track.\n\
\n\
DATABASE_URL is pointing at the wrong database. Applying '{wanted}' migrations here would\n\
write ledger entries the '{stored}' migrator has no files for, leaving the database\n\
permanently unmigratable by it.\n\
\n\
If DATABASE_URL is right and '{stored}' is simply the wrong stamp for this database's real\n\
schema history, correct the marker directly (it is a label, not a schema constraint):\n\
  UPDATE arcodash_schema_track SET track = '{wanted}' WHERE id = 1;\n\
\n\
Otherwise, run the migrator for the '{stored}' track against this database instead."
    )
}

/// Whether an explicit `ARCODASH_SCHEMA_TRACK` value authorizes claiming `wanted` for an unmarked
/// database.
///
/// Takes the value as a plain argument (rather than reading the process environment itself) so
/// callers can unit-test the matching rule without mutating global state.
#[must_use]
pub fn overridden_for(env_value: Option<&str>, wanted: &str) -> bool {
    env_value == Some(wanted)
}

pub async fn ledger_versions(db: &DatabaseConnection) -> Result<HashSet<String>, DbErr> {
    // seaql_migrations may not exist yet — this guard runs before sea-orm installs it.
    let exists = db
        .query_one_raw(Statement::from_string(
            db.get_database_backend(),
            "SELECT to_regclass('seaql_migrations') IS NOT NULL AS present".to_string(),
        ))
        .await?
        .map(|r| r.try_get_by::<bool, _>("present"))
        .transpose()?
        .unwrap_or(false);
    if !exists {
        return Ok(HashSet::new());
    }
    db.query_all_raw(Statement::from_string(
        db.get_database_backend(),
        "SELECT version FROM seaql_migrations".to_string(),
    ))
    .await?
    .into_iter()
    .map(|r| r.try_get_by::<String, _>("version"))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OTHER: &str = "other";

    #[test]
    fn mismatch_message_names_both_tracks() {
        let msg = mismatch_message("other", OSS);
        assert!(msg.contains("'other'") && msg.contains("'oss'"), "{msg}");
    }

    #[test]
    fn overridden_for_matches_exact_value_only() {
        assert!(overridden_for(Some(OSS), OSS));
        assert!(overridden_for(Some(OTHER), OTHER));
        assert!(!overridden_for(Some(OSS), OTHER));
        assert!(!overridden_for(Some(OTHER), OSS));
        assert!(!overridden_for(None, OSS));
        assert!(!overridden_for(None, OTHER));
    }
}

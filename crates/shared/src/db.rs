use std::time::Duration;

use anyhow::Result;
use sea_orm::{ConnectOptions, Database, DatabaseConnection};

/// Pool defaults, stated rather than inherited. The values match sqlx's own defaults, so this
/// changes no behavior on its own — it makes the pool size visible and tunable for a process
/// whose job concurrency can exceed it (see `apps/worker`). Every SQL statement is logged at
/// `info` only when `LOG_SQL` is on; a malformed `LOG_SQL` is a startup error.
fn connect_options(database_url: &str) -> Result<ConnectOptions> {
    let mut opts = ConnectOptions::new(database_url.to_owned());
    opts.max_connections(crate::config::env_parsed("DB_MAX_CONNECTIONS", 10))
        .min_connections(crate::config::env_parsed("DB_MIN_CONNECTIONS", 0))
        .acquire_timeout(Duration::from_secs(30))
        .idle_timeout(Duration::from_secs(600))
        .max_lifetime(Duration::from_mins(30))
        .sqlx_logging(crate::config::parse_bool_env("LOG_SQL", false)?);
    Ok(opts)
}

/// Connect to Postgres via `SeaORM` using the given connection string.
pub async fn connect(database_url: &str) -> Result<DatabaseConnection> {
    Ok(Database::connect(connect_options(database_url)?).await?)
}

/// Same as [`connect`], but caps the pool at `max_connections`.
///
/// Used by composition binaries that run two pools against the same Postgres instance (e.g. a
/// restricted app-role pool plus an owner admin pool) and need to split the connection budget
/// between them.
///
/// Overrides `DB_MAX_CONNECTIONS` — the caller's explicit split always wins over the env-wide
/// default.
pub async fn connect_with_max(
    database_url: &str,
    max_connections: u32,
) -> Result<DatabaseConnection> {
    let mut opts = connect_options(database_url)?;
    opts.max_connections(max_connections);
    Ok(Database::connect(opts).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `env::set_var`/`remove_var` mutate process-global state; serialize the tests that touch
    // these env vars so they can't observe each other's values.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn connect_options_reads_the_pool_size_from_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("DB_MAX_CONNECTIONS", "25");
        }
        let opts = connect_options("postgres://u:p@h/db").unwrap();
        assert_eq!(opts.get_max_connections(), Some(25));
        unsafe {
            std::env::remove_var("DB_MAX_CONNECTIONS");
        }
    }

    #[test]
    fn connect_options_defaults_match_todays_values() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("DB_MAX_CONNECTIONS");
            std::env::remove_var("DB_MIN_CONNECTIONS");
        }
        let opts = connect_options("postgres://u:p@h/db").unwrap();
        assert_eq!(opts.get_max_connections(), Some(10));
        assert_eq!(opts.get_min_connections(), Some(0));
    }

    #[test]
    fn sql_statement_logging_is_off_unless_log_sql_is_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("LOG_SQL");
        }
        assert!(!connect_options("postgres://u:p@h/db")
            .unwrap()
            .get_sqlx_logging());
        unsafe {
            std::env::set_var("LOG_SQL", "true");
        }
        assert!(connect_options("postgres://u:p@h/db")
            .unwrap()
            .get_sqlx_logging());
        unsafe {
            std::env::set_var("LOG_SQL", "maybe");
        }
        assert!(connect_options("postgres://u:p@h/db").is_err());
        unsafe {
            std::env::remove_var("LOG_SQL");
        }
    }
}

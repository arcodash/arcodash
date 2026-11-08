use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::Database;

use arcodash_migration::{assert_track, Migrator};

#[tokio::main]
async fn main() {
    // Loads DATABASE_URL from a local .env if present; in containers env is injected directly.
    match dotenvy::dotenv() {
        Ok(_) => {}
        Err(e) if e.not_found() => {}
        Err(e) => eprintln!("warning: failed to load .env: {e}"),
    }
    cli::run_cli_with_connection(Migrator, |opts| async move {
        let db = Database::connect(opts).await?;
        if let Err(msg) = assert_track(&db).await {
            eprintln!("{msg}");
            std::process::exit(1);
        }
        Ok(db)
    })
    .await;
}

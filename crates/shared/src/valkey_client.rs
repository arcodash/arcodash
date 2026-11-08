use anyhow::Result;
use redis::aio::ConnectionManager;
use redis::Client;

/// Connect to Valkey (RESP-compatible, hence the `redis` crate) using a self-reconnecting
/// `ConnectionManager`.
pub async fn connect(redis_url: &str) -> Result<ConnectionManager> {
    Ok(Client::open(redis_url)?.get_connection_manager().await?)
}

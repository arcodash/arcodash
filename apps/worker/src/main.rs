use arcodash_worker::WorkerExtensions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    arcodash_worker::run_cli(WorkerExtensions::default()).await
}

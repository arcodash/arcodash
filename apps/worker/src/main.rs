use arcodash_worker::WorkerExtensions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    arcodash_worker::run(WorkerExtensions::default()).await
}

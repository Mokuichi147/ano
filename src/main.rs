#[tokio::main]
async fn main() -> anyhow::Result<()> {
    ano::interface::cli::run().await
}

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    flowz::cli::run().await
}

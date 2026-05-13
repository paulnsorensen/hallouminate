#[tokio::main]
async fn main() -> anyhow::Result<()> {
    hallouminate::app::run().await
}

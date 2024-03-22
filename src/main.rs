use mod_launcher::launch_minecraft;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    launch_minecraft().await
}

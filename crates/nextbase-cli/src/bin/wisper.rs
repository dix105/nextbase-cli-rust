#[tokio::main]
async fn main() {
    if let Err(error) = nextbase_cli::run_wisper().await {
        nextbase_cli::ui::failure(&error.to_string());
        std::process::exit(1);
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = nextbase_cli::run_nextbase().await {
        nextbase_cli::ui::failure(&error.to_string());
        std::process::exit(1);
    }
}

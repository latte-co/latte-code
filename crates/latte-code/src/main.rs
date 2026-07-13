#[tokio::main]
async fn main() {
    let code = latte_code::run_cli().await;
    if code != 0 {
        std::process::exit(code);
    }
}

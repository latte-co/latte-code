#[tokio::main]
async fn main() {
    let code = lattecode::run_cli().await;
    if code != 0 {
        std::process::exit(code);
    }
}

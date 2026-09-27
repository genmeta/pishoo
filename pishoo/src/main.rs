#[tokio::main]
async fn main() {
    if let Err(error) = pishoo::run().await {
        eprintln!("pishoo: {error}");
        std::process::exit(1);
    }
}

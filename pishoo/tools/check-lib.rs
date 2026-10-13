//! Validate a WEBC release through the same CLI check used by local installation.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("usage: check-lib FILE.webc")?;
    pishoo::run_command(vec!["lib".into(), "check".into(), path]).await?;
    Ok(())
}

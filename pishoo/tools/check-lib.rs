//! Check a packaged Pishoo component before deploying it.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os().nth(1).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage: check-lib FILE.wasm",
        )
    })?;
    let bytes = std::fs::read(&path)?;
    let api = pishoo::validate_lib(&bytes)?;
    println!(
        "valid Pishoo Lib: {} path(s)",
        api.paths.as_ref().map_or(0, |paths| paths.len())
    );
    Ok(())
}

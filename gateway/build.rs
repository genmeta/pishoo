use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());

    // Allow overriding the root CA via the ROOT_CA environment variable.
    // Default: keychain/root.crt relative to the workspace root (one level up from gateway/).
    let default_path = manifest_dir.join("../keychain/root.crt");
    let src = match std::env::var("ROOT_CA") {
        Ok(path) => PathBuf::from(path),
        Err(_) => default_path,
    };

    let dest = out_dir.join("root.crt");
    std::fs::copy(&src, &dest).unwrap_or_else(|e| {
        panic!(
            "failed to copy root CA from {} to {}: {e}",
            src.display(),
            dest.display()
        )
    });

    println!("cargo::rerun-if-env-changed=ROOT_CA");
    println!("cargo::rerun-if-changed={}", src.display());
}

use std::{
    env,
    path::Path,
    process::{Command, ExitStatus},
};

fn main() {
    for path in [
        "workspace/index.html",
        "workspace/package.json",
        "workspace/bun.lock",
        "workspace/vite.config.ts",
        "workspace/playwright.config.ts",
        "workspace/tsconfig.json",
        "workspace/tsconfig.app.json",
        "workspace/tsconfig.e2e.json",
        "workspace/tsconfig.node.json",
        "workspace/src",
        "workspace/e2e",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }

    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")
        .expect("Cargo should provide CARGO_MANIFEST_DIR to build scripts");
    let workspace_dir = Path::new(&manifest_dir).join("workspace");

    run_bun(&workspace_dir, &["install", "--frozen-lockfile"]);
    run_bun(&workspace_dir, &["run", "build"]);
}

fn run_bun(workspace_dir: &Path, args: &[&str]) {
    let status = Command::new("bun")
        .args(args)
        .current_dir(workspace_dir)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "failed to run Bun for pishoo Workspace: {error}; install Bun and ensure it is available on PATH"
            )
        });

    if !status.success() {
        panic!(
            "Bun command failed while building pishoo Workspace: {}",
            format_status(status)
        );
    }
}

fn format_status(status: ExitStatus) -> String {
    status.code().map_or_else(
        || "terminated by signal".to_owned(),
        |code| format!("exit code {code}"),
    )
}

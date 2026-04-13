//! Root log management: log directory and log file reopening.

use std::path::Path;

use snafu::Report;

pub(crate) const ROOT_LOG_DIR: &str = "/var/log/pishoo";

/// Reopen the root process log file (`/var/log/pishoo/root.log`).
///
/// Creates the log directory if it doesn't exist, then redirects stderr
/// to the (possibly new) log file. Called on SIGUSR1.
pub fn reopen_root_log() {
    use std::fs::OpenOptions;

    let log_dir = Path::new(ROOT_LOG_DIR);
    if let Err(error) = std::fs::create_dir_all(log_dir) {
        tracing::warn!(error = %Report::from_error(&error), dir = %log_dir.display(), "failed to create root log directory");
        return;
    }
    let log_file = log_dir.join("root.log");
    let file = match OpenOptions::new().create(true).append(true).open(&log_file) {
        Ok(f) => f,
        Err(error) => {
            tracing::warn!(error = %Report::from_error(&error), path = %log_file.display(), "failed to open root log file");
            return;
        }
    };
    if let Err(error) = nix::unistd::dup2_stderr(&file) {
        tracing::warn!(
            error = %Report::from_error(&error),
            "failed to dup2 stderr for root log reopen"
        );
    }
}

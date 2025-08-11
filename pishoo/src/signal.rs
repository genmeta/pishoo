use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use tokio::{sync::broadcast, task::JoinSet};

#[cfg(unix)]
mod unix;

#[cfg(windows)]
mod windows;

pub fn send_signal(pid_file: &str, signal_type: &str) -> Result<()> {
    #[cfg(unix)]
    {
        unix::send_signal(pid_file, signal_type)
    }

    #[cfg(windows)]
    {
        tracing::warn!("Signal sending not supported on this platform");
        Ok(())
    }
}

pub async fn handle_signal(
    shutdown_tx: &broadcast::Sender<()>,
    config_file: PathBuf,
    handler: &Arc<tokio::sync::Mutex<JoinSet<anyhow::Result<()>>>>,
) -> Result<()> {
    #[cfg(unix)]
    {
        unix::handle_signal(shutdown_tx, config_file, handler).await
    }

    #[cfg(not(unix))]
    {
        tracing::warn!("Signal handling not supported on this platform");
        Ok(())
    }
}

pub fn init_pid_file(pid_file: &str) -> Result<()> {
    #[cfg(unix)]
    {
        unix::init_pid_file(pid_file)
    }

    #[cfg(windows)]
    {
        tracing::warn!("PID file writing not supported on this platform");
        Ok(())
    }
}

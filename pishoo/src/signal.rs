use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use tokio::task::JoinSet;

use crate::SignalType;

#[cfg(unix)]
mod unix;

#[cfg(windows)]
mod windows;

pub async fn send_signal(pid_file: &str, signal_type: SignalType) -> Result<()> {
    #[cfg(unix)]
    {
        unix::send_signal(pid_file, signal_type).await
    }

    #[cfg(windows)]
    {
        tracing::warn!("Signal sending not supported on this platform");
        Ok(())
    }
}

pub async fn handle_signal(
    config_file: PathBuf,
    handler: &Arc<tokio::sync::Mutex<JoinSet<()>>>,
) -> Result<()> {
    #[cfg(unix)]
    {
        unix::handle_signal(config_file, handler).await
    }

    #[cfg(not(unix))]
    {
        tracing::warn!("Signal handling not supported on this platform");
        Ok(())
    }
}

pub async fn init_pid_file(pid_file: &str) -> Result<()> {
    #[cfg(unix)]
    {
        unix::init_pid_file(pid_file).await
    }

    #[cfg(windows)]
    {
        tracing::warn!("PID file writing not supported on this platform");
        Ok(())
    }
}

use std::{path::PathBuf, sync::Arc};

use gateway::error::Whatever;
use tokio::task::JoinSet;

use crate::SignalType;

#[cfg(unix)]
mod unix;

#[cfg(windows)]
mod windows;

pub async fn send_signal(pid_file: &str, signal_type: SignalType) -> Result<(), Whatever> {
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
) -> Result<(), Whatever> {
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

pub async fn init_pid_file(pid_file: &str) -> Result<(), Whatever> {
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

use gateway::error::Whatever;

use crate::SignalType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownSignal {
    SigTerm,
    SigInt,
}

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
) -> Result<Option<ShutdownSignal>, Whatever> {
    #[cfg(unix)]
    {
        unix::handle_signal().await
    }

    #[cfg(not(unix))]
    {
        tracing::warn!("Signal handling not supported on this platform");
        Ok(None)
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

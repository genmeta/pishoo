use gateway::error::Whatever;

use crate::SignalType;

/// Signal received by the root supervisor process.
///
/// Represents every Unix signal the root listens for. The root is responsible
/// for forwarding the appropriate signal to workers and managing its own
/// lifecycle (shutdown vs. stay-alive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSignal {
    /// SIGTERM — fast shutdown.
    SigTerm,
    /// SIGINT — fast shutdown (Ctrl+C).
    SigInt,
    /// SIGQUIT — graceful shutdown.
    SigQuit,
    /// SIGHUP — reload (forward to workers, root stays alive).
    SigHup,
    /// SIGUSR1 — reopen logs (root reopens its own, then forwards to workers).
    SigUsr1,
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

pub async fn handle_signal() -> Result<Option<RootSignal>, Whatever> {
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

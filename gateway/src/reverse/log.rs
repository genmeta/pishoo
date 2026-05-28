use std::{io::Write, path::PathBuf, sync::Arc};

use dhttp_home::identity::IdentityProfile;
use tracing_appender::{
    non_blocking::{NonBlocking, WorkerGuard},
    rolling::{RollingFileAppender, Rotation},
};

/// A non-blocking, clone-able access log writer backed by
/// `tracing_appender::non_blocking`.
///
/// Created once at service startup per identity; shared across all requests
/// for that identity via axum middleware state. The internal [`WorkerGuard`]
/// is held via `Arc` so that clones keep the background I/O thread alive
/// until the last reference is dropped.
#[derive(Clone, Debug)]
pub struct AccessLogWriter {
    inner: NonBlocking,
    /// Prevent the background thread from shutting down while any clone exists.
    _guard: Arc<WorkerGuard>,
}

impl AccessLogWriter {
    /// Create a new writer that appends to `{log_dir}/access.log`.
    ///
    /// The directory is created if it does not exist.
    pub fn new(log_dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&log_dir)?;

        let appender =
            RollingFileAppender::new(Rotation::NEVER, log_dir, IdentityProfile::ACCESS_LOG_FILE);
        let (non_blocking, guard) = tracing_appender::non_blocking(appender);

        Ok(Self {
            inner: non_blocking,
            _guard: Arc::new(guard),
        })
    }

    /// Create a writer that discards all output.
    ///
    /// Used as a fallback when the log directory cannot be created.
    pub fn disabled() -> Self {
        let (non_blocking, guard) = tracing_appender::non_blocking(std::io::sink());
        Self {
            inner: non_blocking,
            _guard: Arc::new(guard),
        }
    }
}

impl Write for AccessLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }

    /// Write the entire buffer in a single call.
    ///
    /// A single `write_all` is required because [`NonBlocking`] sends each
    /// call as an independent message; splitting across two calls may
    /// interleave with other writers.
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.inner.write_all(buf)
    }
}

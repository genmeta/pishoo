use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use dhttp::log::access::{AccessLogRecord, DefaultAccessFormatter};
use snafu::{Report, ResultExt, Snafu};
use tracing_appender::{
    non_blocking::{NonBlocking, WorkerGuard},
    rolling::{RollingFileAppender, Rotation},
};

use crate::parse::domain::ResolvedConfigPath;

#[derive(Debug, Snafu)]
#[snafu(module(open_access_log_output_error))]
pub enum OpenAccessLogOutputError {
    #[snafu(display("access log path `{}` has no file name", path.display()))]
    MissingFileName { path: PathBuf },
    #[snafu(display("access log file name at `{}` is not UTF-8", path.display()))]
    NonUtf8FileName { path: PathBuf },
    #[snafu(display("failed to create access log parent directory `{}`", path.display()))]
    CreateParent {
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("failed to open access log output `{}`", path.display()))]
    Open {
        path: PathBuf,
        source: tracing_appender::rolling::InitError,
    },
}

/// One process-owned destination for fully formatted access records.
#[derive(Debug)]
pub struct AccessLogOutput {
    path: ResolvedConfigPath,
    writer: AccessLogWriter,
}

impl AccessLogOutput {
    pub fn open(path: ResolvedConfigPath) -> Result<Self, OpenAccessLogOutputError> {
        let writer = AccessLogWriter::new(path.as_ref())?;
        Ok(Self { path, writer })
    }

    pub fn path(&self) -> &ResolvedConfigPath {
        &self.path
    }

    /// Formatting and delivery are deliberately best-effort after the output
    /// has been acquired. A request response must not depend on log delivery.
    pub fn write(&self, record: &AccessLogRecord) {
        let formatted = match DefaultAccessFormatter::format(record) {
            Ok(formatted) => formatted,
            Err(error) => {
                tracing::warn!(
                    path = %self.path.as_ref().display(),
                    error = %Report::from_error(&error),
                    "failed to format access log record"
                );
                return;
            }
        };

        let mut writer = self.writer.clone();
        if let Err(error) = writer.write_all(formatted.as_bytes()) {
            tracing::warn!(
                path = %self.path.as_ref().display(),
                error = %Report::from_error(&error),
                "failed to write access log record"
            );
        }
    }
}

/// Cloneable non-blocking writer for one exact file path.
#[derive(Clone, Debug)]
pub struct AccessLogWriter {
    inner: NonBlocking,
    _guard: Arc<WorkerGuard>,
}

impl AccessLogWriter {
    pub fn new(path: &Path) -> Result<Self, OpenAccessLogOutputError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name =
            path.file_name()
                .ok_or_else(|| OpenAccessLogOutputError::MissingFileName {
                    path: path.to_path_buf(),
                })?;
        let file_name =
            file_name
                .to_str()
                .ok_or_else(|| OpenAccessLogOutputError::NonUtf8FileName {
                    path: path.to_path_buf(),
                })?;

        std::fs::create_dir_all(parent)
            .context(open_access_log_output_error::CreateParentSnafu { path: parent })?;
        let appender = RollingFileAppender::builder()
            .rotation(Rotation::NEVER)
            .filename_prefix(file_name)
            .build(parent)
            .context(open_access_log_output_error::OpenSnafu { path })?;
        let (inner, guard) = tracing_appender::non_blocking(appender);
        Ok(Self {
            inner,
            _guard: Arc::new(guard),
        })
    }
}

impl Write for AccessLogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }

    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.write_all(bytes)
    }
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, time::Duration};

    use chrono::Local;
    use dhttp::log::access::{
        AccessRequestTarget, BodyBytesEmitted, ClientAddress, OptionalReferer, OptionalUserAgent,
    };

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "gateway-access-output-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample_record() -> AccessLogRecord {
        AccessLogRecord {
            completed_at: Local::now().fixed_offset(),
            client: ClientAddress::Ip(Ipv4Addr::LOCALHOST.into()),
            method: http::Method::GET,
            target: "/health".parse::<AccessRequestTarget>().unwrap(),
            version: http::Version::HTTP_3,
            status: http::StatusCode::OK,
            body_bytes: BodyBytesEmitted::from(2),
            referer: OptionalReferer::default(),
            user_agent: OptionalUserAgent::default(),
        }
    }

    #[test]
    fn writer_opens_the_exact_resolved_file() {
        let temp = TempDir::new();
        let path = temp.0.join("nested/custom.log");
        let resolved = ResolvedConfigPath::try_from(path.clone()).unwrap();
        let output = AccessLogOutput::open(resolved).unwrap();
        output.write(&sample_record());
        drop(output);

        for _ in 0..100 {
            if path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(path.exists());
        assert!(!temp.0.join("nested/access.log").exists());
    }
}

use std::{net::SocketAddr, path::PathBuf};

use chrono::Local;
use genmeta_home::GenmetaHome;
use http::{Method, Request, Version};
use snafu::{Report, ResultExt};
use tokio::{fs::OpenOptions, io::AsyncWriteExt};

use crate::error::Result;

// Returns None on Android (no logging)
fn get_log_dir() -> Option<PathBuf> {
    GenmetaHome::load_from_environment().map(|home| home.join("logs"))
}

fn get_access_log_path() -> Option<PathBuf> {
    get_log_dir().map(|dir| dir.join("access.log"))
}

fn get_error_log_path() -> Option<PathBuf> {
    get_log_dir().map(|dir| dir.join("error.log"))
}

async fn ensure_log_dir() -> Result<()> {
    if let Some(dir) = get_log_dir() {
        tokio::fs::create_dir_all(&dir)
            .await
            .whatever_context::<_, crate::error::CustomError>(format!(
                "failed to create log directory: {:?}",
                dir
            ))?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct RequestInfo {
    pub method: Method,
    pub uri: String,
    pub version: Version,
    pub user_agent: String,
    pub referer: String,
    pub client_addr: String,
}

impl RequestInfo {
    pub fn from_request<T>(req: &Request<T>) -> Self {
        Self {
            method: req.method().clone(),
            uri: req.uri().to_string(),
            version: req.version(),
            user_agent: req
                .headers()
                .get("user-agent")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
                .to_string(),
            referer: req
                .headers()
                .get("referer")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
                .to_string(),
            client_addr: req
                .extensions()
                .get::<SocketAddr>()
                .map(|a| a.ip().to_string())
                .unwrap_or_else(|| "-".to_string()),
        }
    }

    pub async fn log_access(&self, status: u16, body_size: u64) {
        let time_local = Local::now().format("%d/%b/%Y:%H:%M:%S %z");
        let log_line = format!(
            "{} - - [{}] \"{} {} {:?}\" {} {} \"{}\" \"{}\"",
            self.client_addr,
            time_local,
            self.method,
            self.uri,
            self.version,
            status,
            body_size,
            self.referer,
            self.user_agent
        );
        write_access_log(log_line).await;
    }

    pub async fn log_error(&self, message: impl AsRef<str>) {
        write_error_log(message.as_ref().to_string()).await;
    }
}

pub async fn write_access_log(line: String) {
    let Some(path) = get_access_log_path() else {
        return;
    };

    let result: Result<()> = async {
        ensure_log_dir().await?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .whatever_context::<_, crate::error::CustomError>(format!(
                "failed to open access log file: {:?}",
                path
            ))?;

        file.write_all(line.as_bytes())
            .await
            .whatever_context::<_, crate::error::CustomError>("failed to write to access log")?;
        file.write_all(b"\n")
            .await
            .whatever_context::<_, crate::error::CustomError>("failed to write to access log")?;
        Ok(())
    }
    .await;

    if let Err(e) = result {
        let report = Report::from_error(e).to_string();
        tracing::error!(
            error = report,
            "failed to write access log"
        );
    }
}

pub async fn write_error_log(line: String) {
    let Some(path) = get_error_log_path() else {
        return;
    };

    let result: Result<()> = async {
        ensure_log_dir().await?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .whatever_context::<_, crate::error::CustomError>(format!(
                "failed to open error log file: {:?}",
                path
            ))?;

        let timestamp = Local::now().format("%Y/%m/%d %H:%M:%S");
        let formatted_line = format!("{} [error] {}\n", timestamp, line);

        file.write_all(formatted_line.as_bytes())
            .await
            .whatever_context::<_, crate::error::CustomError>("failed to write to error log")?;
        Ok(())
    }
    .await;

    if let Err(e) = result {
        let report = Report::from_error(e).to_string();
        tracing::error!(
            error = report,
            "failed to write error log"
        );
    }
}

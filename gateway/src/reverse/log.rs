use std::path::PathBuf;

use chrono::Local;
use snafu::ResultExt;
use tokio::{fs::OpenOptions, io::AsyncWriteExt};

use crate::error::Result;

pub async fn write_access_log(path: PathBuf, line: String) {
    let result: Result<()> = async {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .whatever_context::<_, crate::error::CustomError>(format!(
                "Failed to open access log file: {:?}",
                path
            ))?;

        file.write_all(line.as_bytes())
            .await
            .whatever_context::<_, crate::error::CustomError>("Failed to write to access log")?;
        file.write_all(b"\n")
            .await
            .whatever_context::<_, crate::error::CustomError>("Failed to write to access log")?;
        Ok(())
    }
    .await;

    if let Err(e) = result {
        tracing::error!(target: "access_log", "Failed to write access log: {:?}", e);
    }
}

pub async fn write_error_log(path: PathBuf, line: String) {
    let result: Result<()> = async {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .whatever_context::<_, crate::error::CustomError>(format!(
                "Failed to open error log file: {:?}",
                path
            ))?;

        let timestamp = Local::now().format("%Y/%m/%d %H:%M:%S");
        let formatted_line = format!("{} [error] {}\n", timestamp, line);

        file.write_all(formatted_line.as_bytes())
            .await
            .whatever_context::<_, crate::error::CustomError>("Failed to write to error log")?;
        Ok(())
    }
    .await;

    if let Err(e) = result {
        tracing::error!(target: "error_log", "Failed to write error log: {:?}", e);
    }
}

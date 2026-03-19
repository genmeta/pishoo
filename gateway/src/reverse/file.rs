use std::sync::Arc;

use h3x::message::stream::WriteStream;
use http::{Request, Response, StatusCode, header::CONTENT_LENGTH};
use snafu::{Report, ResultExt};
use tokio::io::{AsyncWriteExt, BufReader};
use tracing::{debug, error, info};

use crate::{
    command::{self, IndexError, content_type, index},
    error::{Result, StreamSnafu, Whatever},
    parse::{Node, Value},
    reverse::{gzip::GzipConfig, log::RequestInfo},
};

/// Handles incoming HTTP requests, attempting to serve a static file based on a configured root directory.
///
/// It retrieves the root directory path from the `location` configuration.
/// If the root path is found, it constructs the full path to the requested file
/// by combining the root path and the request's URI path. It then delegates
/// the actual file serving process to the `serve_static_file` function.
///
/// If the `root` path configuration is missing or invalid in `location`,
/// it sends a generic error response back to the client.
///
/// # Parameters
///
/// * `location`: An `Arc<Node>` containing configuration, expected to hold a `root` path value.
/// * `req`: The incoming HTTP `Request`. Its URI path is used to determine the file to serve.
/// * `sender`: The `WriteStream` used to send the HTTP response (either the file or an error) back to the client.
///
/// # Returns
///
/// * `Ok(())` if a response (either the file or an error message) was successfully initiated.
/// * `Err` if an error occurred during the process of sending the response (e.g., I/O error in `serve_static_file`).
pub async fn root(location: &Arc<Node>, req: Request<()>, sender: WriteStream) -> Result<()> {
    let Some(Value::Path(root)) = location.get("root") else {
        unreachable!()
    };

    let file_path = format!("{}{}", root.display(), req.uri().path());
    serve_static_file(location, &file_path, &req, sender).await?;
    Ok(())
}

/// Handles HTTP requests that match a specific `pattern` by serving files from an aliased directory.
///
/// This function is typically used when a URL prefix (`pattern`) should map to a
/// different directory (`alias`) on the filesystem than the one implied by the
/// URL structure alone. It removes the matched `pattern` from the beginning of
/// the request URI's path to determine the relative path within the `alias` directory.
///
/// It retrieves the `alias` target directory path from the `location` configuration.
/// If the `alias` path is found, it constructs the full path to the requested file
/// and delegates the file serving process to `serve_static_file`.
///
/// If the `alias` path configuration is missing or invalid in `location`,
/// it sends a generic error response back to the client.
///
/// # Parameters
///
/// * `location`: An `Arc<Node>` containing configuration, expected to hold an `alias` path value.
/// * `pattern`: The URL path prefix that was matched to invoke this handler.
/// * `req`: The incoming HTTP `Request`. Its URI path, relative to the `pattern`, is used.
/// * `sender`: The `WriteStream` used to send the HTTP response (either the file or an error) back to the client.
///
/// # Returns
///
/// * `Ok(())` if a response (either the file or an error message) was successfully initiated.
/// * `Err` if an error occurred during the process of sending the response (e.g., I/O error in `serve_static_file`).
pub async fn alias(
    location: &Arc<Node>,
    pattern: &str,
    req: Request<()>,
    sender: WriteStream,
) -> Result<()> {
    // In the case of an `alias`, it is necessary to remove the matched prefix from the URL.
    let relative_path = req.uri().path().trim_start_matches(pattern);

    let Some(Value::Path(alias)) = location.get("alias") else {
        unreachable!()
    };

    let file_path = format!("{}{}", alias.display(), relative_path);
    serve_static_file(location, &file_path, &req, sender).await?;
    Ok(())
}

/// Asynchronously serves a static file located at `file_path`.
///
/// This function attempts to find the specified file using the `location` context.
/// If found, it sends an HTTP 200 OK response with appropriate `Content-Type`
/// and `Content-Length` headers, then streams the file content back to the client.
/// If the file is not found, it sends a 404 Not Found response.
///
/// # Parameters
///
/// * `location`: Context used to locate the file (e.g., root directory configuration).
/// * `file_path`: The path to the static file relative to the `location`.
/// * `uri`: The original request URI, used primarily for logging purposes.
/// * `sender`: The stream used to send the HTTP response (headers and body data) back to the client.
///
/// # Returns
///
/// * `Ok(())` if the file was served successfully or a 404 response was sent.
/// * `Err` if there was an I/O error reading the file or an error sending the response.
async fn serve_static_file(
    location: &Arc<Node>,
    file_path: &str,
    req: &Request<()>,
    mut sender: WriteStream,
) -> Result<()> {
    let req_info = RequestInfo::from_request(req);
    let uri = req.uri();

    debug!(uri = %uri, "processing static file");

    let (file, file_size, file_path) = match index(location, file_path).await {
        Ok(result) => result,
        Err(index_error @ (IndexError::MissingIndexFiles | IndexError::FileNotFound { .. })) => {
            let report = Report::from_error(&index_error).to_string();
            tracing::info!(
                uri = %uri,
                path = file_path,
                error = report,
                "static file was not found"
            );
            super::send_status_and_close(sender, StatusCode::NOT_FOUND).await?;
            req_info.log_access(404, 0).await;
            return Ok(());
        }
        Err(error) => {
            let report = Report::from_error(&error).to_string();
            error!(
                uri = %uri,
                path = file_path,
                error = report,
                "failed to resolve static file"
            );
            return Err(error).whatever_context::<_, Whatever>("failed to resolve static file")?;
        }
    };

    let (mut parts, _body) = Response::<()>::default().into_parts();

    parts.status = StatusCode::OK;

    command::add_header(location, &mut parts);
    content_type(location, &mut parts, &file_path);

    let accept_encoding = req
        .headers()
        .get(http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let gzip = GzipConfig::from_location(location, accept_encoding);

    let should_compress = gzip.should_compress(&parts, Some(file_size));

    if should_compress {
        gzip.apply_headers(&mut parts);
    } else {
        parts.headers.insert(CONTENT_LENGTH, file_size.into());
    }

    // 发送响应头
    sender
        .send_hyper_response_parts(parts)
        .await
        .context(StreamSnafu)?;

    let reader = BufReader::new(file);
    let mut reader = gzip.wrap_reader(should_compress, reader);

    let mut writer = Box::pin(sender.into_writer());
    match tokio::io::copy(&mut reader, &mut writer).await {
        Ok(size) => {
            req_info.log_access(200, size).await;
        }
        Err(error) => {
            let err_msg = format!("failed to send file content: {}", Report::from_error(&error));
            error!(
                uri = %uri,
                error = %Report::from_error(&error),
                "failed to send file content"
            );
            req_info.log_error(&err_msg).await;
            return Err(error).whatever_context::<_, Whatever>("failed to send file content")?;
        }
    }

    match writer.shutdown().await {
        Ok(()) => info!(uri = %uri, "finished sending static file response"),
        Err(error) => {
            error!(
                uri = %uri,
                error = %Report::from_error(&error),
                "failed to finish sending static file response"
            )
        }
    }
    Ok(())
}

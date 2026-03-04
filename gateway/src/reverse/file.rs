use std::sync::Arc;

use async_compression::{Level, tokio::bufread::GzipEncoder};
use h3x::message::stream::WriteStream;
use http::{Request, Response, StatusCode, header::CONTENT_LENGTH};
use snafu::ResultExt;
use tokio::io::{AsyncWriteExt, BufReader};
use tracing::{debug, error, info};

use crate::{
    command::{self, content_type, index},
    error::{Result, StreamSnafu, Whatever},
    parse::{Node, Value},
    reverse::log::RequestInfo,
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

    debug!(target: "static_file", "[Response handling][{}] Processing static file", uri);

    let (file, file_size, file_path) = match index(location, file_path).await {
        Ok(result) => result,
        Err(index_error) => {
            let response = Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(())
                .expect("Failed to build response");

            tracing::info!(target: "static_file", "[Proxy][{}] File not found: {}, error: {}", uri, file_path, index_error);
            let (parts, _) = response.into_parts();
            sender
                .send_hyper_response_parts(parts)
                .await
                .context(StreamSnafu)?;
            sender.close().await.context(StreamSnafu)?;

            req_info.log_access(404, 0).await;
            return Ok(());
        }
    };

    let (mut parts, _body) = Response::<()>::default().into_parts();

    parts.status = StatusCode::OK;

    command::add_header(location, &mut parts);
    content_type(location, &mut parts, &file_path);

    let accept_gzip = req
        .headers()
        .get(http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("gzip"))
        .unwrap_or(false);

    let gzip_enabled = location.get_bool("gzip").unwrap_or(false);
    let gzip_vary = location.get_bool("gzip_vary").unwrap_or(false);
    let gzip_min_length = location.get_str_parsed("gzip_min_length").unwrap_or(20);
    let gzip_comp_level = location.get_str_parsed("gzip_comp_level").unwrap_or(1);
    let gzip_types = location.get_string_vec("gzip_types");

    let content_type = parts
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim())
        .unwrap_or("");

    let length_ok = file_size >= gzip_min_length;
    let type_ok = match &gzip_types {
        None => content_type == "text/html",
        Some(types) => types.iter().any(|t| t == "*" || t == content_type),
    };

    let should_compress = gzip_enabled
        && accept_gzip
        && parts.headers.get(http::header::CONTENT_ENCODING).is_none()
        && length_ok
        && type_ok;

    if should_compress {
        parts
            .headers
            .insert(http::header::CONTENT_ENCODING, "gzip".parse().unwrap());

        if gzip_vary {
            parts
                .headers
                .append(http::header::VARY, "Accept-Encoding".parse().unwrap());
        }
    } else {
        parts.headers.insert(CONTENT_LENGTH, file_size.into());
    }

    // 发送响应头
    sender
        .send_hyper_response_parts(parts)
        .await
        .context(StreamSnafu)?;

    let reader = BufReader::new(file);
    let mut reader: Box<dyn tokio::io::AsyncRead + Unpin + Send> = if should_compress {
        let level = match gzip_comp_level {
            1 => Level::Fastest,
            9 => Level::Best,
            l => Level::Precise(l),
        };
        Box::new(GzipEncoder::with_quality(reader, level))
    } else {
        Box::new(reader)
    };

    let mut writer = Box::pin(sender.into_writer());
    match tokio::io::copy(&mut reader, &mut writer).await {
        Ok(size) => {
            req_info.log_access(200, size).await;
        }
        Err(e) => {
            let err_msg = format!("Failed to send file content: {}", e);
            error!(target: "static_file", "[Proxy][{}] {}", uri, err_msg);
            req_info.log_error(&err_msg).await;
            return Err(e).whatever_context::<_, Whatever>("Failed to send file content")?;
        }
    }

    match writer.shutdown().await {
        Ok(()) => info!(target: "static_file", "[Proxy][{}] Request finished sent", uri),
        Err(e) => {
            error!(target: "static_file", "[Proxy][{}] Error sending request data end: {}", uri, e)
        }
    }
    Ok(())
}

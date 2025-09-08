use std::sync::Arc;

use bytes::Bytes;
use h3::server::RequestStream;
use h3_shim::SendStream;
use http::{Request, Response, StatusCode, Uri, header::CONTENT_LENGTH};
use tokio::io::{AsyncWriteExt, BufReader};
use tracing::{debug, error, info};

use crate::{
    command::{self, content_type, index},
    error::Result,
    h3::H3Sink,
    parse::{Node, Value},
    reverse::build_error_response,
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
/// * `sender`: The `RequestStream` used to send the HTTP response (either the file or an error) back to the client.
///
/// # Returns
///
/// * `Ok(())` if a response (either the file or an error message) was successfully initiated.
/// * `Err` if an error occurred during the process of sending the response (e.g., I/O error in `serve_static_file`).
pub async fn root(
    location: &Arc<Node>,
    req: Request<()>,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<()> {
    let root = if let Some(Value::Path(root)) = location.get("root") {
        root
    } else {
        sender.send_response(build_error_response()).await?;
        sender.finish().await?;
        return Ok(());
    };

    let file_path = format!("{}{}", root.display(), req.uri().path());
    serve_static_file(location, &file_path, req.uri(), sender).await?;
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
/// * `sender`: The `RequestStream` used to send the HTTP response (either the file or an error) back to the client.
///
/// # Returns
///
/// * `Ok(())` if a response (either the file or an error message) was successfully initiated.
/// * `Err` if an error occurred during the process of sending the response (e.g., I/O error in `serve_static_file`).
pub async fn alias(
    location: &Arc<Node>,
    pattern: &str,
    req: Request<()>,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<()> {
    // In the case of an `alias`, it is necessary to remove the matched prefix from the URL.
    let relative_path = req.uri().path().trim_start_matches(pattern);

    let alias = if let Some(Value::Path(alias)) = location.get("alias") {
        alias
    } else {
        sender.send_response(build_error_response()).await?;
        sender.finish().await?;
        return Ok(());
    };

    let file_path = format!("{}{}", alias.display(), relative_path);
    serve_static_file(location, &file_path, req.uri(), sender).await?;
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
    uri: &Uri,
    mut sender: RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<()> {
    debug!("[Response handling][{}] Processing static file", uri);

    let (file, file_size, file_path) = match index(location, file_path).await {
        Ok(result) => result,
        _ => {
            let response = Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(())
                .expect("Failed to build response");

            sender.send_response(response).await?;
            sender.finish().await?;
            return Ok(());
        }
    };

    let (mut parts, body) = Response::<()>::default().into_parts();

    parts.status = StatusCode::OK;

    command::add_header(location, &mut parts);

    // 添加长度
    // TODO gzip 压缩时不添加长度
    parts.headers.insert(CONTENT_LENGTH, file_size.into());
    content_type(location, &mut parts, &file_path);

    let response = Response::from_parts(parts, body);
    sender.send_response(response).await?;

    let mut reader = BufReader::new(file);
    let mut stream = H3Sink::new(sender);
    tokio::io::copy(&mut reader, &mut stream)
        .await
        .inspect_err(|e| error!("[Response handling][{}] Error sending file: {}", uri, e))?;

    match stream.shutdown().await {
        Ok(()) => info!("[Proxy][{}] Request finished sent", uri),
        Err(e) => error!("[Proxy][{}] Error sending request data end: {}", uri, e),
    }
    Ok(())
}

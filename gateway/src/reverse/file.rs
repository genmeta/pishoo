use std::sync::Arc;

use axum::{Extension, response::IntoResponse};
use http::{Response, StatusCode};
use snafu::Report;
use tracing::{debug, error, info};

use crate::{
    command::{self, IndexError, content_type, index},
    parse::{Node, Value},
    reverse::location::LocationMatch,
};

/// Axum-style handler for static file serving.
///
/// Handles both `root` and `alias` directives by using `LocationMatch.remaining`
/// to compute the file path.
pub async fn file_handle(
    Extension(loc): Extension<LocationMatch>,
    req: http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let location = &loc.location;

    let file_path = if let Some(Value::Path(alias)) = location.get("alias") {
        format!("{}{}", alias.display(), loc.remaining)
    } else if let Some(Value::Path(root)) = location.get("root") {
        format!("{}{}", root.display(), req.uri().path())
    } else {
        unreachable!("file_handle requires root or alias")
    };

    let accept_encoding = req
        .headers()
        .get(http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());
    let uri = req.uri().clone();

    // Don't hold a &Request across await (Body is !Sync → &Request is !Send)
    drop(req);

    serve_static_file(loc.location, &file_path, &uri, accept_encoding.as_deref()).await
}

async fn serve_static_file(
    location: Arc<Node>,
    file_path: &str,
    uri: &http::Uri,
    accept_encoding: Option<&str>,
) -> axum::response::Response {
    debug!(uri = %uri, path = file_path, "processing static file");

    let (file, file_size, file_path) = match index(&location, file_path).await {
        Ok(result) => result,
        Err(index_error @ (IndexError::MissingIndexFiles | IndexError::FileNotFound { .. })) => {
            info!(
                uri = %uri,
                path = file_path,
                error = %Report::from_error(&index_error),
                "static file was not found"
            );
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(error) => {
            error!(
                uri = %uri,
                path = file_path,
                error = %Report::from_error(&error),
                "failed to resolve static file"
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let (mut parts, _body) = Response::<()>::default().into_parts();
    parts.status = StatusCode::OK;

    command::add_header(&location, &mut parts);
    content_type(&location, &mut parts, &file_path);

    super::gzip::compress_file_response(&location, accept_encoding, parts, file, file_size)
        .into_response()
}

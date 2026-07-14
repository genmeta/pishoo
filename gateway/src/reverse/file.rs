use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{Extension, response::IntoResponse};
use http::{Response, StatusCode};
use snafu::{Report, ResultExt, Snafu};
use tracing::{debug, error, info, warn};

use crate::{
    command::{self, IndexError, content_type, index},
    parse::{document::ConfigNode, domain::ResolvedConfigPath},
    reverse::location::LocationMatch,
};

#[derive(Debug, Snafu)]
#[snafu(module)]
enum StaticPathError {
    #[snafu(display("unsafe static file path `{requested}` under `{}`", base.display()))]
    UnsafePath {
        base: PathBuf,
        requested: String,
        source: command::file::SafePathError,
    },
}

/// Axum-style handler for static file serving.
///
/// Handles both `root` and `alias` directives by using `LocationMatch.remaining`
/// to compute the file path.
pub async fn file_handle(
    Extension(loc): Extension<LocationMatch>,
    req: http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let location = &loc.location;

    let file_path = if let Some(alias) = location.get::<ResolvedConfigPath>("alias").ok().flatten()
    {
        static_file_path(alias.as_ref().as_ref(), &loc.remaining)
    } else if let Some(root) = location.get::<ResolvedConfigPath>("root").ok().flatten() {
        static_file_path(root.as_ref().as_ref(), req.uri().path())
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

    let file_path = match file_path {
        Ok(file_path) => file_path,
        Err(error) => {
            warn!(
                uri = %uri,
                error = %Report::from_error(&error),
                "unsafe static file path rejected"
            );
            return StatusCode::FORBIDDEN.into_response();
        }
    };

    serve_static_file(loc.location, &file_path, &uri, accept_encoding.as_deref()).await
}

fn static_file_path(base: &Path, requested: &str) -> Result<PathBuf, StaticPathError> {
    let relative = command::file::safe_relative_path(requested)
        .context(static_path_error::UnsafePathSnafu { base, requested })?;
    Ok(base.join(relative))
}

async fn serve_static_file(
    location: Arc<ConfigNode>,
    file_path: &Path,
    uri: &http::Uri,
    accept_encoding: Option<&str>,
) -> axum::response::Response {
    debug!(uri = %uri, path = %file_path.display(), "processing static file");

    let (file, file_size, file_path) = match index(&location, file_path).await {
        Ok(result) => result,
        Err(index_error @ (IndexError::MissingIndexFiles | IndexError::FileNotFound { .. })) => {
            info!(
                uri = %uri,
                path = %file_path.display(),
                error = %Report::from_error(&index_error),
                "static file was not found"
            );
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(error) => {
            error!(
                uri = %uri,
                path = %file_path.display(),
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn static_file_path_rejects_root_parent_traversal() {
        let error = static_file_path(Path::new("/srv/www"), "/../secret.txt")
            .expect_err("parent segment should be rejected");

        assert!(matches!(
            error,
            StaticPathError::UnsafePath {
                source: command::file::SafePathError::ParentDir,
                ..
            }
        ));
    }

    #[test]
    fn static_file_path_rejects_alias_remaining_parent_traversal() {
        let error = static_file_path(Path::new("/srv/assets"), "../secret.txt")
            .expect_err("alias parent segment should be rejected");

        assert!(matches!(
            error,
            StaticPathError::UnsafePath {
                source: command::file::SafePathError::ParentDir,
                ..
            }
        ));
    }

    #[test]
    fn static_file_path_joins_request_under_base() {
        assert_eq!(
            static_file_path(Path::new("/srv/www"), "/assets/app.css")
                .expect("safe path should join"),
            PathBuf::from("/srv/www/assets/app.css")
        );
    }
}

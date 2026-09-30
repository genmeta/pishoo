use std::{
    convert::Infallible,
    future::{Ready, ready},
    task::{Context, Poll},
};

use axum::{
    body::Body,
    response::{IntoResponse, Response},
};
use http::{Request, StatusCode, header};
use include_dir::{Dir, include_dir};

static WORKSPACE_DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/workspace/dist");

#[derive(Clone, Copy, Debug, Default)]
pub struct WorkspaceAssets;

impl tower::Service<Request<Body>> for WorkspaceAssets {
    type Response = Response;
    type Error = Infallible;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        ready(Ok(asset_response(request.uri().path())))
    }
}

fn asset_response(path: &str) -> Response {
    if path == "/workspace" {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [(header::LOCATION, "/workspace/")],
        )
            .into_response();
    }

    let Some(requested) = path.strip_prefix("/workspace/") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if requested.split('/').any(|segment| segment == "..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let file = WORKSPACE_DIST.get_file(requested).or_else(|| {
        std::path::Path::new(requested)
            .extension()
            .is_none()
            .then(|| WORKSPACE_DIST.get_file("index.html"))
            .flatten()
    });
    let Some(file) = file else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let mut response = Response::new(Body::from(file.contents()));
    let content_type = content_type(file.path().extension().and_then(|value| value.to_str()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        content_type.parse().expect("static MIME is valid"),
    );
    if file.path().to_string_lossy().starts_with("assets/") {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            "public, max-age=31536000, immutable"
                .parse()
                .expect("static cache policy is valid"),
        );
    }
    response
}

fn content_type(extension: Option<&str>) -> &'static str {
    match extension {
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        _ => "text/html; charset=utf-8",
    }
}

#[cfg(test)]
mod tests {
    use tower::ServiceExt;

    use super::WorkspaceAssets;

    #[tokio::test]
    async fn workspace_root_redirects_to_the_canonical_path() {
        let response = WorkspaceAssets
            .oneshot(
                http::Request::builder()
                    .uri("/workspace")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("asset service is infallible");

        assert_eq!(response.status(), http::StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.headers()[http::header::LOCATION], "/workspace/");
    }

    #[tokio::test]
    async fn workspace_deep_links_fall_back_to_the_shell() {
        let response = WorkspaceAssets
            .oneshot(
                http::Request::builder()
                    .uri("/workspace/settings/access")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("asset service is infallible");

        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(
            response.headers()[http::header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn missing_workspace_assets_do_not_fall_back_to_html() {
        let response = WorkspaceAssets
            .oneshot(
                http::Request::builder()
                    .uri("/workspace/assets/missing.js")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("asset service is infallible");

        assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
    }
}

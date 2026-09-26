pub(crate) fn management_router(
    access: Arc<AccessService>,
    profile: &str,
    owner_name: &str,
) -> Router {
    let context = serde_json::json!({ "profile": profile, "owner_name": owner_name, "development_identity": false, "demo_data": false, "version": env!("CARGO_PKG_VERSION") });
    access_control::management_router(access)
        .route(
            "/workspace-api/context",
            axum::routing::get(move || {
                let context = context.clone();
                async move { axum::Json(context) }
            }),
        )
        .route(
            "/workspace",
            any(|| async {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(header::LOCATION, "/workspace/")],
                )
            }),
        )
        .route("/workspace/", any(workspace))
        .route("/workspace/{*path}", any(workspace))
}

async fn workspace(request: Request<AxumBody>) -> Response {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return reject(Error::MethodNotAllowed);
    }
    let path = request
        .uri()
        .path()
        .strip_prefix("/workspace/")
        .unwrap_or_default();
    if path.split('/').any(|p| p == "..") || std::path::Path::new(path).extension().is_some() {
        return reject(Error::RouteNotFound);
    }
    let html = include_str!("../../assets/workspace.html");
    let mut response = (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        html,
    )
        .into_response();
    if request.method() == Method::HEAD {
        *response.body_mut() = AxumBody::empty();
    }
    response
}

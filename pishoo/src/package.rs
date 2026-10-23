//! Package acquisition, validation, installation, registration and management HTTP.

use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use dhttp_home::identity::IdentityProfile;

#[cfg(test)]
use crate::sandbox::WasmRuntime;
use crate::{Error, Result, sandbox::Sandbox};

mod files;
mod format;
mod manifest;
mod store;

pub(crate) use files::{
    disk_bytes, installed_apps, installed_packages, lib_root, remove_package, valid_id,
};
#[cfg(test)]
pub(crate) use files::{
    install_app, install_lib, install_package, installed_libs, remove_app, remove_lib,
};
pub(crate) use format::{package_file, package_metadata, read_package};
pub use manifest::{validate_lib, validate_openapi};
pub(crate) use store::install_store_package;
#[cfg(test)]
pub(crate) use store::{STORE_NAME, STORE_OWNER};

fn require_owner(
    endpoint: &dhttp::Endpoint,
    request: &http::Request<axum::body::Body>,
) -> Result<()> {
    // Server's authorization layer creates Visitor from the verified TLS peer.
    let visitor = request
        .extensions()
        .get::<access_control::Visitor>()
        .ok_or(Error::Denied)?;
    let local = endpoint.local_authority()?;
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
        .map_err(|_| Error::Denied)?;
    let subject = access_control::SubjectId::new(ski.owner_hash().as_str().as_bytes())
        .map_err(|_| Error::Denied)?;
    if visitor.name() != endpoint.name() || visitor.subject_id() != &subject {
        return Err(Error::Denied);
    }
    Ok(())
}

fn lib_metadata(package: &webc::Container, id: Option<&str>) -> serde_json::Value {
    // Installed packages are trusted after installation. Catalog reads parse
    // display metadata without repeating package or OpenAPI policy validation.
    let mut value = match package_metadata(package) {
        Ok(value) => value,
        Err(error) => return serde_json::json!({"id":id,"error":error.to_string()}),
    };
    // A runtime snapshot contains installed resources and parsed metadata only.
    // It cannot claim to retain the original manifest's host-purpose text.
    if package.get_volume("pishoo-signature").is_none() {
        value.as_object_mut().unwrap().remove("host");
        value.as_object_mut().unwrap().remove("changelog");
    }
    let mut endpoints = Vec::new();
    if package.get_atom("backend").is_some() {
        let identity = id.unwrap_or_else(|| value["id"].as_str().unwrap());
        let prefix = if identity.starts_with("std.dhttp.net/") {
            "/std".to_owned()
        } else {
            format!("/{}", identity.rsplit('/').next().unwrap())
        };
        let openapi: oas3::OpenApiV3Spec = match package_file(package, "/metadata/openapi.json")
            .and_then(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|error| Error::InvalidComponent(error.to_string()))
            }) {
            Ok(openapi) => openapi,
            Err(error) => return serde_json::json!({"id":id,"error":error.to_string()}),
        };
        if let Some(paths) = &openapi.paths {
            for (path, item) in paths {
                for (method, operation) in item.methods() {
                    let path = format!("{prefix}{path}");
                    endpoints.push(serde_json::json!({"method":method.as_str(),"path":path,
                        "description":operation.summary.as_deref().or(operation.description.as_deref())}));
                }
            }
        }
        value["openapi"] = serde_json::to_value(&openapi).unwrap();
    }
    endpoints.sort_by(|a, b| {
        a["path"]
            .as_str()
            .cmp(&b["path"].as_str())
            .then_with(|| a["method"].as_str().cmp(&b["method"].as_str()))
    });
    value["endpoints"] = endpoints.into();
    if let Some(id) = id {
        value["id"] = id.into();
    }
    value
}

#[cfg(test)]
pub(crate) fn check_lib(bytes: &[u8], runtime: &WasmRuntime) -> Result<oas3::OpenApiV3Spec> {
    let package = read_package(bytes)?;
    let openapi = validate_openapi(&package_file(&package, "/metadata/openapi.json")?)?;
    if let Some(backend) = package.get_atom("backend") {
        runtime.compile(&backend)?;
    }
    Ok(openapi)
}

// Installation metadata belongs to config.db. Neither payload availability nor
// the loaded version affects the installed catalog.
fn installed_catalog(profile: &IdentityProfile, name: Option<&str>) -> Result<serde_json::Value> {
    use rusqlite::OptionalExtension;
    let mut value = if let Some(name) = name {
        let conn = crate::setup::open_config(profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let id: String = conn.query_row("SELECT CASE WHEN author IS NULL THEN name ELSE author||'/'||name END FROM package WHERE name=?1",[name],|r|r.get(0)).optional()?.ok_or(Error::RouteNotFound)?;
        installed_packages(profile, Some(&id))?
    } else {
        installed_packages(profile, None)?
    };
    let convert = |value: &mut serde_json::Value| {
        if let Some(deps) = value["dependencies"].as_array_mut() {
            for dep in deps {
                if let Some(author) = dep.as_object_mut().and_then(|d| d.remove("publisher")) {
                    dep["author"] = author;
                }
            }
        }
    };
    if let Some(values) = value.as_array_mut() {
        for value in values {
            convert(value);
        }
    } else {
        convert(&mut value);
    }
    Ok(value)
}

pub(crate) fn pkg_management_router(
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    sandbox: Arc<RwLock<Sandbox>>,
) -> axum::Router {
    use axum::{response::IntoResponse, routing::any};
    let handler = move |request: http::Request<axum::body::Body>| {
        let (profile, endpoint, sandbox) = (profile.clone(), endpoint.clone(), sandbox.clone());
        async move {
            let mut response = pkg_management_request(profile, endpoint, sandbox, request)
                .await
                .unwrap_or_else(|error| {
                    let status = match &error {
                        Error::InvalidComponent(_) => http::StatusCode::BAD_REQUEST,
                        Error::Io(io) if io.kind() == std::io::ErrorKind::InvalidInput => {
                            http::StatusCode::CONFLICT
                        }
                        _ => error.status(),
                    };
                    if status.is_server_error() {
                        tracing::error!(%error, "Lib management failed");
                        (status, "Lib storage or compilation failed; query the resource to confirm the saved result").into_response()
                    } else {
                        (status, error.to_string()).into_response()
                    }
                });
            response
                .headers_mut()
                .insert(http::header::CACHE_CONTROL, "no-store".parse().unwrap());
            response
                .headers_mut()
                .insert("supported-versions", "v1".parse().unwrap());
            response
        }
    };
    axum::Router::new()
        .route("/std/sys/pkg", any(handler.clone()))
        .route("/std/sys/pkg/list", any(handler.clone()))
        .route("/std/sys/pkg/install", any(handler))
}

async fn pkg_management_request(
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    sandbox: Arc<RwLock<Sandbox>>,
    request: http::Request<axum::body::Body>,
) -> Result<axum::response::Response> {
    use axum::response::IntoResponse;
    use http::{Method, StatusCode, header};
    require_owner(&endpoint, &request)?;
    if request.headers().contains_key("accept-versions") {
        let mut supported = false;
        for value in request.headers().get_all("accept-versions") {
            supported |= value
                .to_str()
                .map_err(|_| Error::BadRequest("invalid Accept-Versions".into()))?
                .split(',')
                .any(|v| v.trim() == "v1");
        }
        if !supported {
            return Ok(StatusCode::HTTP_VERSION_NOT_SUPPORTED.into_response());
        }
    }
    let path = request.uri().path();
    let installing = path == "/std/sys/pkg/install";
    let listing = path.ends_with("/list");
    let method = request.method().clone();
    let allow = if installing {
        "POST"
    } else if listing {
        "GET"
    } else {
        "GET, DELETE"
    };
    if !(installing && method == Method::POST
        || !installing && listing && method == Method::GET
        || !installing && !listing && matches!(method, Method::GET | Method::DELETE))
    {
        return Ok((StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, allow)]).into_response());
    }
    if installing && request.uri().query().is_some() {
        return Err(Error::BadRequest(
            "install does not accept query parameters".into(),
        ));
    }
    let mut id = None;
    if let Some(query) = request.uri().query() {
        for item in query.split('&') {
            let (key, value) = item
                .split_once('=')
                .ok_or_else(|| Error::BadRequest("expected name query parameter".into()))?;
            if key != "name" || id.is_some() || listing {
                return Err(Error::BadRequest("unsupported query parameters".into()));
            }
            let name = percent_encoding::percent_decode_str(value)
                .decode_utf8()
                .map_err(|_| Error::BadRequest("invalid package name encoding".into()))?;
            id = Some(name.into_owned());
        }
    }
    if let Some(id) = &id {
        valid_id(id)?;
        if id.contains('/') {
            return Err(Error::BadRequest(
                "name must be a local package short name".into(),
            ));
        }
    }
    if !installing && !listing && id.is_none() {
        return Err(Error::BadRequest("missing name query parameter".into()));
    }
    let expected_id = request.headers().get("expected-package-id").cloned();
    if expected_id.is_some() && (method != Method::DELETE || id.is_none()) {
        return Err(Error::BadRequest(
            "Expected-Package-Id is supported only for package removal".into(),
        ));
    }
    if installing
        && !request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        return Ok(StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response());
    }
    if request.headers().contains_key(header::TRAILER) {
        return Err(Error::BadRequest(
            "Lib management trailers are unsupported".into(),
        ));
    }
    let bytes = match tokio::time::timeout(
        Duration::from_secs(if installing { 15 } else { 120 }),
        axum::body::to_bytes(request.into_body(), if installing { 4096 } else { 0 }),
    )
    .await
    .map_err(|_| Error::Deadline)?
    {
        Ok(bytes) => bytes,
        Err(error) => {
            use std::error::Error as _;
            if error
                .source()
                .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
            {
                return Ok(if installing {
                    StatusCode::PAYLOAD_TOO_LARGE
                } else {
                    StatusCode::BAD_REQUEST
                }
                .into_response());
            }
            return Err(Error::BadRequest(
                "failed to read management request body".into(),
            ));
        }
    };
    if installing {
        let input: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| Error::BadRequest("invalid store installation JSON".into()))?;
        if input.as_object().is_none_or(|value| {
            value.len() != 3
                || value
                    .keys()
                    .any(|key| !matches!(key.as_str(), "author" | "name" | "version"))
        }) {
            return Err(Error::BadRequest(
                "installation requires only author, name and version".into(),
            ));
        }
        let text = |key: &str| {
            input
                .get(key)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| Error::BadRequest(format!("missing {key}")))
        };
        let name = text("name")?;
        valid_id(name)?;
        if name.contains('/') {
            return Err(Error::BadRequest(
                "name must be a package short name".into(),
            ));
        }
        let mut value = install_store_package(
            &profile,
            &endpoint,
            text("author")?,
            name,
            text("version")?,
            &sandbox,
        )
        .await?;
        if let Some(author) = value
            .as_object_mut()
            .and_then(|value| value.remove("publisher"))
        {
            value["author"] = author;
        }
        if let Some(dependencies) = value["dependencies"].as_array_mut() {
            for dependency in dependencies {
                if let Some(author) = dependency
                    .as_object_mut()
                    .and_then(|value| value.remove("publisher"))
                {
                    dependency["author"] = author;
                }
            }
        }
        return Ok(axum::Json(value).into_response());
    }
    if method == Method::GET {
        let detail = id.is_some();
        let mut value =
            tokio::task::spawn_blocking(move || installed_catalog(&profile, id.as_deref()))
                .await??;
        if detail {
            let loaded = sandbox
                .read()
                .unwrap()
                .libs
                .get(value["id"].as_str().unwrap())
                .cloned();
            if let Some(lib) = loaded
                && value["version"].as_str() == Some(lib.openapi.info.version.as_str())
            {
                let id = value["id"].as_str().unwrap();
                let prefix = if id.starts_with("std.dhttp.net/") {
                    "/std".to_owned()
                } else {
                    format!("/{}", id.rsplit('/').next().unwrap())
                };
                let mut endpoints = Vec::new();
                if let Some(paths) = &lib.openapi.paths {
                    for (path, item) in paths {
                        for (method, operation) in item.methods() {
                            endpoints.push(serde_json::json!({
                                "method": method.as_str(), "path": format!("{prefix}{path}"),
                                "description": operation.summary.as_deref().or(operation.description.as_deref()),
                            }));
                        }
                    }
                }
                endpoints.sort_by(|a, b| {
                    a["path"]
                        .as_str()
                        .cmp(&b["path"].as_str())
                        .then_with(|| a["method"].as_str().cmp(&b["method"].as_str()))
                });
                value["openapi"] = serde_json::to_value(&lib.openapi)
                    .map_err(|error| Error::InvalidComponent(error.to_string()))?;
                value["endpoints"] = endpoints.into();
            }
        }
        let package_id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(http::HeaderValue::from_str)
            .transpose()
            .map_err(|_| Error::InvalidConfig("invalid installed package ID".into()))?;
        let mut response = axum::Json(value).into_response();
        if let Some(id) = package_id {
            response.headers_mut().insert("package-id", id);
        }
        return Ok(response);
    }
    tokio::task::spawn_blocking(move || -> Result<()> {
        use rusqlite::OptionalExtension;
        let name = id.as_deref().unwrap();
        let path = profile.join(".volume/sys/setting/database");
        if !path.symlink_metadata()?.file_type().is_file() {
            return Err(Error::InvalidConfig(
                "config.db must be a regular file".into(),
            ));
        }
        let conn = crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(Duration::from_secs(3))?;
        let registered: Option<String> = conn.query_row("SELECT CASE WHEN author IS NULL THEN name ELSE author||'/'||name END FROM package WHERE name=?1",[name],|r|r.get(0)).optional()?;
        if let Some(expected) = &expected_id
            && registered.as_deref().map(str::as_bytes) != Some(expected.as_bytes())
        {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "installed package identity changed; query the catalog before removing",
            )));
        }
        let mut sandbox = sandbox.write().unwrap();
        if sandbox.tasks.is_closed() {
            return Err(Error::Denied);
        }
        let Some(registered) = registered else {
            return Ok(());
        };
        valid_id(&registered)?;
        if registered.rsplit('/').next() != Some(name) {
            return Err(Error::InvalidConfig(
                "invalid installed package name".into(),
            ));
        }
        remove_package(&profile, &registered)?;
        sandbox.apps.remove(&registered);
        sandbox.libs.remove(&registered);
        Ok(())
    })
    .await??;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
#[path = "../tests/unit/package.rs"]
pub(crate) mod tests;

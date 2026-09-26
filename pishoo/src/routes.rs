use std::{collections::BTreeMap, sync::Arc};

use access_control::{AccessService, Action, AuthResult, Headers, SubjectId, Visitor};
use axum::{
    Router,
    body::Body as AxumBody,
    response::{IntoResponse, Response},
    routing::any,
};
use http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use tokio_util::{io::ReaderStream, task::TaskTracker};

use crate::{
    Body, Error, Result,
    setup::{ProxyLocation, ServerConfig},
    wasm::{Invocation, Lib},
};

include!("routes/router.rs");
include!("routes/authorization.rs");
include!("routes/management.rs");
include!("routes/static_file.rs");
include!("routes/proxy.rs");

fn reserved(path: &str) -> bool {
    [
        "/contact",
        "/contacts",
        "/acl",
        "/workspace",
        "/workspace-api",
        "/.pishoo",
        "/shell",
    ]
    .iter()
    .any(|p| path == *p || path.strip_prefix(p).is_some_and(|r| r.starts_with('/')))
}
fn reject(error: Error) -> Response {
    let status = error.status();
    if status.is_server_error() {
        eprintln!("request failed: {error}");
    }
    (
        status,
        status.canonical_reason().unwrap_or("request failed"),
    )
        .into_response()
}

#[cfg(test)]
#[path = "../tests/unit/routes/mod.rs"]
mod tests;

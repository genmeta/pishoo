use std::path::Path;

use bytes::Bytes;
use http::Request;
use http_body::Frame;
use http_body_util::{BodyExt, StreamBody};
use wasmtime::StoreLimits;

use super::{host::identity, *};
use crate::Body;

#[path = "sandbox/api.rs"]
mod api;
#[path = "sandbox/deployment.rs"]
mod deployment;
#[path = "sandbox/execution.rs"]
mod execution;
#[path = "sandbox/manifest.rs"]
mod manifest;
#[path = "sandbox/resources.rs"]
mod resources;

#[path = "sandbox/management.rs"]
mod management;

#[path = "sandbox/packages.rs"]
mod packages;
pub(crate) use packages::{package_component, package_fixture, publisher};

// Independent fixture routers own a collection initialized from each test's
// supplied resources. Live management tests share one Sandbox between routers.
fn api_router(sandbox: &Sandbox, endpoint: dhttp::Endpoint) -> axum::Router {
    Sandbox::api_router(
        Arc::new(RwLock::new(Sandbox {
            libs: sandbox.libs.clone(),
            runtime: sandbox.runtime.clone(),
            tasks: sandbox.tasks.clone(),
        })),
        endpoint,
    )
}

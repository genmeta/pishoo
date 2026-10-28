use std::path::Path;

use bytes::Bytes;
use http::Request;
use http_body::Frame;
use http_body_util::{BodyExt, StreamBody};
use wasmtime::StoreLimits;

use super::{host::identity, *};
use crate::{
    Body,
    package::{
        install_lib, installed_libs, read_package, remove_lib,
        tests::{package_component, runtime_fixture},
        validate_lib,
    },
};

mod api;
mod deployment;
mod execution;
mod resources;
mod store_network;

// Independent fixture routers own a collection initialized from each test's
// supplied resources. Live management tests share one Sandbox between routers.
pub(crate) fn api_router(sandbox: &Sandbox, endpoint: dhttp::Endpoint) -> axum::Router {
    Sandbox::api_router(
        Arc::new(RwLock::new(Sandbox {
            libs: sandbox.libs.clone(),
            apps: sandbox.apps.clone(),
            runtime: sandbox.runtime.clone(),
            tasks: sandbox.tasks.clone(),
        })),
        endpoint.clone(),
        Arc::new(access_control::AccessService::new(
            sea_orm::DatabaseConnection::default(),
            endpoint.name(),
        )),
        axum::Router::new(),
    )
}

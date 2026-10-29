use std::path::Path;

use bytes::Bytes;
use http::Request;
use http_body_util::BodyExt;

pub(crate) use super::format::runtime_fixture;
use super::*;
use crate::{WasmRuntime, sandbox::tests::api_router};

#[path = "tests/management.rs"]
mod management;
#[path = "tests/manifest.rs"]
mod manifest;
#[path = "tests/packages.rs"]
pub(crate) mod packages;
pub(crate) use packages::{package_component, package_fixture, publisher};

#[path = "tests/pkg.rs"]
mod pkg;
#[path = "tests/updates.rs"]
mod updates;

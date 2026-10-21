use std::path::Path;

use bytes::Bytes;
use http::Request;
use http_body_util::BodyExt;

use super::*;
use crate::sandbox::tests::api_router;

#[path = "package/management.rs"]
mod management;
#[path = "package/manifest.rs"]
mod manifest;
#[path = "package/packages.rs"]
pub(crate) mod packages;
pub(crate) use packages::{package_component, package_fixture, publisher};

#[path = "package/pkg.rs"]
mod pkg;

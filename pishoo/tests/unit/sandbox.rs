use std::path::Path;

use bytes::Bytes;
use http::Request;
use http_body::Frame;
use http_body_util::{BodyExt, StreamBody};
use tokio::sync::oneshot;
use wasmtime::{ResourceLimiter, Store, StoreLimitsBuilder};
use wasmtime_wasi_http::p2::{WasiHttpHooks, types::OutgoingRequestConfig};

use super::{
    host::{identity, outgoing_allowed},
    *,
};
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

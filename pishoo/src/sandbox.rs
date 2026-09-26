//! Shared admission and cancellation for all WASM executions of one identity.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use http::Request;
use tokio_util::sync::CancellationToken;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, filesystem::WasiFilesystemCtx};

use crate::{
    outgoing::{AuthorizedOutgoing, OutgoingContext},
    wasm::{Error, Invocation, Limits, WasmLib},
};

#[derive(Clone, Copy, Debug)]
pub struct SandboxLimits {
    pub concurrent_requests: usize,
    /// Sum of reserved linear-memory ceilings of admitted requests.
    pub memory_bytes: usize,
    /// Sum of fuel allowances of admitted requests, not a lifetime CPU quota.
    pub in_flight_fuel: u64,
    pub request_deadline: Duration,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            concurrent_requests: 16,
            memory_bytes: 256 * 1024 * 1024,
            in_flight_fuel: 400_000_000,
            request_deadline: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub requests: usize,
    pub memory_bytes: usize,
    pub fuel: u64,
}

/// Open approved direct child directories, never the identity root or ssl.
/// Symlink children are not granted. Handles remain valid across path replacement.
pub(crate) fn read_identity_filesystem(
    profile: &dhttp_home::identity::IdentityProfile,
) -> std::io::Result<WasiFilesystemCtx> {
    let mut builder = WasiCtx::builder();
    for entry in std::fs::read_dir(profile.path())? {
        let entry = entry?;
        if entry.file_name().eq_ignore_ascii_case("ssl") || !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| std::io::Error::other("non-UTF8 directory name"))?;
        builder
            .preopened_dir(
                entry.path(),
                format!("/{name}"),
                DirPerms::READ,
                FilePerms::READ,
            )
            .map_err(std::io::Error::other)?;
    }
    let mut wasi = builder.build();
    Ok(wasi.filesystem().clone())
}

/// One local identity. Clones share registrations, file capabilities, usage and cancellation.
#[derive(Clone)]
pub struct Sandbox {
    identity: Arc<str>,
    libs: Arc<Mutex<HashMap<String, Arc<WasmLib>>>>,
    filesystem: Arc<Mutex<WasiFilesystemCtx>>,
    limits: SandboxLimits,
    usage: Arc<Mutex<Usage>>,
    cancel: CancellationToken,
}

impl Sandbox {
    pub fn new(identity: impl Into<String>, limits: SandboxLimits) -> Result<Self, Error> {
        let identity = identity.into();
        if identity.is_empty()
            || limits.concurrent_requests == 0
            || limits.memory_bytes == 0
            || limits.in_flight_fuel == 0
            || limits.request_deadline.is_zero()
        {
            return Err(Error::InvalidLimits);
        }
        Ok(Self {
            identity: Arc::from(identity),
            libs: Arc::new(Mutex::new(HashMap::new())),
            filesystem: Arc::new(Mutex::new(WasiFilesystemCtx::default())),
            limits,
            usage: Arc::new(Mutex::new(Usage::default())),
            cancel: CancellationToken::new(),
        })
    }

    /// Replace the grants used by future invocations. Existing invocations keep
    /// their opened handles; cancel the sandbox to revoke running work.
    pub fn set_filesystem(&self, filesystem: WasiFilesystemCtx) {
        *self.filesystem.lock().unwrap() = filesystem;
    }

    pub fn load_lib(&self, id: &str, bytes: &[u8]) -> Result<(), Error> {
        self.insert_lib(WasmLib::new(id, bytes)?)
    }

    pub(crate) fn insert_lib(&self, lib: WasmLib) -> Result<(), Error> {
        let mut libs = self.libs.lock().unwrap();
        if self.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        libs.insert(lib.id.clone(), Arc::new(lib));
        Ok(())
    }

    /// Load this identity's deployed libraries, retaining old versions of invalid candidates.
    pub fn load_deployments(
        &self,
        profile: &dhttp_home::identity::IdentityProfile,
    ) -> crate::setup::Result<()> {
        if profile.name() != self.identity() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "sandbox identity does not match deployment",
            )
            .into());
        }
        let filesystem = read_identity_filesystem(profile)?;
        let libs = crate::setup::discover_libs(profile)?;
        self.set_filesystem(filesystem);
        for lib in libs {
            self.insert_lib(lib)?;
        }
        Ok(())
    }

    pub fn libs(&self) -> Vec<Arc<WasmLib>> {
        self.libs.lock().unwrap().values().cloned().collect()
    }

    pub fn remove_lib(&self, id: &str) -> bool {
        self.libs.lock().unwrap().remove(id).is_some()
    }

    fn prepare(
        &self,
        request: &mut Request<h3x::ArcWndBuf>,
        limits: Limits,
        qpack: h3x::ArcQpack,
    ) -> Result<(Invocation, Arc<Reservation>), Error> {
        let limits = limits.validate()?;
        let (id, path) = request
            .uri()
            .path()
            .strip_prefix("/api/")
            .and_then(|s| s.split_once('/'))
            .ok_or(Error::RouteNotFound)?;
        let path = format!("/{path}");
        let lib = self
            .libs
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or(Error::RouteNotFound)?;
        let item = lib
            .openapi
            .paths
            .as_ref()
            .and_then(|paths| paths.get(&path))
            .ok_or(Error::RouteNotFound)?;
        if !item
            .methods()
            .into_iter()
            .any(|(method, _)| method == *request.method())
        {
            return Err(Error::MethodNotAllowed);
        }
        let path_and_query = match request.uri().query() {
            Some(query) => format!("{path}?{query}"),
            None => path,
        };
        let mut parts = request.uri().clone().into_parts();
        parts.path_and_query = Some(path_and_query.parse().map_err(|_| Error::RouteNotFound)?);
        *request.uri_mut() = http::Uri::from_parts(parts).map_err(|_| Error::RouteNotFound)?;
        let reservation = self.admit(limits)?;
        let context = request.extensions_mut().remove::<OutgoingContext>();
        let remote = context
            .as_ref()
            .map(|c| Arc::from(c.caller_identity()))
            .unwrap_or_else(|| Arc::from(""));
        let outgoing =
            AuthorizedOutgoing::for_request(&self.identity, context, reservation.cancel.clone());
        let invocation = Invocation::new(
            lib,
            self.identity.clone(),
            remote,
            self.filesystem.lock().unwrap().clone(),
            outgoing,
            limits,
            qpack,
        )?;
        Ok((invocation, reservation))
    }

    /// Route and execute an HTTP/3 request using its connection's QPACK context.
    pub async fn handle_stream<W>(
        &self,
        mut ws: h3x::H3WriteStream<W>,
        request: h3x::Request<h3x::R>,
        qpack: h3x::ArcQpack,
        limits: Limits,
    ) -> Result<(), Error>
    where
        W: tokio::io::AsyncWrite
            + qrecovery::send::CancelStream
            + h3x::TransportError
            + Unpin
            + Send
            + 'static,
    {
        let (parts, incoming) = request.into_parts();
        let mut request = Request::from_parts(parts, incoming);
        let prepared = self.prepare(&mut request, limits, qpack);
        let (invocation, reservation) = match prepared {
            Ok(value) => value,
            Err(error) => {
                qrecovery::recv::StopSending::stop(
                    request.body_mut(),
                    h3x::ErrorCode::RequestCancelled.as_u64(),
                );
                qrecovery::send::CancelStream::cancel(
                    &mut ws,
                    h3x::ErrorCode::RequestCancelled.as_u64(),
                );
                return Err(error);
            }
        };
        let (parts, body) = request.into_parts();
        invocation
            .handle_admitted(ws, h3x::Request::from_parts(parts, body), Some(reservation))
            .await
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }
    pub fn usage(&self) -> Usage {
        *self.usage.lock().unwrap()
    }
    /// Permanently close admission and cancel every invocation of this identity.
    pub fn cancel(&self) {
        let _usage = self.usage.lock().unwrap();
        self.cancel.cancel();
    }
    pub(crate) fn admit(&self, limits: Limits) -> Result<Arc<Reservation>, Error> {
        let mut usage = self.usage.lock().unwrap();
        if self.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if limits.deadline > self.limits.request_deadline
            || usage.requests >= self.limits.concurrent_requests
            || limits.memory_bytes > self.limits.memory_bytes - usage.memory_bytes
            || limits.fuel > self.limits.in_flight_fuel - usage.fuel
        {
            return Err(Error::SandboxCapacity);
        }
        usage.requests += 1;
        usage.memory_bytes += limits.memory_bytes;
        usage.fuel += limits.fuel;
        Ok(Arc::new(Reservation {
            usage: self.usage.clone(),
            limits,
            cancel: self.cancel.child_token(),
        }))
    }
}

pub(crate) struct Reservation {
    usage: Arc<Mutex<Usage>>,
    limits: Limits,
    pub cancel: CancellationToken,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut usage = self.usage.lock().unwrap();
        usage.requests -= 1;
        usage.memory_bytes -= self.limits.memory_bytes;
        usage.fuel -= self.limits.fuel;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loaded_libraries_do_not_keep_their_sandbox_alive() {
        let sandbox = Sandbox::new("alice", Default::default()).unwrap();
        let document = br#"{"openapi":"3.1.0","info":{"title":"Test","version":"1"},"paths":{}}"#;
        let mut bytes =
            include_bytes!("../tests/fixtures/wasi-http-read-request-then-respond.wasm").to_vec();
        let name = b"pishoo:openapi";
        bytes.extend([0, (1 + name.len() + document.len()) as u8, name.len() as u8]);
        bytes.extend(name);
        bytes.extend(document);
        sandbox.load_lib("test", &bytes).unwrap();
        let weak = Arc::downgrade(&sandbox.usage);
        let libs = sandbox.libs();
        drop(sandbox);
        assert!(weak.upgrade().is_none());
        assert_eq!(libs.len(), 1);
    }
}

//! Component loading, Store isolation, invocation execution, and response lifecycle.

use std::{path::Path, sync::Arc};

use http::{Request, Response};
use http_body_util::BodyExt;
use tokio::sync::oneshot;
use tokio_util::task::TaskTracker;
use wasmtime::{
    Config, Engine, Store, StoreLimits,
    component::{Component, Linker, ResourceTable},
};
use wasmtime_wasi::{
    WasiCtx, WasiCtxView, WasiView,
    cli::{WasiCli, WasiCliView},
    filesystem::WasiFilesystemView,
};
use wasmtime_wasi_http::{
    WasiHttpCtx,
    p2::{
        WasiHttpCtxView, WasiHttpView,
        bindings::{Proxy, ProxyPre, http::types},
    },
};

use super::{DenyOutgoing, Invocation, Lib, StoreData, WasmRuntime, host::identity, validate_lib};
use crate::{Body, Error, Result};

// Component compilation, per-version filesystem grants, and Store limits.

impl WasmRuntime {
    pub(crate) fn new() -> Result<Self> {
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .consume_fuel(true)
            .wasm_threads(false);
        let engine = Engine::new(&config).map_err(Error::Guest)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi_http::p2::add_to_linker_async(&mut linker).map_err(Error::Guest)?;
        // The HTTP proxy world omits filesystem interfaces, but Libs have a
        // private WASM /db grant. Register only those missing interfaces.
        wasmtime_wasi::p2::bindings::filesystem::types::add_to_linker::<
            StoreData,
            wasmtime_wasi::filesystem::WasiFilesystem,
        >(&mut linker, StoreData::filesystem)
        .map_err(Error::Guest)?;
        // Rust's WASI libc imports CLI metadata and terminal resource types.
        // WasiCtx retains its empty environment and non-terminal default I/O.
        use wasmtime_wasi::p2::bindings::cli;
        cli::environment::add_to_linker::<StoreData, WasiCli>(&mut linker, StoreData::cli)
            .map_err(Error::Guest)?;
        cli::exit::add_to_linker::<StoreData, WasiCli>(&mut linker, StoreData::cli)
            .map_err(Error::Guest)?;
        cli::terminal_input::add_to_linker::<StoreData, WasiCli>(&mut linker, StoreData::cli)
            .map_err(Error::Guest)?;
        cli::terminal_output::add_to_linker::<StoreData, WasiCli>(&mut linker, StoreData::cli)
            .map_err(Error::Guest)?;
        cli::terminal_stdin::add_to_linker::<StoreData, WasiCli>(&mut linker, StoreData::cli)
            .map_err(Error::Guest)?;
        cli::terminal_stdout::add_to_linker::<StoreData, WasiCli>(&mut linker, StoreData::cli)
            .map_err(Error::Guest)?;
        cli::terminal_stderr::add_to_linker::<StoreData, WasiCli>(&mut linker, StoreData::cli)
            .map_err(Error::Guest)?;
        wasmtime_wasi::p2::bindings::filesystem::preopens::add_to_linker::<
            StoreData,
            wasmtime_wasi::filesystem::WasiFilesystem,
        >(&mut linker, StoreData::filesystem)
        .map_err(Error::Guest)?;
        identity::IdentityHost::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
            &mut linker,
            |state| state,
        )
        .map_err(Error::Guest)?;
        Ok(Self { engine, linker })
    }

    pub(crate) fn compile(&self, bytes: &[u8]) -> Result<Component> {
        let component = Component::from_binary(&self.engine, bytes)
            .map_err(|error| Error::InvalidComponent(error.to_string()))?;
        let pre = self
            .linker
            .instantiate_pre(&component)
            .map_err(|error| Error::InvalidComponent(error.to_string()))?;
        ProxyPre::new(pre).map_err(|error| Error::InvalidComponent(error.to_string()))?;
        Ok(component)
    }
}

impl Lib {
    pub(crate) fn load(
        runtime: Arc<WasmRuntime>,
        id: String,
        bytes: &[u8],
        data_dir: &Path,
    ) -> Result<Self> {
        if id.len() > 63
            || !id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(Error::InvalidComponent("invalid Lib id".into()));
        }
        let openapi = validate_lib(bytes)?;
        let component = runtime.compile(bytes)?;
        match std::fs::symlink_metadata(data_dir) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(Error::InvalidComponent(
                    "Lib data must be a real directory".into(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(data_dir).map_err(Error::Io)?;
            }
            Err(error) => return Err(Error::Io(error)),
        }
        let parent = data_dir
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = std::fs::canonicalize(parent).map_err(Error::Io)?;
        let data = std::fs::canonicalize(data_dir).map_err(Error::Io)?;
        if data.parent() != Some(parent.as_path())
            || std::fs::symlink_metadata(data_dir)
                .map_err(Error::Io)?
                .file_type()
                .is_symlink()
        {
            return Err(Error::InvalidComponent(
                "Lib data resolves outside its directory".into(),
            ));
        }
        let mut builder = WasiCtx::builder();
        builder
            .preopened_dir(
                data,
                "/db",
                wasmtime_wasi::DirPerms::all(),
                wasmtime_wasi::FilePerms::all(),
            )
            .map_err(Error::Guest)?;
        let filesystem = builder.build().filesystem().clone();
        Ok(Self {
            openapi,
            component,
            runtime,
            filesystem,
        })
    }
}

impl WasiView for StoreData {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for StoreData {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.deny_outgoing,
        }
    }
}

// One invocation owns its Store and tracked guest task.

impl Invocation {
    pub(crate) fn new(
        lib: Arc<Lib>,
        endpoint: dhttp::Endpoint,
        handshake: &dhttp::HandshakeSummary,
        tasks: TaskTracker,
    ) -> Result<Self> {
        let local = handshake.local.as_ref().ok_or(Error::MissingHandshake)?;
        if local.name() != endpoint.name() {
            return Err(Error::IdentityMismatch);
        }
        Ok(Self {
            lib,
            local: local.clone(),
            remote: handshake.remote.clone(),
            tasks,
        })
    }

    pub(crate) async fn execute(self, request: Request<Body>) -> Result<Response<Body>> {
        let Self {
            lib,
            local,
            remote,
            tasks,
        } = self;

        // Each invocation owns its WASI state.
        let mut wasi = WasiCtx::builder().build();
        *wasi.filesystem() = lib.filesystem.clone();
        let store_data = StoreData {
            table: ResourceTable::new(),
            wasi,
            http: WasiHttpCtx::new(),
            // Per invocation Store: Wasmtime defaults allow 10,000 instances,
            // memories, and tables, with no extra per-memory byte or per-table
            // element ceiling. Use StoreLimitsBuilder for explicit ceilings.
            memory: StoreLimits::default(),
            deny_outgoing: DenyOutgoing,
            local,
            remote,
        };

        // Keep execution tracked after execute returns or is dropped.
        let (response_tx, response_rx) = oneshot::channel();
        let execution_task = tasks.spawn(async move {
            let mut store = Store::new(&lib.runtime.engine, store_data);
            store.limiter(|state| &mut state.memory);
            store.set_fuel(100_000_000).map_err(Error::Guest)?;
            store
                .fuel_async_yield_interval(Some(10_000))
                .map_err(Error::Guest)?;
            let proxy = Proxy::instantiate_async(&mut store, &lib.component, &lib.runtime.linker)
                .await
                .map_err(Error::Guest)?;
            let scheme = match request
                .uri()
                .scheme_str()
                .expect("dhttp requests have a :scheme")
            {
                "http" => types::Scheme::Http,
                "https" => types::Scheme::Https,
                other => types::Scheme::Other(other.to_owned()),
            };
            let request = request.map(|body| {
                body.map_err(|error| types::ErrorCode::InternalError(Some(error.to_string())))
            });
            let incoming = store
                .data_mut()
                .http()
                .new_incoming_request(scheme, request)
                .map_err(Error::Guest)?;
            let outparam = store
                .data_mut()
                .http()
                .new_response_outparam(response_tx)
                .map_err(Error::Guest)?;
            proxy
                .wasi_http_incoming_handler()
                .call_handle(&mut store, incoming, outparam)
                .await
                .map_err(Error::Guest)
        });

        // The task owns the sender, directly or through its Store. Exiting
        // without a response drops it and wakes the receiver.
        let response = match response_rx.await {
            Ok(response) => response,
            Err(_) => {
                execution_task.await.map_err(Error::Task)??;
                return Err(Error::GuestExitedWithoutResponse);
            }
        }
        .map_err(Error::GuestRejectedResponse)?;

        // Dropping the JoinHandle detaches the guest; TaskTracker still owns
        // its lifecycle. The existing WASI body supplies frames and errors.
        Ok(response.map(|body| {
            body.map_err(|error| Error::GuestRejectedResponse(error).body_error())
                .boxed_unsync()
        }))
    }
}

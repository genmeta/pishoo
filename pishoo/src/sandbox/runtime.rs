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
    random::WasiRandomView,
};
use wasmtime_wasi_http::{
    WasiHttpCtx,
    p2::{
        WasiHttpCtxView, WasiHttpView,
        bindings::{Proxy, ProxyPre, http::types},
    },
};

use super::{
    Invocation, Lib, Outgoing, Sandbox, StoreData, WasmRuntime,
    host::{identity, identity_v2},
};
use crate::{
    Body, Error, Result,
    package::{package_file, valid_id},
};

// Component compilation, per-version filesystem grants, and Store limits.

impl WasmRuntime {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .consume_fuel(true)
            .wasm_threads(false);
        // Installation checks and Lib::load compile the same backend. Reuse
        // Wasmtime's version/configuration/content-keyed machine code across
        // those calls, identities, and process restarts. Package validation is
        // performed by installation; loading only prepares executable resources.
        match wasmtime::Cache::from_file(None) {
            Ok(cache) => {
                config.cache(Some(cache));
            }
            Err(error) => {
                tracing::warn!(%error, "WASM compilation cache unavailable; compiling without cache");
            }
        }
        let engine = Engine::new(&config).map_err(Error::Guest)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi_http::p2::add_to_linker_async(&mut linker).map_err(Error::Guest)?;
        // Rust's HashMap seed imports are outside the HTTP proxy world.
        // Use the invocation's existing WasiCtx random resources.
        wasmtime_wasi::p2::bindings::random::insecure_seed::add_to_linker::<
            StoreData,
            wasmtime_wasi::random::WasiRandom,
        >(&mut linker, StoreData::random)
        .map_err(Error::Guest)?;
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
        identity_v2::IdentityHost::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
            &mut linker,
            |state| state,
        )
        .map_err(Error::Guest)?;
        Ok(Self { engine, linker })
    }

    pub fn compile(&self, bytes: &[u8]) -> Result<Component> {
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
        valid_id(&id).map_err(|_| Error::InvalidComponent("invalid Lib id".into()))?;
        let package =
            webc::Container::from_bytes_and_version(bytes.to_vec().into(), webc::Version::V3)
                .map_err(|error| Error::InvalidComponent(error.to_string()))?;
        let openapi = serde_json::from_slice(&package_file(&package, "/metadata/openapi.json")?)
            .map_err(|error| Error::InvalidComponent(error.to_string()))?;
        let parent = data_dir
            .parent()
            .ok_or_else(|| Error::InvalidComponent("Lib data has no database directory".into()))?;
        // New callers use db/<name>. Existing author directories remain readable
        // so changing the layout never silently replaces a user's stored data.
        let database_root = if id
            .split_once('/')
            .is_some_and(|(author, _)| parent.file_name().is_some_and(|name| name == author))
        {
            parent.parent().ok_or_else(|| {
                Error::InvalidComponent("Lib data has no database directory".into())
            })?
        } else {
            parent
        };
        let legacy_data = id
            .split_once('/')
            .map(|(author, name)| database_root.join(author).join(name));
        let data_dir = if parent == database_root
            && matches!(data_dir.symlink_metadata(), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            match legacy_data.as_deref() {
                Some(legacy) => match legacy.symlink_metadata() {
                    Ok(_) => legacy,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => data_dir,
                    Err(error) => return Err(Error::Io(error)),
                },
                None => data_dir,
            }
        } else {
            data_dir
        };
        for directory in [data_dir.parent().unwrap(), data_dir] {
            match directory.symlink_metadata() {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    return Err(Error::InvalidComponent(
                        "Lib data must be a real directory".into(),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Error::Io(error)),
            }
        }
        let component = runtime.compile(&package.get_atom("backend").ok_or_else(|| {
            Error::InvalidComponent("expected a backend lib, received a frontend app".into())
        })?)?;
        let owner = database_root.symlink_metadata().map_err(Error::Io)?;
        if !owner.is_dir() {
            return Err(Error::InvalidComponent(
                "Lib database root must be a real directory".into(),
            ));
        }
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
        if rustix::process::geteuid().is_root() {
            use std::os::unix::fs::MetadataExt;
            for directory in [data_dir.parent().unwrap(), data_dir] {
                let file = rustix::fs::open(
                    directory,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )
                .map_err(std::io::Error::from)?;
                let permissions = rustix::fs::fstat(&file)
                    .map_err(std::io::Error::from)?
                    .st_mode;
                rustix::fs::fchown(
                    &file,
                    Some(rustix::fs::Uid::from_raw(owner.uid())),
                    Some(rustix::fs::Gid::from_raw(owner.gid())),
                )
                .map_err(std::io::Error::from)?;
                rustix::fs::fchmod(&file, rustix::fs::Mode::from_raw_mode(permissions as _))
                    .map_err(std::io::Error::from)?;
            }
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
            package,
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
            hooks: &mut self.outgoing,
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
        mut outgoing: Outgoing,
    ) -> Result<Self> {
        let local = handshake.local.as_ref().ok_or(Error::MissingHandshake)?;
        if local.name() != endpoint.name() {
            return Err(Error::IdentityMismatch);
        }
        if let Outgoing::Granted {
            endpoint: granted,
            api,
            ..
        } = &mut outgoing
        {
            check_endpoint(&endpoint, granted)?;
            let actual = endpoint.local_authority()?;
            let actual = dhttp_home::certificate::extract_dhttp_subject_key_identifier(
                actual.certificates(),
            )
            .map_err(|_| Error::InvalidIdentity("invalid execution owner".into()))?;
            let invoked =
                dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
                    .map_err(|_| Error::InvalidIdentity("invalid invocation owner".into()))?;
            if actual.owner_hash() != invoked.owner_hash() {
                return Err(Error::IdentityMismatch);
            }
            let handshake = handshake.clone();
            *api = api.clone().layer(axum::middleware::from_fn(
                move |mut request: Request<axum::body::Body>, next: axum::middleware::Next| {
                    let handshake = handshake.clone();
                    async move {
                        // WASI strips guest extensions before Router matching.
                        // Keep Axum's trusted route parameters while replacing
                        // only authentication facts for this invocation.
                        request.extensions_mut().remove::<access_control::Visitor>();
                        request.extensions_mut().remove::<dhttp::LocalAuthority>();
                        request.extensions_mut().insert(handshake);
                        next.run(request).await
                    }
                },
            ));
        }
        Ok(Self {
            lib,
            local: local.clone(),
            remote: handshake.remote.clone(),
            tasks,
            outgoing,
        })
    }

    pub(crate) async fn execute(self, request: Request<Body>) -> Result<Response<Body>> {
        let Self {
            lib,
            local,
            remote,
            tasks,
            outgoing,
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
            outgoing,
            local,
            remote,
        };

        // Keep execution tracked after execute returns or is dropped.
        let (response_tx, response_rx) = oneshot::channel();
        let execution_task = tasks.spawn(async move {
            let _ownership = scopeguard::guard(lib.filesystem.clone(), |filesystem| {
                if let Err(error) = repair_guest_ownership(filesystem) {
                    tracing::error!(%error, "cannot restore Lib data ownership");
                }
            });
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

    pub(crate) async fn run(
        lib: Arc<Lib>,
        endpoint: dhttp::Endpoint,
        tasks: TaskTracker,
        mut outgoing: Outgoing,
    ) -> Result<()> {
        let local = endpoint.local_authority()?.clone();
        if let Outgoing::Granted {
            endpoint: granted,
            api,
            ..
        } = &mut outgoing
        {
            check_endpoint(&endpoint, granted)?;
            let authority = local.clone();
            *api = api.clone().layer(axum::middleware::from_fn(
                move |mut request: Request<axum::body::Body>, next: axum::middleware::Next| {
                    let authority = authority.clone();
                    async move {
                        request.extensions_mut().remove::<access_control::Visitor>();
                        request.extensions_mut().remove::<dhttp::HandshakeSummary>();
                        request.extensions_mut().insert(authority);
                        next.run(request).await
                    }
                },
            ));
        }
        let mut wasi = WasiCtx::builder().build();
        *wasi.filesystem() = lib.filesystem.clone();
        let mut task = scopeguard::guard(
            tasks.spawn(async move {
                let _ownership = scopeguard::guard(lib.filesystem.clone(), |filesystem| {
                    if let Err(error) = repair_guest_ownership(filesystem) {
                        tracing::error!(%error, "cannot restore Lib data ownership");
                    }
                });
                let mut store = Store::new(
                    &lib.runtime.engine,
                    StoreData {
                        table: ResourceTable::new(),
                        wasi,
                        http: WasiHttpCtx::new(),
                        memory: StoreLimits::default(),
                        outgoing,
                        local,
                        remote: None,
                    },
                );
                store.limiter(|state| &mut state.memory);
                store.set_fuel(100_000_000).map_err(Error::Guest)?;
                store
                    .fuel_async_yield_interval(Some(10_000))
                    .map_err(Error::Guest)?;
                let command = wasmtime_wasi::p2::bindings::Command::instantiate_async(
                    &mut store,
                    &lib.component,
                    &lib.runtime.linker,
                )
                .await
                .map_err(Error::Guest)?;
                command
                    .wasi_cli_run()
                    .call_run(&mut store)
                    .await
                    .map_err(Error::Guest)?
                    .map_err(|()| {
                        Error::Guest(wasmtime::Error::msg(
                            "background component returned an error",
                        ))
                    })
            }),
            |task| task.abort(),
        );
        match tokio::time::timeout(std::time::Duration::from_secs(15), &mut *task).await {
            Ok(result) => result.map_err(Error::Task)?,
            Err(_) => {
                task.abort();
                let _ = (&mut *task).await;
                Err(Error::Deadline)
            }
        }
    }
}

// Use the actual preopened directory, including on cancellation, rather than
// retaining a second path or identity in Lib or StoreData.
fn repair_guest_ownership(
    mut filesystem: wasmtime_wasi::filesystem::WasiFilesystemCtx,
) -> Result<()> {
    if !rustix::process::geteuid().is_root() {
        return Ok(());
    }
    use wasmtime_wasi::p2::bindings::filesystem::preopens::Host;
    let mut table = ResourceTable::new();
    let directories = wasmtime_wasi::filesystem::WasiFilesystemCtxView {
        ctx: &mut filesystem,
        table: &mut table,
    }
    .get_directories()
    .map_err(Error::Guest)?;
    for (descriptor, _) in directories {
        if let wasmtime_wasi::filesystem::Descriptor::Dir(directory) = table
            .get(&descriptor)
            .map_err(|error| Error::Guest(error.into()))?
        {
            let owner = directory.dir.try_clone()?.into_std_file().metadata()?;
            repair_guest_directory(&directory.dir, &owner)?;
        }
    }
    Ok(())
}

fn repair_guest_directory(directory: &cap_std::fs::Dir, owner: &std::fs::Metadata) -> Result<()> {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::MetadataExt;
    let mut directories = vec![directory.try_clone()?];
    while let Some(directory) = directories.pop() {
        for entry in directory.entries()? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() && !entry.file_type()?.is_file() {
                continue;
            }
            let file = std::fs::File::from(
                rustix::fs::openat(
                    &directory,
                    entry.file_name(),
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(std::io::Error::from)?,
            );
            let metadata = file.metadata()?;
            if metadata.dev() != owner.dev()
                || (!metadata.is_dir() && (!metadata.is_file() || metadata.nlink() != 1))
            {
                continue;
            }
            if metadata.uid() != owner.uid() || metadata.gid() != owner.gid() {
                rustix::fs::fchown(
                    &file,
                    Some(rustix::fs::Uid::from_raw(owner.uid())),
                    Some(rustix::fs::Gid::from_raw(owner.gid())),
                )
                .map_err(std::io::Error::from)?;
                file.set_permissions(metadata.permissions())?;
            }
            if metadata.is_dir() {
                directories.push(cap_std::fs::Dir::from_std_file(file));
            }
        }
    }
    Ok(())
}

// Endpoint ownership is validated before moving a granted capability into a Store.
fn check_endpoint(actual: &dhttp::Endpoint, granted: &dhttp::Endpoint) -> Result<()> {
    if actual.name() != granted.name() {
        return Err(Error::IdentityMismatch);
    }
    let actual = actual.local_authority()?;
    let granted = granted.local_authority()?;
    let actual =
        dhttp_home::certificate::extract_dhttp_subject_key_identifier(actual.certificates())
            .map_err(|_| Error::InvalidIdentity("invalid execution owner".into()))?;
    let granted =
        dhttp_home::certificate::extract_dhttp_subject_key_identifier(granted.certificates())
            .map_err(|_| Error::InvalidIdentity("invalid outgoing owner".into()))?;
    if actual.owner_hash() != granted.owner_hash() {
        return Err(Error::IdentityMismatch);
    }
    Ok(())
}

impl Sandbox {
    pub(crate) async fn run(
        sandbox: Arc<std::sync::RwLock<Self>>,
        endpoint: dhttp::Endpoint,
        access: Arc<access_control::AccessService>,
        api: axum::Router,
    ) {
        let (mut libs, tasks) = {
            let sandbox = sandbox.read().unwrap();
            if sandbox.tasks.is_closed() {
                return;
            }
            let libs = sandbox
                .libs
                .iter()
                .filter(|(_, lib)| {
                    lib.component
                        .component_type()
                        .exports(&lib.runtime.engine)
                        .any(|(name, _)| name.starts_with("wasi:cli/run@0.2."))
                })
                .map(|(id, lib)| (id.clone(), lib.clone()))
                .collect::<Vec<_>>();
            (libs, sandbox.tasks.clone())
        };
        libs.sort_by(|left, right| left.0.cmp(&right.0));
        futures::future::join_all(libs.into_iter().map(|(id, lib)| {
            let endpoint = endpoint.clone();
            let access = access.clone();
            let api = api.clone();
            let tasks = tasks.clone();
            async move {
                if tasks.is_closed() { return; }
                let outgoing = Outgoing::Granted { endpoint: endpoint.clone(), access, api };
                if let Err(error) = Invocation::run(lib, endpoint.clone(), tasks, outgoing).await {
                    tracing::warn!(identity=endpoint.name(),lib=%id,%error,"background WASI run failed");
                }
            }
        })).await;
    }
}

use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::{Arc, RwLock},
};

use axum::{Router, body::Body as AxumBody, response::IntoResponse, routing::any};
use dhttp_home::{DhttpHome, identity::IdentityProfile};
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use http::Request;
use http_body_util::BodyExt;
use tokio::time::Instant;
use tokio_util::task::TaskTracker;
use tower::ServiceExt;

use crate::{
    Body, Error, Result,
    chat::{self, Chat, store::ChatStore},
    dns, exec,
    routes::{DHTTP_PREFIX, access_router, authorize, file_router, forward_dhttp, proxy_pass},
    sandbox::{Sandbox, WasmRuntime},
    setup::{ServerConfig, config_router, load_server_config},
    workspace::{self, Workspace, store::WorkspaceStore},
};

struct Server {
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    config: ServerConfig,
    access: Arc<access_control::AccessService>,
    workspace: Arc<Workspace>,
    chat: Arc<Chat>,
    router: Arc<RwLock<axum::Router>>,
    sandbox: Sandbox,
    exec_tasks: TaskTracker,
    publisher: Option<Arc<ddns::H3Resolver>>,
}

pub async fn run() -> Result<()> {
    let home = DhttpHome::load(dhttp_home::HomeScope::User)
        .map_err(|e| Error::InvalidConfig(e.to_string()))?;
    #[cfg(unix)]
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let runtime = Arc::new(WasmRuntime::new()?);
    let mdns = dns::install()?;
    let addresses = qprotocol::AddressBook::global();
    let mut inner_events = addresses.subscribe_punch(dhttp::Scope::Internal);
    let mut outer = addresses.subscribe_ddns();
    let mut servers = BTreeMap::<String, Server>::new();
    let mut jobs = FuturesUnordered::<BoxFuture<'static, Option<Instant>>>::new();
    // Startup failures share the same DNS and application cleanup path as shutdown.
    let result = async {
        let loaded = async {
            for profile in home.discover_identity_profiles()? {
                let server = Server::load(profile, runtime.clone()).await?;
                servers.insert(server.name().to_owned(), server);
            }
            dhttp::DhttpNetwork::init().await?;
            Ok::<(), Error>(())
        }.await;
        if let Err(error) = loaded {
            // Nothing has registered yet: do not send withdrawals for unused publishers.
            for server in servers.values_mut() { server.publisher.take(); }
            return Err(error);
        }
        let mut unregistered = servers.values_mut().filter(|s| s.config.listen != 0);
        while let Some(server) = unregistered.next() {
            match server.listen().await {
                Ok(listener) => { tokio::spawn(listener); }
                Err(error) => {
                    server.publisher.take();
                    for server in unregistered { server.publisher.take(); }
                    return Err(error);
                }
            }
        }
        let interrupt = tokio::signal::ctrl_c();
        tokio::pin!(interrupt);
        #[cfg(unix)]
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut removed_bounds = Vec::new();
        'maintenance: loop {
            let endpoints = servers.values()
                .filter(|s| !s.exec_tasks.is_closed() && s.config.listen & 1 != 0)
                .map(|s| s.endpoint.clone()).collect::<Vec<_>>();
            let mut next_due = match dns::sync_mdns(&mdns, &endpoints, &removed_bounds).await {
                Ok(()) => None,
                Err(error) => {
                    eprintln!("DNS maintenance: {error}");
                    Some(Instant::now() + std::time::Duration::from_secs(5))
                }
            };
            removed_bounds.clear();
            let snapshot = outer.borrow_and_update().clone();
            for server in servers.values().filter(|s| !s.exec_tasks.is_closed()) {
                if let Some(publisher) = &server.publisher {
                    jobs.push(dns::publish(server.name().to_owned(), publisher.clone(), snapshot.clone()).boxed());
                }
            }
            let mut timer = None;
            loop {
                if jobs.is_empty() && timer.is_none() {
                    timer = next_due.take().map(|due| Box::pin(tokio::time::sleep_until(due)));
                }
                tokio::select! {
                    result = &mut interrupt => { result?; return Ok(()); }
                    _ = async { #[cfg(unix)] { terminate.recv().await; } #[cfg(not(unix))] { std::future::pending::<()>().await; } } => return Ok(()),
                    _ = async { #[cfg(unix)] { hangup.recv().await; } #[cfg(not(unix))] { std::future::pending::<()>().await; } } => {
                        // Each operation already has its own finite deadline; poll all remaining work.
                        while jobs.next().await.is_some() {}
                        let reload = async {
                            let profiles = home.discover_identity_profiles()?;
                            let present = profiles.iter().map(|p| p.name()).collect::<HashSet<_>>();
                            for server in servers.values_mut() {
                                if !present.contains(server.name()) && !server.exec_tasks.is_closed() {
                                    if let Err(error) = dns::withdraw(&server.endpoint, server.publisher.as_deref(), &mdns).await {
                                        eprintln!("{error}");
                                    }
                                    server.close().await?;
                                }
                            }
                            for profile in profiles {
                                if let Some(server) = servers.get_mut(profile.name()) {
                                    if server.exec_tasks.is_closed() {
                                        return Err(Error::InvalidConfig(format!(
                                            "server {} was removed; restart required", server.name())));
                                    }
                                    server.reload().await?;
                                    continue;
                                }
                                let mut server = Server::load(profile, runtime.clone()).await?;
                                if server.config.listen != 0 {
                                    match server.listen().await {
                                        Ok(listener) => { tokio::spawn(listener); }
                                        Err(error) => {
                                            if let Err(close) = server.close().await { eprintln!("new server cleanup: {close}"); }
                                            return Err(error);
                                        }
                                    }
                                }
                                servers.insert(server.name().to_owned(), server);
                            }
                            Ok::<(), Error>(())
                        }.await;
                        if let Err(error) = reload { eprintln!("reload rejected: {error}"); }
                        // Even a partially rejected reload must resume maintenance of live servers.
                        continue 'maintenance;
                    }
                    result = jobs.next(), if !jobs.is_empty() => {
                        if let Some(Some(due)) = result {
                            next_due = Some(next_due.map_or(due, |previous| previous.min(due)));
                        }
                    }
                    event = inner_events.recv(), if jobs.is_empty() => {
                        let event = event.ok_or_else(|| std::io::Error::other("DNS address subscription ended"))?;
                        if let qprotocol::AddressEvent::BoundRemoved { bound } = event { removed_bounds.push(bound); }
                        while let Ok(event) = inner_events.try_recv() {
                            if let qprotocol::AddressEvent::BoundRemoved { bound } = event { removed_bounds.push(bound); }
                        }
                        break;
                    }
                    changed = outer.changed(), if jobs.is_empty() => { changed.map_err(std::io::Error::other)?; break; }
                    _ = async { if let Some(timer) = &mut timer { timer.as_mut().await; } }, if jobs.is_empty() && timer.is_some() => { break; }
                }
            }
        }
    }.await;
    // No new batches after the run body exits, including failure during startup.
    while jobs.next().await.is_some() {}
    let mut shutdown = Ok(());
    {
        let mut withdrawals = servers
            .values()
            .filter(|s| !s.exec_tasks.is_closed())
            .map(|s| dns::withdraw(&s.endpoint, s.publisher.as_deref(), &mdns))
            .collect::<FuturesUnordered<_>>();
        while let Some(result) = withdrawals.next().await {
            if let Err(error) = result {
                eprintln!("{error}");
                shutdown = Err(Error::Io(error));
            }
        }
    }
    for server in servers.values_mut() {
        if let Err(error) = server.close().await {
            eprintln!("server {} shutdown: {error}", server.name());
            shutdown = Err(error);
        }
    }
    if let Err(error) = mdns.shutdown().await {
        eprintln!("mDNS shutdown: {error}");
        shutdown = Err(Error::Io(std::io::Error::other(error)));
    }
    result.and(shutdown)
}

#[cfg(test)]
#[path = "../tests/unit/server.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/unit/server/exec_route.rs"]
mod exec_tests;

fn exec(enabled: bool, name: String, cwd: PathBuf, tasks: TaskTracker) -> Router {
    Router::new().route(
        "/exec",
        any(move |request: Request<AxumBody>| {
            let (name, cwd, tasks) = (name.clone(), cwd.clone(), tasks.clone());
            async move {
                let request = request.map(|body| body.map_err(Into::into).boxed_unsync());
                match exec::execute(enabled, &name, &cwd, tasks, request).await {
                    Ok(response) => response.map(AxumBody::new).into_response(),
                    Err(error) => {
                        let status = error.status();
                        if status.is_server_error() {
                            eprintln!("exec request for {name}: {error}");
                        }
                        (
                            status,
                            status.canonical_reason().unwrap_or("request failed"),
                        )
                            .into_response()
                    }
                }
            }
        }),
    )
}

fn dhttp_router(endpoint: dhttp::Endpoint) -> Router {
    Router::new().route(
        &format!("{DHTTP_PREFIX}{{*path}}"),
        any(move |request: Request<AxumBody>| forward_dhttp(endpoint.clone(), request)),
    )
}

fn chat_router(chat: Arc<Chat>, workspace: Arc<Workspace>) -> Router {
    chat::router(chat).layer(axum::middleware::from_fn(
        move |request: Request<AxumBody>, next: axum::middleware::Next| {
            let workspace = workspace.clone();
            async move {
                if request.method() == http::Method::POST && request.uri().path() == "/std/message"
                {
                    let Some(visitor) = request.extensions().get::<access_control::Visitor>()
                    else {
                        return http::StatusCode::FORBIDDEN.into_response();
                    };
                    match workspace::directory::approved_chat_grant(&workspace, visitor).await {
                        Ok(true) => {}
                        Ok(false) => return http::StatusCode::FORBIDDEN.into_response(),
                        Err(error) => {
                            eprintln!("Chat capability authorization failed: {error}");
                            return http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
                        }
                    }
                }
                next.run(request).await
            }
        },
    ))
}

impl Server {
    pub(super) async fn load(profile: IdentityProfile, runtime: Arc<WasmRuntime>) -> Result<Self> {
        let certs = profile
            .load_certs()
            .await
            .map_err(|e| Error::InvalidIdentity(e.to_string()))?;
        let endpoint = dhttp::Endpoint::load(profile.name()).await?;
        let config = load_server_config(&profile)?;
        if config.listen != 0 {
            dns::authority(&endpoint)?;
        }
        let publisher = if config.listen & 2 != 0 {
            Some(dns::publisher(&endpoint)?)
        } else {
            None
        };
        let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(&certs)
            .map_err(|error| Error::InvalidIdentity(error.to_string()))?;
        let subject = access_control::SubjectId::new(ski.owner_hash().as_str().as_bytes())
            .map_err(|_| Error::InvalidIdentity("invalid certificate subject".into()))?;
        std::fs::create_dir_all(profile.db_dir())?;
        let uri = format!("sqlite://{}?mode=rwc", profile.access_db_path().display());
        let access = Arc::new(
            access_control::AccessService::load_from_db(&uri, profile.name(), &subject).await?,
        );
        let workspace_store = WorkspaceStore::open(&profile)
            .await
            .map_err(|error| Error::InvalidConfig(error.to_string()))?;
        let chat_store = ChatStore::open(&profile)
            .await
            .map_err(|error| Error::InvalidConfig(error.to_string()))?;
        let workspace = Arc::new(Workspace::new(
            profile.name().to_owned(),
            endpoint.name().to_owned(),
            subject.clone(),
            workspace_store,
            access.clone(),
        ));
        let chat = Arc::new(Chat::new(
            endpoint.name().to_owned(),
            subject,
            chat_store,
            access.clone(),
        ));
        workspace.configure_chat(chat.clone()).await;
        workspace
            .configure_outbound(Arc::new(endpoint.clone()))
            .await;
        chat.configure_outbound(Arc::new(endpoint.clone())).await;
        let exec_tasks = TaskTracker::new();
        let mut sandbox = Sandbox::new(runtime);
        sandbox.load_libs(&profile)?;
        let proxies = config.proxy_locations.clone();
        let router = Router::new()
            .merge(access_router(access.clone()))
            .merge(config_router(profile.clone(), endpoint.clone()))
            .merge(workspace::router(workspace.clone()))
            .merge(chat_router(chat.clone(), workspace.clone()))
            .merge(sandbox.api_router(endpoint.clone()))
            .merge(exec(
                config.exec,
                endpoint.name().to_owned(),
                profile.path().to_path_buf(),
                exec_tasks.clone(),
            ))
            .merge(file_router(profile.join("file")))
            .merge(dhttp_router(endpoint.clone()))
            .fallback(any(move |request: Request<AxumBody>| {
                proxy_pass(proxies.clone(), request)
            }))
            .layer(axum::middleware::from_fn_with_state(
                access.clone(),
                authorize,
            ));
        chat.start_worker();
        Ok(Self {
            profile,
            endpoint,
            config,
            access,
            workspace,
            chat,
            router: Arc::new(RwLock::new(router)),
            sandbox,
            exec_tasks,
            publisher,
        })
    }
    pub(super) fn name(&self) -> &str {
        self.endpoint.name()
    }

    pub(super) async fn reload(&mut self) -> Result<()> {
        let config = load_server_config(&self.profile)?;
        if config.listen != self.config.listen || config.exec != self.config.exec {
            return Err(Error::InvalidConfig(
                "listen and exec changes require restarting the instance".into(),
            ));
        }
        let certs = self
            .profile
            .load_certs()
            .await
            .map_err(|error| Error::InvalidIdentity(error.to_string()))?;
        if certs != self.endpoint.local_authority()?.certificates() {
            return Err(Error::InvalidConfig(
                "identity credentials changed; restart required".into(),
            ));
        }
        self.sandbox.load_libs(&self.profile)?;
        let proxies = config.proxy_locations.clone();
        let router = Router::new()
            .merge(access_router(self.access.clone()))
            .merge(config_router(self.profile.clone(), self.endpoint.clone()))
            .merge(workspace::router(self.workspace.clone()))
            .merge(chat_router(self.chat.clone(), self.workspace.clone()))
            .merge(self.sandbox.api_router(self.endpoint.clone()))
            .merge(exec(
                config.exec,
                self.endpoint.name().to_owned(),
                self.profile.path().to_path_buf(),
                self.exec_tasks.clone(),
            ))
            .merge(file_router(self.profile.join("file")))
            .merge(dhttp_router(self.endpoint.clone()))
            .fallback(any(move |request: Request<AxumBody>| {
                proxy_pass(proxies.clone(), request)
            }))
            .layer(axum::middleware::from_fn_with_state(
                self.access.clone(),
                authorize,
            ));
        *self.router.write().unwrap() = router;
        self.config = config;
        Ok(())
    }

    pub(super) async fn listen(&self) -> Result<dhttp::ListenFuture> {
        let (endpoint, router, listen) = (
            self.endpoint.clone(),
            self.router.clone(),
            self.config.listen,
        );
        let scopes = match listen {
            0 => return Err(Error::InvalidConfig("listen is disabled".into())),
            1 => dhttp::Scope::Internal.into(),
            2 => dhttp::Scope::External.into(),
            _ => dhttp::Scope::Internal | dhttp::Scope::External,
        };
        let name = endpoint.name().to_owned();
        let service = tower::service_fn(move |request: http::Request<Body>| {
            let (router, name) = (router.clone(), name.clone());
            async move {
                let method = request.method().clone();
                let path = request.uri().path().to_owned();
                let version = request.version();
                let peer = request
                    .extensions()
                    .get::<dhttp::HandshakeSummary>()
                    .and_then(|handshake| handshake.remote.as_ref())
                    .map(|authority| authority.name().to_owned());
                let result: Result<http::Response<AxumBody>> = async {
                    let app = router.read().unwrap().clone();
                    let response = app
                        .oneshot(request.map(axum::body::Body::new))
                        .await
                        .expect("Router is infallible");
                    Ok(response)
                }
                .await;
                let response = result.unwrap_or_else(|error| {
                    let status = error.status();
                    if status.is_server_error() {
                        eprintln!("request for {name}: {error}");
                    }
                    (
                        status,
                        status.canonical_reason().unwrap_or("request failed"),
                    )
                        .into_response()
                });
                eprintln!(
                    "{} request server={name} peer={} version={version:?} method={method} path={path:?} status={}",
                    chrono::Utc::now().to_rfc3339(),
                    peer.as_deref().unwrap_or("anonymous"),
                    response.status().as_u16(),
                );
                Ok::<_, std::convert::Infallible>(response)
            }
        });
        endpoint.listen(scopes, service).await.map_err(Into::into)
    }

    pub(super) async fn close(&mut self) -> Result<()> {
        self.publisher.take();
        self.sandbox.close();
        self.exec_tasks.close();
        *self.router.write().unwrap() = axum::Router::new();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            tokio::join!(self.workspace.shutdown(), self.chat.shutdown());
            let (sandbox, ()) = tokio::join!(self.sandbox.wait(), self.exec_tasks.wait());
            sandbox
        })
        .await
        .map_err(|_| Error::ShutdownDeadline)?
    }
}

#[cfg(test)]
#[path = "../tests/unit/server/workspace_network.rs"]
mod network_tests;

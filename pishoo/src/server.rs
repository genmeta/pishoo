use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::{Arc, RwLock},
};

use axum::{Router, body::Body as AxumBody, response::IntoResponse, routing::any};
use dhttp_home::{DhttpHome, identity::IdentityProfile};
use http::Request;
use http_body_util::BodyExt;
use tokio_util::task::TaskTracker;
use tower::ServiceExt;

use crate::{
    Body, Error, Result,
    chat::{self, Chat, store::ChatStore},
    exec,
    routes::{DHTTP_PREFIX, access_router, authorize, file_router, forward_dhttp, proxy_pass},
    sandbox::{Sandbox, WasmRuntime},
    setup::{ServerConfig, load_server_config},
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
}

pub async fn run() -> Result<()> {
    let home = DhttpHome::load(dhttp_home::HomeScope::User)
        .map_err(|e| Error::InvalidConfig(e.to_string()))?;
    #[cfg(unix)]
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let runtime = Arc::new(WasmRuntime::new()?);
    let mut servers = BTreeMap::new();
    for profile in home.discover_identity_profiles()? {
        let server = Server::load(profile, runtime.clone()).await?;
        servers.insert(server.name().to_owned(), server);
    }
    dhttp::DhttpNetwork::init().await?;
    for server in servers.values().filter(|s| s.config.listen != 0) {
        let (name, listener) = (server.name().to_owned(), server.listen());
        tokio::spawn(async move {
            if let Err(e) = listener.await {
                eprintln!("listener {name} ended: {e}");
            }
        });
    }
    let result = async {
        let interrupt = tokio::signal::ctrl_c();
        tokio::pin!(interrupt);
        #[cfg(unix)]
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        loop {
            tokio::select! {
                result = &mut interrupt => { result?; return Ok(()); }
                _ = async { #[cfg(unix)] { terminate.recv().await; } #[cfg(not(unix))] { std::future::pending::<()>().await; } } => return Ok(()),
                _ = async { #[cfg(unix)] { hangup.recv().await; } #[cfg(not(unix))] { std::future::pending::<()>().await; } } => {
                    let reload = async {
                        let profiles = home.discover_identity_profiles()?;
                        let present = profiles.iter().map(|p| p.name()).collect::<HashSet<_>>();
                        for server in servers.values_mut() {
                            if !present.contains(server.name()) && !server.exec_tasks.is_closed() {
                                server.close().await?;
                            }
                        }
                        for profile in profiles {
                            if let Some(server) = servers.get_mut(profile.name()) {
                                if server.exec_tasks.is_closed() {
                                    return Err(Error::InvalidConfig(format!(
                                        "server {} was removed; restart required",
                                        server.name()
                                    )));
                                }
                                server.reload().await?;
                                continue;
                            }
                            let server = Server::load(profile, runtime.clone()).await?;
                            if server.config.listen != 0 {
                                let (name, listener) = (server.name().to_owned(), server.listen());
                                tokio::spawn(async move {
                                    if let Err(e) = listener.await {
                                        eprintln!("listener {name} ended: {e}");
                                    }
                                });
                            }
                            servers.insert(server.name().to_owned(), server);
                        }
                        Ok::<(), Error>(())
                    }.await;
                    if let Err(e) = reload { eprintln!("reload rejected: {e}"); }
                },
            }
        }
    }.await;
    let mut shutdown = Ok(());
    for server in servers.values_mut() {
        if let Err(e) = server.close().await {
            shutdown = Err(e);
        }
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
        let exec_tasks = TaskTracker::new();
        let mut sandbox = Sandbox::new(runtime);
        sandbox.load_libs(&profile)?;
        let proxies = config.proxy_locations.clone();
        let router = Router::new()
            .merge(access_router(access.clone()))
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
        self.sandbox.load_libs(&self.profile)?;
        let proxies = config.proxy_locations.clone();
        let router = Router::new()
            .merge(access_router(self.access.clone()))
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

    pub(super) fn listen(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>> {
        let (endpoint, router, listen) = (
            self.endpoint.clone(),
            self.router.clone(),
            self.config.listen,
        );
        Box::pin(async move {
            let scopes = match listen {
                0 => return Ok(()),
                1 => dhttp::Scope::Internal.into(),
                2 => dhttp::Scope::External.into(),
                _ => dhttp::Scope::Internal | dhttp::Scope::External,
            };
            let name = endpoint.name().to_owned();
            let service = tower::service_fn(move |request: http::Request<Body>| {
                let (router, name) = (router.clone(), name.clone());
                async move {
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
                    Ok::<_, std::convert::Infallible>(response)
                }
            });
            endpoint.listen(scopes, service).await.map_err(Into::into)
        })
    }

    pub(super) async fn close(&mut self) -> Result<()> {
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

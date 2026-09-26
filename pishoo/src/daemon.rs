use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use axum::response::IntoResponse;
use dhttp_home::{DhttpHome, identity::IdentityProfile};
use http_body_util::BodyExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{sync::Semaphore, task::JoinSet};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tower::ServiceExt;

use crate::{
    Body, Error, Result, TerminalPolicy,
    routes::build_router,
    setup::{ServerConfig, load_server_config, network_config},
    terminal::TerminalManager,
    wasm::{Lib, LibPolicy, Runtime},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    pub state_dir: PathBuf,
    #[serde(default)]
    pub terminal: TerminalPolicy,
}
struct Daemon {
    home: DhttpHome,
    servers: BTreeMap<String, Server>,
    listeners: JoinSet<(String, Result<()>)>,
    runtime: Arc<Runtime>,
    terminal: Arc<TerminalManager>,
}
struct Server {
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    config: ServerConfig,
    access: Arc<access_control::AccessService>,
    router: Arc<RwLock<axum::Router>>,
    libs: BTreeMap<String, Arc<Lib>>,
    runtime: Arc<Runtime>,
    lib_slots: Arc<Semaphore>,
    terminal: Arc<TerminalManager>,
    tasks: TaskTracker,
    cancel: CancellationToken,
}

pub async fn run(config: DaemonConfig) -> Result<()> {
    // The open file owns the instance lock for this entire invocation.
    let expected = std::fs::canonicalize(&config.state_dir)?;
    let home = DhttpHome::load(dhttp_home::HomeScope::User)
        .map_err(|e| Error::InvalidConfig(e.to_string()))?;
    if std::fs::canonicalize(home.as_path())? != expected {
        return Err(Error::InvalidConfig(
            "state_dir and startup DHTTP_HOME must be the same directory".into(),
        ));
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(expected.join("pishoo.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .map_err(|e| Error::InvalidConfig(format!("instance is already running: {e}")))?;
    let mut daemon = Daemon::load(config).await?;
    let result = daemon.run().await;
    let shutdown = daemon.shutdown().await;
    drop(lock);
    result.and(shutdown)
}

impl Daemon {
    async fn load(config: DaemonConfig) -> Result<Self> {
        let home = DhttpHome::new(config.state_dir.clone());
        let runtime = Arc::new(Runtime::new()?);
        let terminal = Arc::new(TerminalManager::new(config.terminal, &config.state_dir)?);
        let mut servers = BTreeMap::new();
        for profile in home.discover_identity_profiles()? {
            match Server::load(profile.clone(), runtime.clone(), terminal.clone()).await {
                Ok(server) => {
                    servers.insert(server.name().to_owned(), server);
                }
                Err(e) => eprintln!("skipping server {}: {e}", profile.name()),
            }
        }
        let configs = servers
            .values()
            .map(|s| s.config.clone())
            .collect::<Vec<_>>();
        if let Some(config) = network_config(&configs)? {
            dhttp::DhttpNetwork::init(config).await?;
        }
        let mut daemon = Self {
            home,
            servers,
            listeners: JoinSet::new(),
            runtime,
            terminal,
        };
        for server in daemon.servers.values().filter(|s| s.config.listen != 0) {
            let (name, listener) = (server.name().to_owned(), server.listen());
            daemon
                .listeners
                .spawn(async move { (name, listener.await) });
        }
        Ok(daemon)
    }

    async fn run(&mut self) -> Result<()> {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        let interrupt = tokio::signal::ctrl_c();
        tokio::pin!(interrupt);
        #[cfg(unix)]
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        loop {
            tokio::select! {
                result = &mut interrupt => { result?; return Ok(()); }
                _ = async { #[cfg(unix)] { terminate.recv().await; } #[cfg(not(unix))] { std::future::pending::<()>().await; } } => return Ok(()),
                _ = interval.tick() => if let Err(e) = self.reload().await { eprintln!("reload rejected: {e}"); },
                result = self.listeners.join_next(), if !self.listeners.is_empty() => {
                    if let Some(result) = result {
                        match result {
                            Ok((name, outcome)) => {
                                if let Err(e) = outcome { eprintln!("listener {name} ended: {e}"); }
                                if let Some(server) = self.servers.get_mut(&name) {
                                    if let Err(e) = server.close().await { eprintln!("closing {name}: {e}"); }
                                }
                            }
                            Err(e) => eprintln!("listener task failed: {e}"),
                        }
                    }
                }
            }
        }
    }

    async fn reload(&mut self) -> Result<()> {
        let profiles = self.home.discover_identity_profiles()?;
        let present = profiles.iter().map(|p| p.name()).collect::<HashSet<_>>();
        for (name, server) in &mut self.servers {
            if !present.contains(name.as_str()) && !server.cancel.is_cancelled() {
                if let Err(e) = server.close().await {
                    eprintln!("closing removed server {name}: {e}");
                }
            }
        }
        for profile in profiles {
            if let Some(server) = self.servers.get_mut(profile.name()) {
                if !server.cancel.is_cancelled() {
                    if let Err(e) = server.reload().await {
                        eprintln!("keeping server {}: {e}", server.name());
                    }
                }
                continue;
            }
            match Server::load(profile.clone(), self.runtime.clone(), self.terminal.clone()).await {
                Ok(server) => {
                    if server.config.listen != 0 {
                        // Endpoint.listen checks its scopes against the immutable startup rules.
                        if dhttp::DhttpNetwork::global().is_err() {
                            eprintln!("new listening server {} requires a restart", server.name());
                            continue;
                        }
                        let (name, listener) = (server.name().to_owned(), server.listen());
                        self.listeners.spawn(async move { (name, listener.await) });
                    }
                    self.servers.insert(server.name().to_owned(), server);
                }
                Err(e) => eprintln!("skipping server {}: {e}", profile.name()),
            }
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut result = Ok(());
        // Cancel every identity before waiting for any application execution.
        for server in self.servers.values() {
            server.cancel.cancel();
            if dhttp::DhttpNetwork::global().is_ok() {
                if let Err(e) = server.endpoint.close() {
                    result = Err(e.into());
                }
            }
            server.tasks.close();
        }
        let own = async {
            let mut outcome = Ok(());
            for server in self.servers.values_mut() {
                if let Err(e) = server.close().await {
                    outcome = Err(e);
                }
            }
            while let Some(joined) = self.listeners.join_next().await {
                match joined {
                    Ok((_, Err(e))) => eprintln!("listener shutdown: {e}"),
                    Err(e) => outcome = Err(e.into()),
                    _ => {}
                }
            }
            outcome
        };
        match tokio::time::timeout_at(deadline, own).await {
            Ok(Err(e)) => result = Err(e),
            Err(_) => result = Err(Error::ShutdownDeadline),
            _ => {}
        }
        if let Err(e) = self.terminal.shutdown(deadline.into_std()).await {
            result = Err(e);
        }
        if let Ok(network) = dhttp::DhttpNetwork::global() {
            if let Err(e) = network.shutdown() {
                result = Err(e.into());
            }
        }
        result
    }
}

impl Server {
    async fn load(
        profile: IdentityProfile,
        runtime: Arc<Runtime>,
        terminal: Arc<TerminalManager>,
    ) -> Result<Self> {
        let identity = profile
            .load_identity()
            .await
            .map_err(|e| Error::InvalidIdentity(e.to_string()))?;
        if identity.name != profile.name() {
            return Err(Error::IdentityMismatch);
        }
        let endpoint = dhttp::Endpoint::load(profile.name()).await?;
        let config = load_server_config(&profile)?;
        let subject =
            access_control::SubjectId::new(dhttp::certificate::subject_id(&identity.certs)?)
                .map_err(|_| Error::InvalidIdentity("invalid certificate subject".into()))?;
        std::fs::create_dir_all(profile.db_dir())?;
        let uri = format!("sqlite://{}?mode=rwc", profile.access_db_path().display());
        let access = Arc::new(
            access_control::AccessService::load_from_db(&uri, &identity.name, &subject).await?,
        );
        let cancel = CancellationToken::new();
        let libs = load_libs(&profile, &runtime, &BTreeMap::new(), &cancel)?;
        let lib_slots = Arc::new(Semaphore::new(4));
        let tasks = TaskTracker::new();
        let router = build_router(
            endpoint.clone(),
            access.clone(),
            &libs,
            &config,
            &profile,
            lib_slots.clone(),
            tasks.clone(),
        )?;
        Ok(Self {
            profile,
            endpoint,
            config,
            access,
            router: Arc::new(RwLock::new(router)),
            libs,
            runtime,
            lib_slots,
            terminal,
            tasks,
            cancel,
        })
    }
    fn name(&self) -> &str {
        self.endpoint.name()
    }

    async fn reload(&mut self) -> Result<()> {
        let config = load_server_config(&self.profile)?;
        if config.listen != self.config.listen {
            return Err(Error::InvalidConfig(
                "listen changes require restarting the instance".into(),
            ));
        }
        let libs = load_libs(&self.profile, &self.runtime, &self.libs, &self.cancel)?;
        let router = build_router(
            self.endpoint.clone(),
            self.access.clone(),
            &libs,
            &config,
            &self.profile,
            self.lib_slots.clone(),
            self.tasks.clone(),
        )?;
        if !self.profile.path().symlink_metadata()?.is_dir() {
            return Err(Error::InvalidIdentity(
                "identity directory disappeared".into(),
            ));
        }
        for (id, lib) in &libs {
            if self.libs.get(id).is_some_and(|old| Arc::ptr_eq(old, lib)) {
                continue;
            }
            let path = self.profile.join("lib").join(id).join("lib.wasm");
            let bytes = std::fs::read(path)?;
            if <[u8; 32]>::from(Sha256::digest(bytes)) != lib.digest {
                return Err(Error::InvalidComponent(
                    "component changed during reload".into(),
                ));
            }
        }
        *self.router.write().unwrap() = router;
        for (id, old) in &self.libs {
            if !libs.contains_key(id) {
                old.cancel.cancel();
            }
        }
        self.libs = libs;
        self.config = config;
        Ok(())
    }

    fn listen(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>> {
        let (endpoint, router, cancel, terminal, listen) = (
            self.endpoint.clone(),
            self.router.clone(),
            self.cancel.clone(),
            self.terminal.clone(),
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
            endpoint.listen(scopes, tower::service_fn(move |mut request: http::Request<Body>| {
                let (router, cancel, terminal, name) = (router.clone(), cancel.clone(), terminal.clone(), name.clone());
                async move {
                    let result: Result<http::Response<Body>> = async {
                        if cancel.is_cancelled() { return Err(Error::Closed); }
                        let summary = request.extensions().get::<dhttp::HandshakeSummary>().ok_or(Error::MissingHandshake)?;
                        if summary.local.as_ref().is_none_or(|l| l.name() != name) { return Err(Error::IdentityMismatch); }
                        let identity = dhttp_identity::name::DhttpName::try_from(name.clone()).map_err(|_| Error::IdentityMismatch)?;
                        *request.uri_mut() = identity.expand_uri(request.uri().clone()).map_err(|_| Error::IdentityMismatch)?;
                        let authority = request.uri().authority().ok_or(Error::IdentityMismatch)?;
                        if authority.as_str().contains('@') || dhttp_home::normalize_name(authority.host()).as_deref() != Some(name.as_str()) { return Err(Error::IdentityMismatch); }
                        if matches!(request.uri().path(), "/shell" | "/.pishoo/terminal") || request.uri().path().starts_with("/shell/") {
                            return terminal.handle(&name, cancel, request).await;
                        }
                        let names = request.headers().keys().filter(|n| n.as_str().starts_with("pishoo-")).cloned().collect::<Vec<_>>();
                        for name in names { request.headers_mut().remove(name); }
                        let app = router.read().unwrap().clone();
                        let response = tokio::select! {
                            _ = cancel.cancelled() => return Err(Error::Closed),
                            response = app.oneshot(request.map(axum::body::Body::new)) => response.expect("Router is infallible"),
                        };
                        Ok(response.map(|b| b.map_err(Into::into).boxed_unsync()))
                    }.await;
                    let response = result.unwrap_or_else(|error| {
                        let status = error.status();
                        if status.is_server_error() { eprintln!("request for {name}: {error}"); }
                        (status, status.canonical_reason().unwrap_or("request failed")).into_response().map(|b| b.map_err(Into::into).boxed_unsync())
                    });
                    Ok::<_, std::convert::Infallible>(response)
                }
            })).await.map_err(Into::into)
        })
    }

    async fn close(&mut self) -> Result<()> {
        self.cancel.cancel();
        self.lib_slots.close();
        let result = if dhttp::DhttpNetwork::global().is_ok() {
            self.endpoint.close().map_err(Error::from)
        } else {
            Ok(())
        };
        *self.router.write().unwrap() = axum::Router::new();
        self.libs.clear();
        self.tasks.close();
        tokio::time::timeout(Duration::from_secs(15), self.tasks.wait())
            .await
            .map_err(|_| Error::ShutdownDeadline)?;
        result
    }
}

fn load_libs(
    profile: &IdentityProfile,
    runtime: &Arc<Runtime>,
    old: &BTreeMap<String, Arc<Lib>>,
    cancel: &CancellationToken,
) -> Result<BTreeMap<String, Arc<Lib>>> {
    let root = profile.join("lib");
    let mut candidates = BTreeMap::new();
    match root.symlink_metadata() {
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(Error::InvalidComponent(
                "lib root must be a directory, not a symlink".into(),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(candidates),
        Err(e) => return Err(e.into()),
        _ => {}
    }
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(candidates),
        Err(e) => return Err(e.into()),
    };
    let mut entries = entries.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if id.is_empty()
            || id.len() > 63
            || !id.as_bytes()[0].is_ascii_lowercase()
            || !id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            continue;
        }
        let path = entry.path().join("lib.wasm");
        let metadata = match path.symlink_metadata() {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let result = (|| -> Result<Arc<Lib>> {
            if !metadata.file_type().is_file() || metadata.len() > 64 * 1024 * 1024 {
                return Err(Error::InvalidComponent("invalid component file".into()));
            }
            let bytes = std::fs::read(&path)?;
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            if let Some(lib) = old.get(&id).filter(|lib| lib.digest == digest) {
                return Ok(lib.clone());
            }
            let token = old
                .get(&id)
                .map_or_else(|| cancel.child_token(), |lib| lib.cancel.clone());
            let lib = Lib::load(
                runtime.clone(),
                id.clone(),
                &bytes,
                &entry.path().join("data"),
                LibPolicy::default(),
                token,
            )?;
            Ok(Arc::new(lib))
        })();
        match result {
            Ok(lib) => {
                candidates.insert(id, lib);
            }
            Err(e) => {
                eprintln!("keeping/skipping lib {id}: {e}");
                if let Some(lib) = old.get(&id) {
                    candidates.insert(id, lib.clone());
                }
            }
        }
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests;

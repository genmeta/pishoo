use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, RwLock},
    time::Duration,
};

use dhttp_home::{DhttpHome, identity::IdentityProfile};
use tokio::{sync::Semaphore, task::JoinSet};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    Error, Result,
    sandbox::{Runtime, Sandbox},
    setup::ServerConfig,
};

mod server;

struct Server {
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    config: ServerConfig,
    access: Arc<access_control::AccessService>,
    router: Arc<RwLock<axum::Router>>,
    sandbox: Sandbox,
    exec_tasks: TaskTracker,
    exec_slots: Arc<Semaphore>,
    cancel: CancellationToken,
}

pub async fn run() -> Result<()> {
    // The open file owns the instance lock for this entire invocation.
    let home = DhttpHome::load(dhttp_home::HomeScope::User)
        .map_err(|e| Error::InvalidConfig(e.to_string()))?;
    let state_dir = home.as_path();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state_dir.join("pishoo.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .map_err(|e| Error::InvalidConfig(format!("instance is already running: {e}")))?;
    let runtime = Arc::new(Runtime::new()?);
    let mut servers = BTreeMap::new();
    for profile in home.discover_identity_profiles()? {
        match Server::load(profile.clone(), runtime.clone()).await {
            Ok(server) => {
                servers.insert(server.name().to_owned(), server);
            }
            Err(e) => eprintln!("skipping server {}: {e}", profile.name()),
        }
    }
    dhttp::DhttpNetwork::init().await?;
    let mut listeners = JoinSet::new();
    for server in servers.values().filter(|s| s.config.listen != 0) {
        let (name, listener) = (server.name().to_owned(), server.listen());
        listeners.spawn(async move { (name, listener.await) });
    }
    let result = async {
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
                _ = interval.tick() => {
                    let reload = async {
                        let profiles = home.discover_identity_profiles()?;
                        let present = profiles.iter().map(|p| p.name()).collect::<HashSet<_>>();
                        let mut stopped_without_listener = Vec::new();
                        for (name, server) in &mut servers {
                            if !present.contains(name.as_str()) && !server.cancel.is_cancelled() {
                                if let Err(e) = server.close().await {
                                    eprintln!("closing removed server {name}: {e}");
                                }
                                if server.config.listen == 0 {
                                    stopped_without_listener.push(name.clone());
                                }
                            }
                        }
                        for name in stopped_without_listener {
                            servers.remove(&name);
                        }
                        for profile in profiles {
                            if let Some(server) = servers.get_mut(profile.name()) {
                                if !server.cancel.is_cancelled() {
                                    if let Err(e) = server.reload().await {
                                        eprintln!("keeping server {}: {e}", server.name());
                                    }
                                }
                                continue;
                            }
                            match Server::load(profile.clone(), runtime.clone()).await {
                                Ok(server) => {
                                    if server.config.listen != 0 {
                                        // Endpoint.listen checks its scopes against the immutable startup rules.
                                        if dhttp::DhttpNetwork::global().is_err() {
                                            eprintln!("new listening server {} requires a restart", server.name());
                                            continue;
                                        }
                                        let (name, listener) = (server.name().to_owned(), server.listen());
                                        listeners.spawn(async move { (name, listener.await) });
                                    }
                                    servers.insert(server.name().to_owned(), server);
                                }
                                Err(e) => eprintln!("skipping server {}: {e}", profile.name()),
                            }
                        }
                        Ok::<(), Error>(())
                    }.await;
                    if let Err(e) = reload { eprintln!("reload rejected: {e}"); }
                },
                result = listeners.join_next(), if !listeners.is_empty() => {
                    if let Some(result) = result {
                        match result {
                            Ok((name, outcome)) => {
                                if let Err(e) = outcome { eprintln!("listener {name} ended: {e}"); }
                                if let Some(server) = servers.get_mut(&name) {
                                    if !server.cancel.is_cancelled() {
                                        if let Err(e) = server.close().await { eprintln!("closing {name}: {e}"); }
                                    }
                                }
                                servers.remove(&name);
                            }
                            Err(e) => eprintln!("listener task failed: {e}"),
                        }
                    }
                }
            }
        }
    }.await;
    let shutdown = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut result = Ok(());
        // Cancel every identity before waiting for any application execution.
        for server in servers.values_mut() {
            server.cancel.cancel();
            if dhttp::DhttpNetwork::global().is_ok() {
                if let Err(e) = server.endpoint.stop_listening() {
                    result = Err(e.into());
                }
            }
            server.sandbox.close();
            server.exec_tasks.close();
            server.exec_slots.close();
        }
        let own = async {
            let mut outcome = Ok(());
            for server in servers.values_mut() {
                if let Err(e) = server.close().await {
                    outcome = Err(e);
                }
            }
            while let Some(joined) = listeners.join_next().await {
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
        result
    }
    .await;
    drop(lock);
    result.and(shutdown)
}

#[cfg(test)]
#[path = "../tests/unit/daemon.rs"]
mod tests;

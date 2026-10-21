//! Drive the process event loop and serial reconciliation of identity servers.
use std::{collections::BTreeMap, sync::Arc};

use dhttp_home::{DhttpHome, identity::IdentityProfile};
use futures::{StreamExt, stream::FuturesUnordered};
use tokio::time::Instant;

use super::{Server, current_router, native_router, ocsp::renew_ocsp};
use crate::{
    Error, Result, dns,
    routes::authorize,
    sandbox::{Sandbox, WasmRuntime},
    setup::{ServerConfig, read_server_config},
};

pub(super) fn ocsp_ticks() -> tokio::time::Interval {
    let period = std::time::Duration::from_secs(3 * 24 * 60 * 60);
    let mut interval = tokio::time::interval_at(Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

pub async fn run() -> Result<()> {
    #[cfg(target_os = "linux")]
    let multiuser = rustix::process::geteuid().is_root();
    #[cfg(not(target_os = "linux"))]
    let multiuser = false;
    let home = if multiuser {
        DhttpHome::load(dhttp_home::HomeScope::Global)
            .map_err(|e| Error::InvalidConfig(e.to_string()))?
    } else {
        DhttpHome::for_user_home_dir(
            dirs::home_dir()
                .ok_or_else(|| Error::InvalidConfig("cannot locate user home directory".into()))?,
        )
    };
    let runtime = Arc::new(WasmRuntime::new()?);
    let mdns = dns::install()?;
    let addresses = qprotocol::AddressBook::global();
    let mut inner_events = addresses.subscribe_punch(dhttp::Scope::Internal);
    let mut outer = addresses.subscribe_ddns();
    let mut servers = BTreeMap::<String, Server>::new();
    let mut jobs = FuturesUnordered::new();
    // Startup failures share the same DNS and application cleanup path as shutdown.
    let result = async {
        // OCSP bootstrap uses ordinary HTTPS and qtls's configured trust roots.
        dhttp::DhttpNetwork::init().await?;
        let user_homes = multiuser.then(account_homes).transpose()?;
        for profile in discover_profiles(&home, user_homes.as_deref())? {
            if let Some(server) = load_profile(profile, runtime.clone()).await? {
                servers.insert(server.name().to_owned(), server);
            }
        }
        for server in servers.values_mut().filter(|s| s.config.listen != "off") {
            server.listener = Some(server.listen().await?);
            tracing::trace!(endpoint = server.name(), listen = server.config.listen, "Pishoo listener ready");
        }
        let mut config_ticks = tokio::time::interval_at(
            Instant::now() + std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
        );
        config_ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut wasi_ticks = tokio::time::interval(std::time::Duration::from_secs(3));
        wasi_ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut runs = FuturesUnordered::new();
        let mut ocsp_ticks = ocsp_ticks();
        let mut ocsp_updates = FuturesUnordered::new();
        let interrupt = tokio::signal::ctrl_c();
        tokio::pin!(interrupt);
        #[cfg(unix)]
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        #[cfg(unix)]
        let mut reload = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        let mut removed_bounds = Vec::new();
        loop {
            let endpoints = servers.values()
                .filter(|s| matches!(s.config.listen.as_str(), "internal" | "all"))
                .map(|s| s.endpoint.clone()).collect::<Vec<_>>();
            let mut next_due = match dns::maintain_mdns(&mdns, &endpoints, &removed_bounds).await {
                Ok(()) => None,
                Err(error) => {
                    tracing::warn!(%error, "DNS maintenance failed");
                    Some(Instant::now() + std::time::Duration::from_secs(5))
                }
            };
            removed_bounds.clear();
            let snapshot = outer.borrow_and_update().clone();
            for server in servers.values() {
                if let Some(publisher) = &server.publisher {
                    jobs.push(dns::publish(server.name().to_owned(), publisher.clone(), snapshot.clone()));
                }
            }
            loop {
                tokio::select! {
                    result = &mut interrupt => { result?; return Ok(()); }
                    _ = async { #[cfg(unix)] { terminate.recv().await; } #[cfg(not(unix))] { std::future::pending::<()>().await; } } => return Ok(()),
                    result = jobs.next(), if !jobs.is_empty() => {
                        if let Some(Some(due)) = result {
                            next_due = Some(next_due.map_or(due, |previous| previous.min(due)));
                        }
                    }
                    _ = wasi_ticks.tick(), if runs.is_empty() => {
                        for server in servers.values() {
                            let sandbox = server.sandbox.clone();
                            let endpoint = server.endpoint.clone();
                            let access = server.access.clone();
                            let profile = server.profile.clone();
                            let contact_db = server.contact_db.clone();
                            let api = native_router(server).layer(axum::middleware::from_fn_with_state(access.clone(),authorize));
                            runs.push(async move {
                                if let Err(error) = crate::contact::contact_jobs(&profile, &endpoint, &access, &contact_db).await {
                                    tracing::warn!(identity = endpoint.name(), %error, "Contact delivery failed");
                                }
                                Sandbox::run(sandbox,endpoint,access,api).await
                            });
                        }
                    }
                    _ = runs.next(), if !runs.is_empty() => {},
                    _ = async {
                        #[cfg(unix)]
                        { reload.recv().await; }
                        #[cfg(not(unix))]
                        { std::future::pending::<()>().await; }
                    } => {
                        // Apply through the same serial reconciliation after active DNS/OCSP work.
                        config_ticks.reset_immediately();
                    }
                    _ = config_ticks.tick(), if jobs.is_empty() && ocsp_updates.is_empty() => {
                        let result = async {
                            let user_homes = multiuser.then(account_homes).transpose()?;
                            reconcile_profiles(&mdns, &home, user_homes.as_deref(), &mut servers, runtime.clone()).await
                        }.await;
                        match result {
                            Ok(true) => break,
                            Ok(false) => {},
                            Err(error) => tracing::warn!(%error,
                                "Server discovery failed; retaining active identities"),
                        }
                    }
                    _ = ocsp_ticks.tick(), if ocsp_updates.is_empty() => {
                        for server in servers.values() {
                            let profile = server.profile.clone();
                            let endpoint = server.endpoint.clone();
                            ocsp_updates.push(async move {
                                let result = renew_ocsp(&profile, &endpoint).await;
                                (profile.name().to_owned(), result)
                            });
                        }
                    }
                    Some((name, result)) = ocsp_updates.next(), if jobs.is_empty() && !ocsp_updates.is_empty() => {
                        let server = servers.get_mut(&name).expect("loaded identity remains registered");
                        let result = match result {
                            Ok(endpoint) => apply_ocsp(server, endpoint).await,
                            Err(error) => Err(error),
                        };
                        match result {
                            Ok(()) => tracing::info!(identity = %name, "OCSP refreshed"),
                            Err(error) => tracing::warn!(identity = %name, %error, "OCSP refresh failed"),
                        }
                        // Publish the current credentials after the preceding DNS batch ends.
                        break;
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
                    _ = async { if let Some(due) = next_due { tokio::time::sleep_until(due).await; } }, if jobs.is_empty() && next_due.is_some() => { break; }
                }
            }
        }
    }.await;
    // Remove all local mDNS response records before waiting for outstanding work.
    for server in servers.values() {
        remove_mdns_name(&mdns, server.name());
    }
    // No new batches after the run body exits, including failure during startup.
    while jobs.next().await.is_some() {}
    let mut shutdown = Ok(());
    for server in servers.values_mut() {
        if let Err(error) = server.close().await {
            tracing::error!(identity = server.name(), %error, "server shutdown failed");
            shutdown = Err(error);
        }
    }
    if let Err(error) = mdns.shutdown().await {
        tracing::error!(%error, "mDNS shutdown failed");
        shutdown = Err(Error::Io(std::io::Error::other(error)));
    }
    result.and(shutdown)
}

pub(super) async fn load_profile(
    profile: IdentityProfile,
    runtime: Arc<WasmRuntime>,
) -> Result<Option<Server>> {
    let name = profile.name().to_owned();
    // Credentials alone do not opt a browser/client identity into hosting.
    // Startup and runtime discovery share this check before any identity loading.
    if !profile.join(".volume/sys/setting/database").try_exists()?
        && !profile.config_db_path().try_exists()?
    {
        return Ok(None);
    }
    // An explicitly disabled identity needs no credentials, WASM or background workers.
    match crate::setup::config_database(
        &profile,
        &http::Method::GET,
        &http::Uri::from_static("/std/sys/listen"),
        None,
    ) {
        Ok(settings) if settings["listen"] == "off" => return Ok(None),
        Ok(_) => {}
        Err(error) => {
            // Preserve startup recovery of a genuinely empty database, but do not
            // initialize it before validating this identity's credentials.
            let empty = profile.join(".volume/sys/setting/database").symlink_metadata()?.file_type().is_file()
                && rusqlite::Connection::open_with_flags(profile.join(".volume/sys/setting/database"),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .ok().is_some_and(|conn| conn.query_row(
                        "SELECT NOT EXISTS(SELECT 1 FROM sqlite_master WHERE name NOT LIKE 'sqlite_%') AND (SELECT user_version FROM pragma_user_version) = 0",
                        [], |row| row.get::<_, bool>(0)).unwrap_or(false));
            if !empty {
                return Err(error);
            }
        }
    }
    match Server::load(profile, runtime).await {
        Ok(mut server) if server.config.listen == "off" => {
            server.close().await?;
            Ok(None)
        }
        Ok(server) => Ok(Some(server)),
        Err(error @ Error::InvalidIdentity(_)) => {
            tracing::warn!(identity = %name, %error, "skipping identity");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

// getent uses the distribution's configured account sources (including NSS).
// Minimal systems without getent still have their local passwd database.
fn account_homes() -> Result<Vec<std::path::PathBuf>> {
    let records = match std::process::Command::new("getent").arg("passwd").output() {
        Ok(output) if output.status.success() => String::from_utf8(output.stdout)
            .map_err(|error| Error::InvalidConfig(format!("invalid account database: {error}")))?,
        Ok(output) => {
            return Err(Error::InvalidConfig(format!(
                "account discovery failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::read_to_string("/etc/passwd")?
        }
        Err(error) => return Err(error.into()),
    };
    Ok(parse_account_homes(&records))
}

pub(super) fn parse_account_homes(records: &str) -> Vec<std::path::PathBuf> {
    let mut homes = records
        .lines()
        .filter_map(|line| line.split(':').nth(5))
        .map(std::path::PathBuf::from)
        .filter(|home| home.is_absolute())
        .collect::<Vec<_>>();
    homes.sort();
    homes.dedup();
    homes
}

// Paths and discovery results are local to each scan; Server retains the actual profile.
pub(super) fn discover_profiles(
    home: &DhttpHome,
    user_homes: Option<&[std::path::PathBuf]>,
) -> Result<Vec<IdentityProfile>> {
    let Some(user_homes) = user_homes else {
        return home.discover_identity_profiles().map_err(Into::into);
    };
    let mut homes = vec![home.clone()];
    for user_home in user_homes {
        match user_home.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_dir() => {
                homes.push(DhttpHome::for_user_home_dir(user_home));
            }
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
    }
    homes.sort_by(|a, b| a.as_path().cmp(b.as_path()));
    homes.dedup_by(|a, b| a.as_path() == b.as_path());
    let mut profiles = BTreeMap::<String, IdentityProfile>::new();
    for home in homes {
        match home.as_path().symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
        for profile in home.discover_identity_profiles()? {
            if let Some(previous) = profiles.get(profile.name()) {
                return Err(Error::InvalidConfig(format!(
                    "duplicate identity {} in {} and {}",
                    profile.name(),
                    previous.path().display(),
                    profile.path().display()
                )));
            }
            profiles.insert(profile.name().to_owned(), profile);
        }
    }
    Ok(profiles.into_values().collect())
}

// mDNS responders are process-owned; retire this name on every interface before
// waiting for application cleanup. Shared responders and other names remain live.
fn remove_mdns_name(mdns: &ddns::mdns::MdnsResolverSet, name: &str) {
    for instance in mdns.snapshot() {
        instance.remove_name(name);
    }
}

// Reconcile each identity serially; a failed discovery never removes active resources.
pub(super) async fn reconcile_profiles(
    mdns: &ddns::mdns::MdnsResolverSet,
    home: &DhttpHome,
    user_homes: Option<&[std::path::PathBuf]>,
    servers: &mut BTreeMap<String, Server>,
    runtime: Arc<WasmRuntime>,
) -> Result<bool> {
    let profiles = discover_profiles(home, user_homes)?;
    let mut changed = false;
    let removed = servers
        .keys()
        .filter(|name| {
            !profiles.iter().any(|profile| {
                profile.name() == name.as_str() && profile.path() == servers[*name].profile.path()
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    for name in removed {
        let mut server = servers.remove(&name).unwrap();
        changed = true;
        remove_mdns_name(mdns, &name);
        if let Err(error) = server.close().await {
            tracing::warn!(identity = %name, %error, "Server removal cleanup failed");
        }
        tracing::info!(identity = %name, "Server removed");
    }
    for profile in profiles {
        let name = profile.name().to_owned();
        let result = async {
            if servers.contains_key(&name) {
                // End the read transaction before applying changes or awaiting I/O.
                let config = {
                    let mut conn = servers[&name].config_db.lock().unwrap();
                    read_server_config(&mut conn)?
                };
                if config.listen == "off" {
                    let mut server = servers.remove(&name).unwrap();
                    changed = true;
                    remove_mdns_name(mdns, &name);
                    server.close().await?;
                    tracing::info!(identity = %name, "Server disabled");
                } else {
                    let server = servers.get_mut(&name).unwrap();
                    if !same_config(&server.config, &config) {
                        let result = apply_config(server, config).await;
                        if !matches!(server.config.listen.as_str(), "internal" | "all")
                            || server.listener.is_none()
                        {
                            remove_mdns_name(mdns, &name);
                        }
                        // If re-registration and rollback both fail, release the resources
                        // and retry loading this identity on the next scan.
                        if server.config.listen != "off" && server.listener.is_none() {
                            let mut server = servers.remove(&name).unwrap();
                            changed = true;
                            if let Err(error) = server.close().await {
                                tracing::warn!(identity = %name, %error, "Server cleanup failed");
                            }
                        }
                        result?;
                        changed = true;
                        tracing::info!(identity = %name, "Server configuration applied");
                    }
                }
            } else if let Some(mut server) = load_profile(profile, runtime.clone()).await? {
                if server.config.listen != "off" {
                    match server.listen().await {
                        Ok(listener) => server.listener = Some(listener),
                        Err(error) => {
                            server.close().await?;
                            return Err(error);
                        }
                    }
                }
                servers.insert(name.clone(), server);
                changed = true;
                tracing::info!(identity = %name, "Server enabled");
            }
            Ok::<_, Error>(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(identity = %name, %error, "Server configuration was not applied; retrying on next scan");
        }
    }
    Ok(changed)
}

fn same_config(a: &ServerConfig, b: &ServerConfig) -> bool {
    a.listen == b.listen
        && a.proxy_locations.len() == b.proxy_locations.len()
        && a.proxy_locations
            .iter()
            .zip(&b.proxy_locations)
            .all(|(a, b)| {
                a.location == b.location
                    && a.proxy_pass.scheme == b.proxy_pass.scheme
                    && a.proxy_pass.authority == b.proxy_pass.authority
                    && a.proxy_pass.path_and_query == b.proxy_pass.path_and_query
            })
}

async fn apply_config(server: &mut Server, config: ServerConfig) -> Result<()> {
    let scope_changed = server.config.listen != config.listen;
    let publisher = if scope_changed && config.listen != "off" {
        dns::authority(&server.endpoint)
            .map_err(|error| Error::InvalidIdentity(error.to_string()))?;
        if matches!(config.listen.as_str(), "external" | "all") {
            Some(dns::publisher(&server.endpoint)?)
        } else {
            None
        }
    } else if scope_changed {
        None
    } else {
        server.publisher.clone()
    };
    let previous = std::mem::replace(&mut server.config, config);
    if scope_changed {
        // Dropping the resource unregisters this name without closing shared connections.
        server.listener.take();
        if server.config.listen != "off" {
            match server.listen().await {
                Ok(listener) => server.listener = Some(listener),
                Err(error) => {
                    server.config = previous;
                    if server.config.listen != "off" {
                        match server.listen().await {
                            Ok(listener) => server.listener = Some(listener),
                            Err(rollback) => tracing::error!(identity = server.name(), %rollback,
                                "could not restore previous listener"),
                        }
                    }
                    return Err(error);
                }
            }
        }
    }
    server.publisher = publisher;
    *server.router.write().unwrap() = current_router(server);
    Ok(())
}

pub(super) async fn apply_ocsp(server: &mut Server, endpoint: dhttp::Endpoint) -> Result<()> {
    let publisher = if matches!(server.config.listen.as_str(), "external" | "all") {
        Some(dns::publisher(&endpoint)?)
    } else {
        None
    };
    server.endpoint = endpoint;
    server.publisher = publisher;
    *server.router.write().unwrap() = current_router(server);
    Ok(())
}

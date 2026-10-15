use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use axum::{Router, body::Body as AxumBody, response::IntoResponse, routing::any};
use dhttp_home::{DhttpHome, identity::IdentityProfile};
use futures::{StreamExt, stream::FuturesUnordered};
use http::Request;
use tokio::time::Instant;
use tower::ServiceExt;

use crate::{
    Body, Error, Result, dns,
    routes::{DHTTP_PREFIX, access_router, authorize, file_router, forward_dhttp, proxy_pass},
    sandbox::{Sandbox, WasmRuntime},
    setup::{ServerConfig, config_router, load_server_config},
};

struct Server {
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    config: ServerConfig,
    access: Arc<access_control::AccessService>,
    router: Arc<RwLock<axum::Router>>,
    sandbox: Arc<RwLock<Sandbox>>,
    publisher: Option<Arc<ddns::H3Resolver>>,
    listener: Option<dhttp::ListenFuture>,
}

fn ocsp_ticks() -> tokio::time::Interval {
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
        for server in servers.values_mut().filter(|s| s.config.listen != 0) {
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
                .filter(|s| s.config.listen & 1 != 0)
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
                            let api = native_router(server).layer(axum::middleware::from_fn_with_state(access.clone(),authorize));
                            runs.push(async move {
                                if let Err(error) = crate::contact::contact_jobs(&profile, &endpoint, &access).await {
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

#[cfg(test)]
#[path = "../tests/unit/server.rs"]
mod tests;

async fn load_profile(
    profile: IdentityProfile,
    runtime: Arc<WasmRuntime>,
) -> Result<Option<Server>> {
    let name = profile.name().to_owned();
    // Credentials alone do not opt a browser/client identity into hosting.
    // Startup and runtime discovery share this check before any identity loading.
    if !profile.config_db_path().try_exists()? {
        return Ok(None);
    }
    // An explicitly disabled identity needs no credentials, WASM or background workers.
    match crate::setup::config_database(
        &profile,
        &http::Method::GET,
        &http::Uri::from_static("/std/sys/settings"),
        None,
    ) {
        Ok(settings) if settings["enabled"] == false => return Ok(None),
        Ok(_) => {}
        Err(error) => {
            // Preserve startup recovery of a genuinely empty database, but do not
            // initialize it before validating this identity's credentials.
            let empty = profile.config_db_path().symlink_metadata()?.file_type().is_file()
                && rusqlite::Connection::open_with_flags(profile.config_db_path(),
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
        Ok(mut server) if !server.config.enabled => {
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

fn parse_account_homes(records: &str) -> Vec<std::path::PathBuf> {
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
fn discover_profiles(
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
async fn reconcile_profiles(
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
                // Validate without initialization first: missing/corrupt active databases
                // must not be silently recreated by the startup initializer.
                crate::setup::config_database(
                    &profile,
                    &http::Method::GET,
                    &http::Uri::from_static("/std/sys/settings"),
                    None,
                )?;
                let config = load_server_config(&profile)?;
                if !config.enabled {
                    let mut server = servers.remove(&name).unwrap();
                    changed = true;
                    remove_mdns_name(mdns, &name);
                    server.close().await?;
                    tracing::info!(identity = %name, "Server disabled");
                } else {
                    let server = servers.get_mut(&name).unwrap();
                    if !same_config(&server.config, &config) {
                        let result = apply_config(server, config).await;
                        if server.config.listen & 1 == 0 || server.listener.is_none() {
                            remove_mdns_name(mdns, &name);
                        }
                        // If re-registration and rollback both fail, release the resources
                        // and retry loading this identity on the next scan.
                        if server.config.listen != 0 && server.listener.is_none() {
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
                if server.config.listen != 0 {
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
    a.enabled == b.enabled
        && a.listen == b.listen
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
    let publisher = if scope_changed && config.listen != 0 {
        dns::authority(&server.endpoint)
            .map_err(|error| Error::InvalidIdentity(error.to_string()))?;
        if config.listen & 2 != 0 {
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
        if server.config.listen != 0 {
            match server.listen().await {
                Ok(listener) => server.listener = Some(listener),
                Err(error) => {
                    server.config = previous;
                    if server.config.listen != 0 {
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

async fn renew_ocsp(
    profile: &IdentityProfile,
    endpoint: &dhttp::Endpoint,
) -> Result<dhttp::Endpoint> {
    // Renew the loaded certificate's proof; certificate/key rotation still requires restart.
    let authority = endpoint.local_authority()?;
    let response = fetch_ocsp(authority.certificates()).await?;
    cache_ocsp(profile, &response)?;
    endpoint.reload().await.map_err(Into::into)
}

async fn apply_ocsp(server: &mut Server, endpoint: dhttp::Endpoint) -> Result<()> {
    let publisher = if server.config.listen & 2 != 0 {
        Some(dns::publisher(&endpoint)?)
    } else {
        None
    };
    server.endpoint = endpoint;
    server.publisher = publisher;
    *server.router.write().unwrap() = current_router(server);
    Ok(())
}

fn dhttp_router(endpoint: dhttp::Endpoint) -> Router {
    Router::new().route(
        &format!("{DHTTP_PREFIX}{{*path}}"),
        any(move |request: Request<AxumBody>| forward_dhttp(endpoint.clone(), request)),
    )
}

fn initialize_directories(profile: &IdentityProfile) -> Result<()> {
    for directory in [
        "db",
        "file",
        "libs",
        "libs/std",
        "apps",
        "apps/std",
        "logs",
        "repo",
        "templates",
    ] {
        let path = profile.join(directory);
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        if !path.symlink_metadata()?.file_type().is_dir() {
            return Err(Error::InvalidConfig(format!(
                "{} must be a directory, not a symlink",
                path.display()
            )));
        }
    }
    Ok(())
}

fn cache_ocsp(profile: &IdentityProfile, response: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut cache = tempfile::NamedTempFile::new_in(profile.ssl_dir())?;
    cache.write_all(response)?;
    cache.as_file().sync_all()?;
    cache
        .persist(profile.ocsp_path())
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(profile.ssl_dir())?.sync_all()?;
    Ok(())
}

async fn fetch_ocsp(certificates: &[dhttp::CertificateDer<'_>]) -> Result<Vec<u8>> {
    use der::{Decode, Encode};
    use sha1::{Digest, Sha1};
    use x509_parser::prelude::FromDer;
    let error = |error: String| Error::InvalidIdentity(format!("OCSP: {error}"));
    let leaf = certificates
        .first()
        .ok_or_else(|| error("missing certificate".into()))?;
    let issuer = certificates
        .get(1)
        .ok_or_else(|| error("missing issuer certificate".into()))?;
    let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(leaf.as_ref())
        .map_err(|e| error(e.to_string()))?;
    if !parsed.validity().is_valid() {
        return Err(error("certificate is outside its validity period".into()));
    }
    let url = parsed
        .extensions()
        .iter()
        .find_map(|extension| {
            let x509_parser::extensions::ParsedExtension::AuthorityInfoAccess(aia) =
                extension.parsed_extension()
            else {
                return None;
            };
            aia.accessdescs.iter().find_map(|access| {
                if access.access_method.to_id_string() != "1.3.6.1.5.5.7.48.1" {
                    return None;
                }
                match &access.access_location {
                    x509_parser::extensions::GeneralName::URI(uri) => Some((*uri).to_owned()),
                    _ => None,
                }
            })
        })
        .ok_or_else(|| error("certificate has no OCSP responder URI".into()))?;
    let leaf = x509_cert::Certificate::from_der(leaf.as_ref()).map_err(|e| error(e.to_string()))?;
    let issuer =
        x509_cert::Certificate::from_der(issuer.as_ref()).map_err(|e| error(e.to_string()))?;
    let request = x509_ocsp::OcspRequest {
        tbs_request: x509_ocsp::TbsRequest {
            request_list: vec![x509_ocsp::Request {
                req_cert: x509_ocsp::CertId {
                    hash_algorithm: x509_cert::spki::AlgorithmIdentifierOwned {
                        oid: "1.3.14.3.2.26"
                            .parse::<der::asn1::ObjectIdentifier>()
                            .map_err(|e| error(e.to_string()))?,
                        parameters: Some(der::asn1::Any::null()),
                    },
                    issuer_name_hash: der::asn1::OctetString::new(
                        Sha1::digest(
                            issuer
                                .tbs_certificate
                                .subject
                                .to_der()
                                .map_err(|e| error(e.to_string()))?,
                        )
                        .to_vec(),
                    )
                    .map_err(|e| error(e.to_string()))?,
                    issuer_key_hash: der::asn1::OctetString::new(
                        Sha1::digest(
                            issuer
                                .tbs_certificate
                                .subject_public_key_info
                                .subject_public_key
                                .raw_bytes(),
                        )
                        .to_vec(),
                    )
                    .map_err(|e| error(e.to_string()))?,
                    serial_number: leaf.tbs_certificate.serial_number,
                },
                single_request_extensions: None,
            }],
            ..Default::default()
        },
        optional_signature: None,
    }
    .to_der()
    .map_err(|e| error(e.to_string()))?;
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .https_only(false)
            .build()
            .map_err(|e| error(e.to_string()))?;
        let uri = reqwest::Url::parse(&url).map_err(|e| error(e.to_string()))?;
        if !matches!(uri.scheme(), "http" | "https") {
            return Err(error("unsupported responder scheme".into()));
        }
        let mut response = client
            .post(uri)
            .header("Content-Type", "application/ocsp-request")
            .header("Accept", "application/ocsp-response")
            .body(request)
            .send()
            .await
            .map_err(|e| error(e.to_string()))?;
        if !response.status().is_success() {
            return Err(error(format!("responder returned {}", response.status())));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| error(e.to_string()))? {
            if body.len().saturating_add(chunk.len()) > 64 * 1024 {
                return Err(error("response exceeds 64KiB".into()));
            }
            body.extend_from_slice(&chunk);
        }
        qtls::validate_ocsp(&body, certificates, qtls::UnixTime::now())
            .map_err(|e| error(e.to_string()))?;
        Ok(body)
    })
    .await
    .map_err(|_| error("responder timed out after 15 seconds".into()))?
}

// None is a missing or empty database, never a malformed or unrecognized one.
fn access_version(path: &std::path::Path) -> Result<Option<i64>> {
    match path.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(Error::InvalidConfig(format!(
                "{} must be a regular database file",
                path.display()
            )));
        }
        Ok(_) => {}
    }
    let inspect = || -> Result<Option<i64>> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let integrity: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(Error::InvalidConfig(integrity));
        }
        let objects: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        let user_version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if objects == 0 && user_version == 0 {
            return Ok(None);
        }
        let module: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='module')",
            [],
            |row| row.get(0),
        )?;
        if !module {
            let legacy: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name IN ('location_rule_sets','location_rules'))", [], |row| row.get(0))?;
            return Err(Error::InvalidConfig(if legacy {
                "unsupported or incomplete legacy ACL schema; original database preserved".into()
            } else {
                "unrecognized access database format; original database preserved".into()
            }));
        }
        let rows: i64 = conn.query_row("SELECT count(*) FROM module", [], |row| row.get(0))?;
        if rows != 1 {
            return Err(Error::InvalidConfig(
                "access module must contain exactly one row".into(),
            ));
        }
        let version: i64 = conn.query_row(
            "SELECT version FROM module WHERE module_name='access'",
            [],
            |row| row.get(0),
        )?;
        if !matches!(version, 0 | 1) {
            return Err(Error::InvalidConfig(format!(
                "unsupported access database version {version}; expected 0 or 1"
            )));
        }
        // Validate the library's required tables before allowing its migrations to write.
        for sql in [
            "SELECT id,name,subject_id,alias,class,grants,offers,requests,description,status,expired_after,updated_at,created_at FROM contacts LIMIT 0",
            "SELECT id,method,api,effect,grantee_type,grantee,updated_at,created_at FROM access_rules LIMIT 0",
            "SELECT id,request_id,visitor,visitor_sid,method,api,stage,reason,expired_after,updated_at,created_at FROM access_reviews LIMIT 0",
        ] {
            conn.prepare(sql)?;
        }
        if version == 1 {
            conn.prepare("SELECT application_id,applicant_name,applicant_sid,payload_hash,status,received_at,expired_after FROM contact_applications LIMIT 0")?;
        } else {
            let applications: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='contact_applications')",
                [],
                |row| row.get(0),
            )?;
            if applications {
                return Err(Error::InvalidConfig(
                    "access v0 already contains v1 tables".into(),
                ));
            }
        }
        Ok(Some(version))
    };
    inspect().map_err(|error| {
        Error::InvalidConfig(format!("access database {}: {error}", path.display()))
    })
}

fn snapshot_access(source: &std::path::Path, destination: &std::path::Path) -> Result<()> {
    let blank = match std::fs::metadata(destination) {
        Ok(metadata) => metadata.len() == 0,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
    };
    let conn =
        rusqlite::Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.backup(rusqlite::DatabaseName::Main, destination, None)?;
    let snapshot = rusqlite::Connection::open(destination)?;
    if blank {
        snapshot.pragma_update(None, "journal_mode", "DELETE")?;
    }
    let integrity: String = snapshot.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(Error::InvalidConfig(format!(
            "database snapshot failed: {integrity}"
        )));
    }
    snapshot
        .close()
        .map_err(|(_, error)| Error::ConfigDatabase(error))?;
    std::fs::File::open(destination)?.sync_all()?;
    Ok(())
}

fn legacy_access(path: &std::path::Path) -> Result<bool> {
    if !path.try_exists()? {
        return Ok(false);
    }
    if !path.symlink_metadata()?.file_type().is_file() {
        return Ok(false);
    }
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let tables = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if !tables.iter().any(|name| name == "location_rule_sets")
        || !tables.iter().any(|name| name == "location_rules")
        || tables.iter().any(|name| {
            !matches!(
                name.as_str(),
                "location_rule_sets" | "location_rules" | "migration"
            )
        })
    {
        return Ok(false);
    }
    // Recognize the published configuration schema, not merely similarly named tables.
    for sql in [
        "SELECT id,pattern,created_at,updated_at FROM location_rule_sets LIMIT 0",
        "SELECT id,location_id,action,exprs,created_at,updated_at FROM location_rules LIMIT 0",
    ] {
        if conn.prepare(sql).is_err() {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn load_access(
    profile: &IdentityProfile,
    subject: &access_control::SubjectId,
) -> Result<Arc<access_control::AccessService>> {
    let path = profile.access_db_path();
    let legacy = legacy_access(&path).map_err(|error| {
        Error::InvalidConfig(format!("access database {}: {error}", path.display()))
    })?;
    let version = if legacy { None } else { access_version(&path)? };
    let load = || async {
        let uri = format!("sqlite://{}?mode=rw", path.display());
        access_control::AccessService::load_from_db(&uri, profile.name(), subject)
            .await
            .map(Arc::new)
            .map_err(|error| {
                Error::InvalidConfig(format!("access database {}: {error}", path.display()))
            })
    };
    if version == Some(1) {
        return load().await;
    }
    // Nothing partially initialized is ever published at the application's database path.
    let staging = tempfile::Builder::new()
        .prefix(".access-init-")
        .tempdir_in(profile.db_dir())?;
    let working = tempfile::NamedTempFile::new_in(staging.path())?;
    let uri = format!("sqlite://{}?mode=rw", working.path().display());
    let access = access_control::AccessService::load_from_db(&uri, profile.name(), subject).await?;
    access
        .set_policy(
            access_control::Method::Specified(http::Method::POST),
            "/std/contact",
            access_control::Effect::Allow,
            access_control::Grantee::Named,
        )
        .await?;
    // Discovery is public before a contact relationship exists. The profile
    // GET prefix covers the public profile, avatar and introduction context.
    for (method, path) in [
        (http::Method::GET, "/std/acl/capabilities"),
        (http::Method::GET, "/std/profile"),
        (http::Method::HEAD, "/std/profile/context"),
        // The public #/intro page and its assets are served by Workspace.
        (http::Method::GET, "/std/workspace"),
        (http::Method::HEAD, "/std/workspace"),
    ] {
        access
            .set_policy(
                access_control::Method::Specified(method),
                path,
                access_control::Effect::Allow,
                access_control::Grantee::All,
            )
            .await?;
    }
    let ready = tempfile::NamedTempFile::new_in(staging.path())?;
    snapshot_access(working.path(), ready.path())?;
    if access_version(ready.path())? != Some(1) {
        return Err(Error::InvalidConfig(
            "initialized access database failed version validation".into(),
        ));
    }
    // Old configuration is archived, never translated or imported into the new database.
    if legacy || version == Some(0) {
        let prefix = if legacy {
            "access-legacy-backup-"
        } else {
            "access-v0-backup-"
        };
        let backup = tempfile::Builder::new()
            .prefix(prefix)
            .suffix(".db")
            .tempfile_in(profile.db_dir())?;
        snapshot_access(&path, backup.path())?;
        let (_, backup_path) = backup.keep().map_err(|error| error.error)?;
        tracing::info!(
            database = %path.display(),
            backup = %backup_path.display(),
            "access database backed up"
        );
        // SQLite performs the replacement as a database transaction, including
        // existing journals/WAL, rather than renaming only the main database file.
        if legacy_access(&path)? != legacy || (!legacy && access_version(&path)? != version) {
            return Err(Error::InvalidConfig(format!(
                "{} changed during initialization; backup retained",
                path.display()
            )));
        }
        snapshot_access(ready.path(), &path)?;
        #[cfg(unix)]
        std::fs::File::open(profile.db_dir())?.sync_all()?;
        return load().await;
    }
    // A simultaneous initializer must not overwrite a completed database or user rules.
    if access_version(&path)? != version {
        return load().await;
    }
    if path.try_exists()? {
        // Restore through SQLite when recovering an empty file, so any existing
        // journal/WAL is handled by SQLite rather than replacing only the main file.
        snapshot_access(ready.path(), &path)?;
    } else {
        match std::fs::hard_link(ready.path(), &path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return load().await,
            Err(error) => return Err(error.into()),
        }
    }
    #[cfg(unix)]
    std::fs::File::open(profile.db_dir())?.sync_all()?;
    load().await
}

fn native_router(server: &Server) -> Router {
    let endpoint = server.endpoint.clone();
    let local_router = Arc::downgrade(&server.router);
    let capability_profile = server.profile.clone();
    Router::new()
        .route("/std/acl/capabilities", axum::routing::get(move || {
            let profile = capability_profile.clone();
            async move {
                let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
                    let path = profile.config_db_path();
                    if !path.symlink_metadata()?.is_file() {
                        return Err(Error::InvalidConfig("config.db must be a regular file".into()));
                    }
                    let mut conn = rusqlite::Connection::open_with_flags(path,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                    conn.busy_timeout(std::time::Duration::from_secs(3))?;
                    let tx = conn.transaction()?;
                    let ready: bool = tx.query_row("SELECT count(*)=2 FROM sqlite_master WHERE type='table' AND name IN ('lib_capabilities','lib_capability_operations')",[],|r|r.get(0))?;
                    if !ready { return Err(Error::InvalidConfig("capability registration missing; install/reinstall Libs using normal installation".into())); }
                    let mut values = Vec::new();
                    for row in tx.prepare("SELECT c.lib_id,c.capability_id,c.title,l.version FROM lib_capabilities c JOIN libs l ON l.id=c.lib_id ORDER BY c.lib_id||'#'||c.capability_id")?
                        .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))? {
                        let (lib,id,title,version)=row?;
                        let endpoints = tx.prepare("SELECT method,path FROM lib_capability_operations WHERE lib_id=?1 AND capability_id=?2 ORDER BY method,path")?
                            .query_map(rusqlite::params![lib,id],|r|Ok(serde_json::json!({"method":r.get::<_,String>(0)?,"path":r.get::<_,String>(1)?})))?
                            .collect::<std::result::Result<Vec<_>,_>>()?;
                        values.push(serde_json::json!({"id":format!("{lib}#{id}"),"title":title,"version":version,"visibility":"contact","approval_mode":"capability","selectable":true,"endpoints":endpoints}));
                    }
                    Ok(values.into())
                }).await;
                let mut response = match result {
                    Ok(Ok(value)) => axum::Json(value).into_response(),
                    Ok(Err(error)) => (error.status(),error.to_string()).into_response(),
                    Err(error) => (http::StatusCode::INTERNAL_SERVER_ERROR,error.to_string()).into_response(),
                };
                response.headers_mut().insert(http::header::CACHE_CONTROL,http::HeaderValue::from_static("no-store"));
                response
            }
        }))
        .merge(access_router(server.access.clone()))
        .merge(crate::contact::contact_router(
            server.profile.clone(),
            server.endpoint.clone(),
            server.access.clone(),
        ))
        .merge(config_router(
            server.profile.clone(),
            server.endpoint.clone(),
        ))
        .merge(crate::sandbox::lib_management_router(
            server.profile.clone(),
            server.endpoint.clone(),
            server.sandbox.clone(),
        ))
        .merge(file_router(server.profile.join("file")))
        .merge(dhttp_router(server.endpoint.clone()))
        .fallback(any(move |mut request: Request<AxumBody>| {
            let endpoint = endpoint.clone();
            let local_router = local_router.clone();
            async move {
                if !request.uri().path().starts_with("/std/") {
                    return http::StatusCode::NOT_FOUND.into_response();
                }
                if request
                    .extensions()
                    .get::<dhttp::HandshakeSummary>()
                    .is_none()
                    && request
                        .extensions()
                        .get::<dhttp::LocalAuthority>()
                        .is_some()
                {
                    // CLI HTTP is a new authenticated owner request. Use the
                    // actual transport to obtain genuine caller facts; the CLI
                    // invocation itself continues to expose caller=None.
                    let query = request.uri().path_and_query().unwrap().as_str();
                    let mut parts = request.uri().clone().into_parts();
                    parts.path_and_query = Some(
                        format!("{DHTTP_PREFIX}{}{query}", endpoint.name())
                            .parse()
                            .unwrap(),
                    );
                    *request.uri_mut() = http::Uri::from_parts(parts).unwrap();
                    return forward_dhttp(endpoint, request).await;
                }
                let Some(router) = local_router.upgrade() else {
                    return http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                };
                let router = router.read().unwrap().clone();
                // Preserve the calling request's verified handshake across Libs.
                router.oneshot(request).await.expect("Router is infallible")
            }
        }))
}

fn current_router(server: &Server) -> Router {
    let proxies = server.config.proxy_locations.clone();
    let native = native_router(server);
    let outgoing = native.clone().layer(axum::middleware::from_fn_with_state(
        server.access.clone(),
        authorize,
    ));
    native
        .merge(Sandbox::api_router(
            server.sandbox.clone(),
            server.endpoint.clone(),
            server.access.clone(),
            outgoing,
        ))
        .fallback(any(move |request: Request<AxumBody>| {
            proxy_pass(proxies.clone(), request)
        }))
        .layer(axum::middleware::from_fn_with_state(
            server.access.clone(),
            authorize,
        ))
}

impl Server {
    pub(super) async fn load(profile: IdentityProfile, runtime: Arc<WasmRuntime>) -> Result<Self> {
        let certs = profile
            .load_certs()
            .await
            .map_err(|e| Error::InvalidIdentity(e.to_string()))?;
        match profile.load_ocsp().await {
            Ok(response)
                if qtls::validate_ocsp(&response, &certs, qtls::UnixTime::now()).is_ok() => {}
            _ => {
                let response = fetch_ocsp(&certs).await?;
                cache_ocsp(&profile, &response)?;
            }
        }
        let endpoint =
            dhttp::Endpoint::load_from(profile.path())
                .await
                .map_err(|error| match error {
                    dhttp::Error::InvalidName { .. }
                    | dhttp::Error::Home { .. }
                    | dhttp::Error::Credentials { .. } => Error::InvalidIdentity(error.to_string()),
                    error => Error::Dhttp(error),
                })?;
        initialize_directories(&profile)?;
        let config = load_server_config(&profile)?;
        if config.listen != 0 {
            dns::authority(&endpoint).map_err(|error| Error::InvalidIdentity(error.to_string()))?;
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
        let access = load_access(&profile, &subject).await?;
        let mut sandbox = Sandbox::new(runtime);
        sandbox.load_libs(&profile)?;
        let server = Self {
            profile,
            endpoint,
            config,
            access,
            router: Arc::new(RwLock::new(Router::new())),
            sandbox: Arc::new(RwLock::new(sandbox)),
            publisher,
            listener: None,
        };
        *server.router.write().unwrap() = current_router(&server);
        Ok(server)
    }

    pub(super) fn name(&self) -> &str {
        self.endpoint.name()
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
                        tracing::error!(identity = %name, %error, "request failed");
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
        self.listener.take();
        self.publisher.take();
        let tasks = {
            let mut sandbox = self.sandbox.write().unwrap();
            sandbox.close();
            sandbox.tasks.clone()
        };
        *self.router.write().unwrap() = axum::Router::new();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            tasks.wait().await;
            Ok(())
        })
        .await
        .map_err(|_| Error::ShutdownDeadline)?
    }
}

#[cfg(test)]
#[path = "../tests/unit/server/workspace_network.rs"]
mod network_tests;

#[cfg(test)]
#[path = "../tests/unit/server/initialization.rs"]
mod initialization_tests;

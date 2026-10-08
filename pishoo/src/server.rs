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
    Body, Error, Result,
    chat::{self, Chat, store::ChatStore},
    dns,
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
    publisher: Option<Arc<ddns::H3Resolver>>,
}

fn ocsp_ticks() -> tokio::time::Interval {
    let period = std::time::Duration::from_secs(3 * 24 * 60 * 60);
    let mut interval = tokio::time::interval_at(Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

pub async fn run() -> Result<()> {
    let home = DhttpHome::load(dhttp_home::HomeScope::User)
        .map_err(|e| Error::InvalidConfig(e.to_string()))?;
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
        for profile in home.discover_identity_profiles()? {
            if let Some(server) = load_profile(profile, runtime.clone()).await? {
                servers.insert(server.name().to_owned(), server);
            }
        }
        for server in servers.values().filter(|s| s.config.listen != 0) {
            tokio::spawn(server.listen().await?);
        }
        let mut ocsp_ticks = ocsp_ticks();
        let mut ocsp_updates = FuturesUnordered::new();
        let interrupt = tokio::signal::ctrl_c();
        tokio::pin!(interrupt);
        #[cfg(unix)]
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut removed_bounds = Vec::new();
        loop {
            let endpoints = servers.values()
                .filter(|s| s.config.listen & 1 != 0)
                .map(|s| s.endpoint.clone()).collect::<Vec<_>>();
            let mut next_due = match dns::maintain_mdns(&mdns, &endpoints, &removed_bounds).await {
                Ok(()) => None,
                Err(error) => {
                    eprintln!("DNS maintenance: {error}");
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
                            Ok(()) => eprintln!("OCSP refreshed identity {name}"),
                            Err(error) => eprintln!("OCSP refresh failed identity {name}: {error}"),
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
    // No new batches after the run body exits, including failure during startup.
    while jobs.next().await.is_some() {}
    let mut shutdown = Ok(());
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

async fn load_profile(
    profile: IdentityProfile,
    runtime: Arc<WasmRuntime>,
) -> Result<Option<Server>> {
    let name = profile.name().to_owned();
    match Server::load(profile, runtime).await {
        Ok(server) => Ok(Some(server)),
        Err(error @ Error::InvalidIdentity(_)) => {
            eprintln!("skipping identity {name}: {error}");
            Ok(None)
        }
        Err(error) => Err(error),
    }
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
    server
        .workspace
        .configure_outbound(Arc::new(endpoint.clone()))
        .await;
    server
        .chat
        .configure_outbound(Arc::new(endpoint.clone()))
        .await;
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

fn initialize_directories(profile: &IdentityProfile) -> Result<()> {
    for directory in ["db", "file", "lib", "logs", "repo", "templates"] {
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
            "/contact",
            access_control::Effect::Allow,
            access_control::Grantee::Named,
        )
        .await?;
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
        eprintln!(
            "access database {} backup: {}",
            path.display(),
            backup_path.display()
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

fn current_router(server: &Server) -> Router {
    let proxies = server.config.proxy_locations.clone();
    Router::new()
        .merge(access_router(server.access.clone()))
        .merge(config_router(
            server.profile.clone(),
            server.endpoint.clone(),
        ))
        .merge(workspace::router(server.workspace.clone()))
        .merge(chat_router(server.chat.clone(), server.workspace.clone()))
        .merge(server.sandbox.api_router(server.endpoint.clone()))
        .merge(file_router(server.profile.join("file")))
        .merge(dhttp_router(server.endpoint.clone()))
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
            dhttp::Endpoint::load(profile.name())
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
        let mut sandbox = Sandbox::new(runtime);
        sandbox.load_libs(&profile)?;
        let server = Self {
            profile,
            endpoint,
            config,
            access,
            workspace,
            chat,
            router: Arc::new(RwLock::new(Router::new())),
            sandbox,
            publisher,
        };
        *server.router.write().unwrap() = current_router(&server);
        server.chat.start_worker();
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
        *self.router.write().unwrap() = axum::Router::new();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            tokio::join!(self.workspace.shutdown(), self.chat.shutdown());
            self.sandbox.wait().await
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

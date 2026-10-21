use std::sync::{Arc, Mutex, RwLock};

use axum::{Router, body::Body as AxumBody, response::IntoResponse, routing::any};
use dhttp_home::identity::IdentityProfile;
use http::Request;
use tower::ServiceExt;

use crate::{
    Body, Error, Result, dns,
    routes::{
        DHTTP_PREFIX, authorize, capabilities_router, file_router, forward_dhttp, proxy_pass,
    },
    sandbox::{Sandbox, WasmRuntime},
    setup::{ServerConfig, config_router, load_server_config},
};

mod access;
mod files;
mod ocsp;
mod run;

pub use self::run::run;

use self::{
    access::load_access,
    files::{inherit_identity_owner, initialize_directories, repair_identity_ownership},
    ocsp::{cache_ocsp, fetch_ocsp},
};

struct Server {
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    config: ServerConfig,
    config_db: Mutex<rusqlite::Connection>,
    contact_db: Arc<Mutex<rusqlite::Connection>>,
    access: Arc<access_control::AccessService>,
    router: Arc<RwLock<axum::Router>>,
    sandbox: Arc<RwLock<Sandbox>>,
    publisher: Option<Arc<ddns::H3Resolver>>,
    listener: Option<dhttp::ListenFuture>,
}

#[cfg(test)]
#[path = "../tests/unit/server.rs"]
mod tests;

fn dhttp_router(endpoint: dhttp::Endpoint) -> Router {
    Router::new().route(
        &format!("{DHTTP_PREFIX}{{*path}}"),
        any(move |request: Request<AxumBody>| forward_dhttp(endpoint.clone(), request)),
    )
}

fn native_router(server: &Server) -> Router {
    let sandbox = server.sandbox.clone();
    let endpoint = server.endpoint.clone();
    let local_router = Arc::downgrade(&server.router);
    capabilities_router(server.profile.clone())
        .merge(crate::contact::contact_router(
            server.profile.clone(),
            server.endpoint.clone(),
            server.access.clone(),
        ))
        .merge(config_router(
            server.profile.clone(),
            server.endpoint.clone(),
        ))
        .merge(crate::package::pkg_management_router(
            server.profile.clone(),
            server.endpoint.clone(),
            server.sandbox.clone(),
        ))
        .merge(file_router(server.profile.join(".volume/sys/file/.file")))
        .merge(dhttp_router(server.endpoint.clone()))
        .fallback(any(move |mut request: Request<AxumBody>| {
            let endpoint = endpoint.clone();
            let local_router = local_router.clone();
            let sandbox = sandbox.clone();
            async move {
                let path = request.uri().path();
                if !path.starts_with("/std/")
                    && !sandbox.read().unwrap().libs.keys().any(|id| {
                        !id.starts_with("std.dhttp.net/")
                            && path
                                .strip_prefix(&format!("/{}", id.rsplit('/').next().unwrap()))
                                .is_some_and(|tail| tail.is_empty() || tail.starts_with('/'))
                    })
                {
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
    let api = Sandbox::api_router(
        server.sandbox.clone(),
        server.endpoint.clone(),
        server.access.clone(),
        outgoing,
    );
    let sandbox = server.sandbox.clone();
    native
        .merge(api.clone().reset_fallback())
        .fallback(any(move |request: Request<AxumBody>| {
            let api = api.clone();
            let proxies = proxies.clone();
            let name = request
                .uri()
                .path()
                .trim_start_matches('/')
                .split('/')
                .next()
                .unwrap_or("");
            let installed = {
                let sandbox = sandbox.read().unwrap();
                request.uri().path().strip_prefix("/app/").is_some_and(|tail| {
                    let app_name = tail.split('/').next().unwrap_or("");
                    sandbox.apps.keys().any(|id| id.rsplit('/').next() == Some(app_name))
                })
                    || sandbox.libs.keys().any(|id| {
                        !id.starts_with("std.dhttp.net/") && id.rsplit('/').next() == Some(name)
                    })
            };
            async move {
                if installed {
                    api.oneshot(request).await.expect("Router is infallible")
                } else {
                    proxy_pass(proxies, request).await
                }
            }
        }))
        .layer(axum::middleware::from_fn_with_state(
            server.access.clone(),
            authorize,
        ))
}

fn open_access_log(profile: &IdentityProfile) -> Result<std::fs::File> {
    let path = profile.join("log/access.log");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    let file = options.open(&path).map_err(|error| {
        Error::Io(std::io::Error::new(
            error.kind(),
            format!("{}: {error}", path.display()),
        ))
    })?;
    inherit_identity_owner(&file, profile)?;
    Ok(file)
}

async fn serve_request(
    router: Arc<RwLock<Router>>,
    request: Request<Body>,
    name: &str,
    access_log: &std::sync::Mutex<std::fs::File>,
) -> http::Response<AxumBody> {
    use std::io::Write;

    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let version = request.version();
    let peer = request
        .extensions()
        .get::<dhttp::HandshakeSummary>()
        .and_then(|handshake| handshake.remote.as_ref())
        .map(|authority| authority.name().to_owned());
    let app = router.read().unwrap().clone();
    let response = app
        .oneshot(request.map(axum::body::Body::new))
        .await
        .expect("Router is infallible");
    let status = response.status().as_u16();
    if response.status().is_server_error() {
        tracing::error!(identity = name, peer = peer.as_deref().unwrap_or("anonymous"),
            %method, ?path, status, "request failed");
    }
    let line = format!(
        "{} request server={name} peer={} version={version:?} method={method} path={path:?} status={status}\n",
        chrono::Utc::now().to_rfc3339(),
        peer.as_deref().unwrap_or("anonymous"),
    );
    // The listener owns the file; one lock keeps concurrent request lines intact.
    let result = access_log
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .write_all(line.as_bytes());
    if let Err(error) = result {
        tracing::error!(identity = name, %error, "cannot write access.log");
    }
    response
}

impl Server {
    pub(super) async fn load(profile: IdentityProfile, runtime: Arc<WasmRuntime>) -> Result<Self> {
        initialize_directories(&profile)?;
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
        let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(&certs)
            .map_err(|error| Error::InvalidIdentity(error.to_string()))?;
        let subject = access_control::SubjectId::new(ski.owner_hash().as_str().as_bytes())
            .map_err(|_| Error::InvalidIdentity("invalid certificate subject".into()))?;
        let access = load_access(&profile, &subject).await?;
        let config = load_server_config(&profile)?;
        if config.listen != "off" {
            dns::authority(&endpoint).map_err(|error| Error::InvalidIdentity(error.to_string()))?;
        }
        let publisher = if matches!(config.listen.as_str(), "external" | "all") {
            Some(dns::publisher(&endpoint)?)
        } else {
            None
        };
        let mut sandbox = Sandbox::new(runtime);
        sandbox.load_libs(&profile)?;
        let config_db = Mutex::new(crate::setup::open_config(
            &profile,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?);
        let contact_db = Arc::new(Mutex::new(
            crate::contact::database(&profile)
                .map_err(|(_, message)| Error::InvalidConfig(message))?,
        ));
        if rustix::process::geteuid().is_root() {
            let directory = std::fs::File::open(profile.path())?;
            repair_identity_ownership(&directory, &directory.metadata()?)?;
        }
        let server = Self {
            profile,
            endpoint,
            config,
            config_db,
            contact_db,
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
            self.config.listen.clone(),
        );
        let scopes = match listen.as_str() {
            "off" => return Err(Error::InvalidConfig("listen is disabled".into())),
            "loopback" => dhttp::Scope::Loopback.into(),
            "internal" => dhttp::Scope::Internal.into(),
            "external" => dhttp::Scope::External.into(),
            "all" => dhttp::Scopes::ALL,
            _ => return Err(Error::InvalidConfig("invalid listen mode".into())),
        };
        let access_log = Arc::new(std::sync::Mutex::new(open_access_log(&self.profile)?));
        let name = endpoint.name().to_owned();
        let service = tower::service_fn(move |request: http::Request<Body>| {
            let (router, name, access_log) = (router.clone(), name.clone(), access_log.clone());
            async move {
                let response = serve_request(router, request, &name, &access_log).await;
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

#[cfg(test)]
#[path = "../tests/unit/server/logging.rs"]
mod logging_tests;

#[cfg(test)]
#[path = "../tests/unit/review.rs"]
mod review_tests;

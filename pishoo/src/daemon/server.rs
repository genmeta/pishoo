use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use access_control::{AccessService, Effect, Grantee};
use axum::{Router, body::Body as AxumBody, response::IntoResponse, routing::any};
use dhttp_home::identity::IdentityProfile;
use http::Request;
use http_body_util::BodyExt;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use tokio_util::task::TaskTracker;
use tower::ServiceExt;

use super::Server;
use crate::{
    Body, Error, Result, exec,
    routes::build_router,
    sandbox::{Lib, Sandbox, WasmRuntime},
    setup::load_server_config,
};

#[cfg(test)]
#[path = "../../tests/unit/daemon/exec_route.rs"]
mod tests;

fn exec_router(
    enabled: bool,
    name: String,
    cwd: PathBuf,
    tasks: TaskTracker,
) -> Router {
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

async fn register_lib_apis(
    access: &AccessService,
    libs: &BTreeMap<String, Arc<Lib>>,
) -> Result<()> {
    for lib in libs.values() {
        if let Some(paths) = &lib.openapi.paths {
            for (path, item) in paths {
                let api = format!("/api/{}{}", lib.id, path);
                let api = api.trim_end_matches('/');
                for (method, _) in item.methods() {
                    let exists = access
                        .database()
                        .query_one_raw(Statement::from_sql_and_values(
                            DatabaseBackend::Sqlite,
                            "SELECT 1 FROM access_rules WHERE api = ? AND method IN (?, '*') LIMIT 1",
                            [api.into(), method.as_str().into()],
                        ))
                        .await?
                        .is_some();
                    if !exists {
                        access
                            .set_policy(
                                access_control::Method::Specified(method),
                                api,
                                Effect::Deny,
                                Grantee::All,
                            )
                            .await?;
                    }
                }
            }
        }
    }
    Ok(())
}

impl Server {
    pub(super) async fn load(profile: IdentityProfile, runtime: Arc<WasmRuntime>) -> Result<Self> {
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
        let exec_tasks = TaskTracker::new();
        let mut sandbox = Sandbox::new(runtime);
        let libs = sandbox.load_libs(&profile)?;
        let app_router = sandbox
            .api_router(endpoint.clone(), &libs)
            .merge(exec_router(
                config.ssh,
                endpoint.name().to_owned(),
                profile.path().to_path_buf(),
                exec_tasks.clone(),
            ));
        let router = build_router(
            endpoint.clone(),
            access.clone(),
            app_router,
            &config,
            &profile,
        )?;
        sandbox.verify_libs(&profile, &libs)?;
        register_lib_apis(&access, &libs).await?;
        sandbox.replace_libs(libs);
        Ok(Self {
            profile,
            endpoint,
            config,
            access,
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
        if config.listen != self.config.listen || config.ssh != self.config.ssh {
            return Err(Error::InvalidConfig(
                "listen and ssh changes require restarting the instance".into(),
            ));
        }
        let libs = self.sandbox.load_libs(&self.profile)?;
        let app_router = self
            .sandbox
            .api_router(self.endpoint.clone(), &libs)
            .merge(exec_router(
                config.ssh,
                self.endpoint.name().to_owned(),
                self.profile.path().to_path_buf(),
                self.exec_tasks.clone(),
            ));
        let router = build_router(
            self.endpoint.clone(),
            self.access.clone(),
            app_router,
            &config,
            &self.profile,
        )?;
        self.sandbox.verify_libs(&self.profile, &libs)?;
        register_lib_apis(&self.access, &libs).await?;
        *self.router.write().unwrap() = router;
        self.sandbox.replace_libs(libs);
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
            let service = tower::service_fn(move |mut request: http::Request<Body>| {
                let (router, name) = (router.clone(), name.clone());
                async move {
                    let result: Result<http::Response<Body>> = async {
                        let summary = request.extensions().get::<dhttp::HandshakeSummary>().ok_or(Error::MissingHandshake)?;
                        if summary.local.as_ref().is_none_or(|l| l.name() != name) { return Err(Error::IdentityMismatch); }
                        let identity = dhttp_identity::name::DhttpName::try_from(name.clone()).map_err(|_| Error::IdentityMismatch)?;
                        *request.uri_mut() = identity.expand_uri(request.uri().clone()).map_err(|_| Error::IdentityMismatch)?;
                        let authority = request.uri().authority().ok_or(Error::IdentityMismatch)?;
                        if authority.as_str().contains('@') || dhttp_home::normalize_name(authority.host()).as_deref() != Some(name.as_str()) { return Err(Error::IdentityMismatch); }
                        let names = request.headers().keys().filter(|n| n.as_str().starts_with("pishoo-")).cloned().collect::<Vec<_>>();
                        for name in names { request.headers_mut().remove(name); }
                        let app = router.read().unwrap().clone();
                        let response = app.oneshot(request.map(axum::body::Body::new)).await.expect("Router is infallible");
                        Ok(response.map(|b| b.map_err(Into::into).boxed_unsync()))
                    }.await;
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
                            .map(|b| b.map_err(Into::into).boxed_unsync())
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
        let result = if dhttp::DhttpNetwork::global().is_ok() {
            self.endpoint.stop_listening().map_err(Error::from)
        } else {
            Ok(())
        };
        *self.router.write().unwrap() = axum::Router::new();
        let cleanup = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let (sandbox, ()) = tokio::join!(self.sandbox.wait(), self.exec_tasks.wait());
            sandbox
        })
        .await
        .map_err(|_| Error::ShutdownDeadline)?;
        result.and(cleanup)
    }
}

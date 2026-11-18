//! Device administration is checked against transport identity on every request.
use std::sync::Arc;

use access_control::{SubjectId, Visitor};
use axum::{
    Router,
    body::Body,
    response::{IntoResponse, Response},
    routing::any,
};
use dhttp_home::DhttpHome;
use http::{Method, Request, StatusCode, header};
use rusqlite::Connection;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

use crate::{Error, Result};

mod storage;
mod users;
pub(crate) use storage::open;

pub(crate) async fn recover(home: &DhttpHome, db: &Arc<Mutex<Connection>>) -> Result<()> {
    users::recover(home, &mut *db.lock().await)
}

pub(crate) fn router(
    home: DhttpHome,
    db: Arc<Mutex<Connection>>,
    commands: mpsc::Sender<&'static str>,
) -> Router {
    let handler = move |request: Request<Body>| {
        let (home, db, commands) = (home.clone(), db.clone(), commands.clone());
        async move {
            let response = match handle(&home, &db, &commands, request).await {
                Ok(response) => response,
                Err(error) => {
                    if error.status().is_server_error() {
                        tracing::error!(%error, "system request failed");
                    }
                    (
                        error.status(),
                        axum::Json(json!({"error":error.to_string()})),
                    )
                        .into_response()
                }
            };
            let mut response = response;
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            response
        }
    };
    Router::new()
        .route("/sys", any(handler.clone()))
        .route("/sys/{*path}", any(handler))
}

/// Project device authority into the existing permission catalog for administrators.
pub(crate) fn permissions_router(
    profile: dhttp_home::identity::IdentityProfile,
    access: Arc<access_control::AccessService>,
    database: Arc<Mutex<Connection>>,
) -> Router {
    use tower::ServiceExt;

    let catalog = crate::routes::capabilities_router(profile).layer(
        axum::middleware::from_fn_with_state(access, crate::routes::authorize),
    );
    Router::new().route(
        "/std/acl/permissions",
        any(move |request: Request<Body>| {
            let (catalog, database) = (catalog.clone(), database.clone());
            async move {
                let result: Result<Response> = async {
                    if request.method() != Method::GET {
                        return Ok(catalog
                            .oneshot(request)
                            .await
                            .expect("Router is infallible"));
                    }
                    let details =
                        form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes())
                            .any(|(key, value)| key == "details" && value == "1");
                    let endpoint = request
                        .extensions()
                        .get::<dhttp::Endpoint>()
                        .ok_or(Error::MissingHandshake)?;
                    let (target, visitor) = caller(&request, endpoint)?;
                    let response = catalog
                        .oneshot(request)
                        .await
                        .expect("Router is infallible");
                    if !response.status().is_success() {
                        return Ok(response);
                    }
                    let Some(visitor) = visitor else {
                        return Ok(response);
                    };
                    let group = {
                        let db = database.lock().await;
                        if !storage::administrator(&db, &target)?
                            || !storage::administrator(&db, &visitor)?
                        {
                            return Ok(response);
                        }
                        let endpoints = json!({
                            "/sys/version": ["GET"],
                            "/sys/info": ["GET"],
                            "/sys/admin": ["GET", "POST", "DELETE"],
                            "/sys/user": ["GET", "POST", "DELETE"],
                            "/sys/reload": ["POST"],
                            "/sys/restart": ["POST"]
                        });
                        if details {
                            let members = db
                                .prepare("SELECT name FROM administrator ORDER BY name")?
                                .query_map([], |row| row.get::<_, String>(0))?
                                .collect::<std::result::Result<Vec<_>, _>>()?;
                            json!({"title":{"en":"System administration","zh-CN":"系统管理"},
                            "endpoints":endpoints,"management":"administrator","members":members})
                        } else {
                            endpoints
                        }
                    };
                    let (mut parts, body) = response.into_parts();
                    let bytes = axum::body::to_bytes(body, usize::MAX)
                        .await
                        .map_err(|error| Error::InvalidConfig(error.to_string()))?;
                    let mut value: Value = serde_json::from_slice(&bytes)
                        .map_err(|error| Error::InvalidConfig(error.to_string()))?;
                    value
                        .as_object_mut()
                        .ok_or_else(|| Error::InvalidConfig("invalid permission catalog".into()))?
                        .insert("pishoo · 系统管理 / System administration".into(), group);
                    parts.headers.remove(header::CONTENT_LENGTH);
                    Ok(Response::from_parts(parts, Body::from(value.to_string())))
                }
                .await;
                let mut response = result
                    .unwrap_or_else(|error| (error.status(), error.to_string()).into_response());
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
                response
            }
        }),
    )
}

pub(crate) async fn register_host(
    database: &Arc<Mutex<Connection>>,
    profile: &dhttp_home::identity::IdentityProfile,
    endpoint: &dhttp::Endpoint,
) -> Result<()> {
    if profile.name() != endpoint.name() {
        return Err(Error::IdentityMismatch);
    }
    let local = endpoint.local_authority()?;
    let owner = identity(local.name(), local.certificates())?;
    let mut db = database.lock().await;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    storage::register_first_administrator(&tx, &owner)?;
    tx.commit()?;
    Ok(())
}

fn identity(name: &str, certificates: &[qtls::CertificateDer<'_>]) -> Result<Visitor> {
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(certificates)
        .map_err(|_| Error::Denied)?;
    Ok(Visitor::new(
        name,
        SubjectId::new(ski.owner_hash().as_str().as_bytes()).map_err(|_| Error::Denied)?,
    ))
}

fn caller(
    request: &Request<Body>,
    endpoint: &dhttp::Endpoint,
) -> Result<(Visitor, Option<Visitor>)> {
    let local = endpoint.local_authority()?;
    let target = identity(local.name(), local.certificates())?;
    if let Some(handshake) = request.extensions().get::<dhttp::HandshakeSummary>() {
        let local = handshake.local.as_ref().ok_or(Error::MissingHandshake)?;
        if identity(local.name(), local.certificates())? != target {
            return Err(Error::IdentityMismatch);
        }
        let visitor = handshake
            .remote
            .as_ref()
            .map(|remote| identity(remote.name(), remote.certificates()))
            .transpose()?;
        return Ok((target, visitor));
    }
    if let Some(local) = request.extensions().get::<dhttp::LocalAuthority>() {
        let visitor = identity(local.name(), local.certificates())?;
        if visitor != target {
            return Err(Error::Denied);
        }
        return Ok((target, Some(visitor)));
    }
    Err(Error::MissingHandshake)
}

fn required<'a>(body: &'a Value, field: &str) -> Result<&'a str> {
    body.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::BadRequest(format!("{field} must be a nonempty string")))
}

fn allow(methods: &'static str) -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, methods)]).into_response()
}

async fn handle(
    home: &DhttpHome,
    database: &Arc<Mutex<Connection>>,
    commands: &mpsc::Sender<&'static str>,
    request: Request<Body>,
) -> Result<Response> {
    let endpoint = request
        .extensions()
        .get::<dhttp::Endpoint>()
        .ok_or(Error::MissingHandshake)?
        .clone();
    let (target, visitor) = caller(&request, &endpoint)?;
    let visitor = visitor.ok_or(Error::Denied)?;
    // Recheck after accepting the body so revocation is visible to mutations.
    storage::authorize(&*database.lock().await, &target, &visitor)?;
    let path = request.uri().path().to_owned();
    let builtin = matches!(
        path.as_str(),
        "/sys/version" | "/sys/info" | "/sys/admin" | "/sys/user" | "/sys/reload" | "/sys/restart"
    );
    if !builtin {
        return Err(Error::RouteNotFound);
    }
    let method = request.method().clone();
    let query = request.uri().query().map(str::to_owned);
    let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|_| {
            Error::BadRequest("system request exceeds 1 MiB or body could not be read".into())
        })?;
    if query.is_some()
        && !matches!(
            (&method, path.as_str()),
            (&Method::DELETE, "/sys/admin" | "/sys/user")
        )
    {
        return Err(Error::BadRequest("unexpected query".into()));
    }
    if method != Method::POST && !bytes.is_empty() {
        return Err(Error::BadRequest("unexpected request body".into()));
    }
    let mut db = database.lock().await;
    storage::authorize(&db, &target, &visitor)?;
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).map_err(|_| Error::BadRequest("invalid JSON".into()))?
    };
    let value = match path.as_str() {
        "/sys/version" => {
            if method != Method::GET {
                return Ok(allow("GET"));
            }
            json!({"version":env!("CARGO_PKG_VERSION")})
        }
        "/sys/info" => {
            if method != Method::GET {
                return Ok(allow("GET"));
            }
            json!({"version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"identity":target.name()})
        }
        "/sys/admin" => match method {
            Method::GET => {
                let rows = db
                    .prepare("SELECT name,subject_id FROM administrator ORDER BY name")?
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Value::Array(rows.into_iter().map(|(name,subject)| json!({"name":name,"subject_id":String::from_utf8_lossy(&subject)})).collect())
            }
            Method::POST => {
                let object = body
                    .as_object()
                    .ok_or_else(|| Error::BadRequest("admin requires a JSON object".into()))?;
                if object
                    .keys()
                    .any(|k| !matches!(k.as_str(), "name" | "subject_id"))
                {
                    return Err(Error::BadRequest("unknown administrator field".into()));
                }
                let name = storage::canonical(required(&body, "name")?)?;
                let subject = required(&body, "subject_id")?;
                if subject.len() != 64
                    || !subject
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(Error::BadRequest(
                        "subject_id must be a lowercase certificate owner hash".into(),
                    ));
                }
                let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                storage::authorize(&tx, &target, &visitor)?;
                storage::put_admin(&tx, &name, subject.as_bytes())?;
                tx.commit()?;
                json!({"name":name})
            }
            Method::DELETE => {
                let name = delete_name(query.as_deref())?;
                db.execute("DELETE FROM administrator WHERE name=?1", [&name])?;
                json!({"name":name,"deleted":true})
            }
            _ => return Ok(allow("GET, POST, DELETE")),
        },
        "/sys/user" => match method {
            Method::GET => {
                let mut rows = db
                    .prepare("SELECT name,phase FROM managed_user ORDER BY name")?
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<std::result::Result<std::collections::BTreeMap<_, _>, _>>()?;
                for profile in crate::server::local_profiles(home)? {
                    rows.entry(profile.name().to_owned())
                        .or_insert_with(|| "active".into());
                }
                Value::Array(
                    rows.into_iter()
                        .map(|(name, phase)| json!({"name":name,"phase":phase}))
                        .collect(),
                )
            }
            Method::POST => {
                users::create(home, &mut db, body, dhttp::trust::DHTTP_ROOT_CA_DER).await?
            }
            Method::DELETE => users::delete(home, &mut db, &delete_name(query.as_deref())?).await?,
            _ => return Ok(allow("GET, POST, DELETE")),
        },
        "/sys/reload" | "/sys/restart" => {
            if method != Method::POST {
                return Ok(allow("POST"));
            }
            if !bytes.is_empty() {
                return Err(Error::BadRequest(
                    "control request must have an empty body".into(),
                ));
            }
            let command = if path.ends_with("/reload") {
                "reload"
            } else {
                "restart"
            };
            let permit =
                commands
                    .clone()
                    .try_reserve_owned()
                    .map_err(|_| Error::PackagePolicy {
                        code: "system_control_unavailable",
                        status: StatusCode::SERVICE_UNAVAILABLE,
                    })?;
            let bytes =
                bytes::Bytes::from(json!({"status":"accepted","command":command}).to_string());
            // Hand the response to the transport before requesting process control.
            let body = Body::from_stream(async_stream::stream! {
                yield Ok::<_, std::convert::Infallible>(bytes);
                permit.send(command);
            });
            return Ok((
                StatusCode::ACCEPTED,
                [(header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response());
        }
        _ => return Err(Error::RouteNotFound),
    };
    Ok(axum::Json(value).into_response())
}

fn delete_name(query: Option<&str>) -> Result<String> {
    let query = query.ok_or_else(|| Error::BadRequest("name query is required".into()))?;
    let fields = form_urlencoded::parse(query.as_bytes()).collect::<Vec<_>>();
    if fields.len() != 1 || fields[0].0 != "name" {
        return Err(Error::BadRequest(
            "exactly one name query is required".into(),
        ));
    }
    storage::canonical(&fields[0].1)
}

#[cfg(test)]
#[path = "../tests/unit/system/tests.rs"]
mod tests;

//! Read, validate and manage identity service configuration.
use std::{collections::HashSet, net::SocketAddr};

use access_control::{SubjectId, Visitor};
use axum::{
    Json, Router,
    body::Body,
    response::{IntoResponse, Response},
    routing::any,
};
use dhttp_home::identity::IdentityProfile;
use http::{Method, Request, StatusCode, header};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, types::ValueRef};
use serde_json::{Value, json};

use crate::{Error, Result, routes::reserved};

#[derive(Clone, Debug)]
pub(crate) struct ServerConfig {
    pub(crate) listen: u8,
    pub(crate) exec: bool,
    pub(crate) proxy_locations: Vec<ProxyLocation>,
}
#[derive(Debug)]
pub(crate) struct ProxyLocation {
    pub(crate) location: String,
    pub(crate) proxy_pass: http::uri::Parts,
}
impl Clone for ProxyLocation {
    fn clone(&self) -> Self {
        let mut proxy_pass = http::uri::Parts::default();
        proxy_pass.scheme = self.proxy_pass.scheme.clone();
        proxy_pass.authority = self.proxy_pass.authority.clone();
        proxy_pass.path_and_query = self.proxy_pass.path_and_query.clone();
        Self {
            location: self.location.clone(),
            proxy_pass,
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/setup.rs"]
mod tests;

pub(crate) fn load_server_config(profile: &IdentityProfile) -> Result<ServerConfig> {
    initialize_config(profile)
        .and_then(|()| read_server_config(profile))
        .map_err(|error| {
            Error::InvalidConfig(format!(
                "configuration database {}: {error}",
                profile.config_db_path().display()
            ))
        })
}

fn read_server_config(profile: &IdentityProfile) -> Result<ServerConfig> {
    let mut conn = open_config(profile, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let tx = conn.transaction()?;
    let config = read_config(&tx)?;
    tx.commit()?;
    Ok(config)
}

fn initialize_config(profile: &IdentityProfile) -> Result<()> {
    let directory = profile.db_dir();
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&directory)?;
    if !directory.symlink_metadata()?.file_type().is_dir() {
        return Err(Error::InvalidConfig(format!(
            "{} must be a directory, not a symlink",
            directory.display()
        )));
    }
    let path = profile.config_db_path();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let conn = open_config(profile, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(Error::InvalidConfig(integrity));
    }
    let empty = conn.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM sqlite_master WHERE name NOT LIKE 'sqlite_%') AND (SELECT user_version FROM pragma_user_version) = 0",
        [], |row| row.get::<_, bool>(0),
    )?;
    drop(conn);
    if !empty {
        return Ok(());
    }
    let mut conn = open_config(profile, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Recheck under SQLite's write lock: another startup may have initialized it.
    let empty = tx.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM sqlite_master WHERE name NOT LIKE 'sqlite_%') AND (SELECT user_version FROM pragma_user_version) = 0",
        [], |row| row.get::<_, bool>(0),
    )?;
    if empty {
        tx.execute_batch(
            "CREATE TABLE settings (
                listen INTEGER NOT NULL CHECK(typeof(listen) = 'integer' AND listen BETWEEN 0 AND 3),
                exec INTEGER NOT NULL CHECK(typeof(exec) = 'integer' AND exec IN (0,1))
             );
             INSERT INTO settings(listen,exec) VALUES(3,0);
             CREATE TABLE proxy_locations(location TEXT NOT NULL PRIMARY KEY, proxy_pass TEXT NOT NULL);
             PRAGMA user_version=1;",
        )?;
    }
    read_config(&tx)?;
    tx.commit()?;
    Ok(())
}

fn open_config(profile: &IdentityProfile, flags: OpenFlags) -> Result<Connection> {
    let db = profile.config_db_path();
    if !db.symlink_metadata()?.file_type().is_file() {
        return Err(Error::InvalidConfig(
            "config.db must be a regular file".into(),
        ));
    }
    let conn = Connection::open_with_flags(db, flags)?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    Ok(conn)
}

fn read_config(conn: &Connection) -> Result<ServerConfig> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 1 {
        return Err(Error::InvalidConfig(format!(
            "unsupported schema {version}"
        )));
    }
    let mut stmt = conn.prepare("SELECT listen, exec FROM settings")?;
    let mut rows = stmt.query([])?;
    let Some(row) = rows.next()? else {
        return Err(Error::InvalidConfig(
            "settings must contain exactly one row".into(),
        ));
    };
    let listen = match row.get_ref(0)? {
        ValueRef::Integer(v @ 0..=3) => v as u8,
        _ => {
            return Err(Error::InvalidConfig(
                "listen must be an integer in 0..=3".into(),
            ));
        }
    };
    let exec = match row.get_ref(1)? {
        ValueRef::Integer(0) => false,
        ValueRef::Integer(1) => true,
        _ => return Err(Error::InvalidConfig("exec must be 0 or 1".into())),
    };
    if rows.next()?.is_some() {
        return Err(Error::InvalidConfig(
            "settings must contain exactly one row".into(),
        ));
    }
    drop(rows);
    drop(stmt);
    let mut stmt =
        conn.prepare("SELECT location, proxy_pass FROM proxy_locations ORDER BY location")?;
    let values = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ServerConfig {
        listen,
        exec,
        proxy_locations: parse_proxies(values)?,
    })
}

fn parse_proxies(values: Vec<(String, String)>) -> Result<Vec<ProxyLocation>> {
    let mut proxy_locations = Vec::new();
    let mut seen = HashSet::new();
    for (location, upstream) in values {
        let path = location.strip_prefix("= ").unwrap_or(&location);
        if !valid_path(path) || !seen.insert(location.clone()) || reserved(path) {
            return Err(Error::InvalidConfig(format!(
                "invalid or reserved proxy location: {location}"
            )));
        }
        let bare_address = upstream.parse::<SocketAddr>().ok();
        let uri = bare_address
            .map(|address| format!("http://{address}"))
            .unwrap_or_else(|| upstream.clone());
        let proxy_pass: http::Uri = uri
            .parse()
            .map_err(|_| Error::InvalidConfig("invalid proxy URI".into()))?;
        let address = proxy_pass
            .authority()
            .and_then(|authority| authority.as_str().parse::<SocketAddr>().ok());
        if proxy_pass.scheme_str() != Some("http")
            || address.is_none_or(|address| !address.ip().is_loopback() || address.port() == 0)
            || upstream.contains(['?', '#', '$'])
        {
            return Err(Error::InvalidConfig(
                "proxy_pass must identify a local HTTP/TCP endpoint".into(),
            ));
        }
        let mut proxy_pass = proxy_pass.into_parts();
        if bare_address.is_some()
            || uri
                .split_once("://")
                .is_some_and(|(_, rest)| !rest.contains('/'))
        {
            proxy_pass.path_and_query = None;
        }
        proxy_locations.push(ProxyLocation {
            location,
            proxy_pass,
        });
    }
    proxy_locations.sort_by(|a, b| a.location.cmp(&b.location));
    Ok(proxy_locations)
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains("//")
        && !path.contains(['?', '#', '%', '\\'])
        && !path.split('/').any(|p| p == "." || p == "..")
        && !path.chars().any(|c| c.is_whitespace() || c.is_control())
}

pub(crate) fn config_router(profile: IdentityProfile, endpoint: dhttp::Endpoint) -> Router {
    let handler = move |request: Request<Body>| {
        let (profile, endpoint) = (profile.clone(), endpoint.clone());
        async move {
            let mut response = config_request(profile, endpoint, request)
                .await
                .unwrap_or_else(|error| {
                    let status = error.status();
                    if status.is_server_error() {
                        tracing::error!(%error, "configuration API failed");
                        (status, "configuration storage failed").into_response()
                    } else {
                        (status, error.to_string()).into_response()
                    }
                });
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            response
                .headers_mut()
                .insert("supported-versions", "v1".parse().unwrap());
            response
        }
    };
    Router::new()
        .route("/sys/settings", any(handler.clone()))
        .route("/sys/proxies", any(handler))
}

async fn config_request(
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    request: Request<Body>,
) -> Result<Response> {
    // authorize removes supplied Visitors and constructs this one from the TLS peer.
    let visitor = request.extensions().get::<Visitor>().ok_or(Error::Denied)?;
    let local = endpoint.local_authority()?;
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
        .map_err(|_| Error::Denied)?;
    let subject =
        SubjectId::new(ski.owner_hash().as_str().as_bytes()).map_err(|_| Error::Denied)?;
    if visitor.name() != endpoint.name() || visitor.subject_id() != &subject {
        return Err(Error::Denied);
    }

    if request.headers().contains_key("accept-versions") {
        let mut supported = false;
        for versions in request.headers().get_all("accept-versions") {
            let versions = versions
                .to_str()
                .map_err(|_| Error::BadRequest("invalid Accept-Versions".into()))?;
            supported |= versions.split(',').any(|version| version.trim() == "v1");
        }
        if !supported {
            return Ok(StatusCode::HTTP_VERSION_NOT_SUPPORTED.into_response());
        }
    }

    let settings = request.uri().path() == "/sys/settings";
    let method = request.method().clone();
    if method != Method::GET && method != if settings { Method::PATCH } else { Method::PUT } {
        return Ok((
            StatusCode::METHOD_NOT_ALLOWED,
            [(
                header::ALLOW,
                if settings { "GET, PATCH" } else { "GET, PUT" },
            )],
        )
            .into_response());
    }
    let payload = if method == Method::GET {
        None
    } else {
        if !request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
        {
            return Ok(StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response());
        }
        let bytes = match axum::body::to_bytes(request.into_body(), 64 * 1024).await {
            Ok(bytes) => bytes,
            Err(error) => {
                use std::error::Error as _;
                if error
                    .source()
                    .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
                {
                    return Ok(StatusCode::PAYLOAD_TOO_LARGE.into_response());
                }
                return Err(Error::BadRequest("failed to read JSON body".into()));
            }
        };
        Some(
            serde_json::from_slice::<Value>(&bytes)
                .map_err(|_| Error::BadRequest("invalid JSON".into()))?,
        )
    };
    let value =
        tokio::task::spawn_blocking(move || config_database(&profile, settings, payload)).await??;
    Ok(Json(value).into_response())
}

fn config_database(
    profile: &IdentityProfile,
    settings: bool,
    payload: Option<Value>,
) -> Result<Value> {
    let config = match payload {
        None => read_server_config(profile)?,
        Some(payload) => {
            // Validate the complete input before taking a write transaction.
            let proxies = if settings {
                validate_settings_patch(&payload)?;
                None
            } else {
                Some(parse_proxy_json(&payload)?)
            };
            let mut conn = open_config(profile, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
            // An immediate transaction serializes read/modify/write, preserving omitted fields.
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut config = read_config(&tx)?;
            if let Some(proxies) = proxies {
                tx.execute("DELETE FROM proxy_locations", [])?;
                for proxy in proxies {
                    tx.execute(
                        "INSERT INTO proxy_locations(location, proxy_pass) VALUES(?1, ?2)",
                        rusqlite::params![proxy.location, upstream_text(&proxy)],
                    )?;
                }
            } else {
                if let Some(listen) = payload.get("listen") {
                    config.listen = listen.as_u64().unwrap() as u8;
                }
                if let Some(exec) = payload.get("exec") {
                    config.exec = exec.as_bool().unwrap();
                }
                tx.execute(
                    "UPDATE settings SET listen=?1, exec=?2",
                    rusqlite::params![config.listen, config.exec],
                )?;
            }
            // Revalidate persisted values before commit, including database constraints/triggers.
            let config = read_config(&tx)?;
            tx.commit()?;
            config
        }
    };
    if settings {
        Ok(json!({"listen": config.listen, "exec": config.exec}))
    } else {
        Ok(Value::Array(
            config
                .proxy_locations
                .iter()
                .map(
                    |proxy| json!({"location": proxy.location, "proxy_pass": upstream_text(proxy)}),
                )
                .collect(),
        ))
    }
}

fn validate_settings_patch(payload: &Value) -> Result<()> {
    let object = payload
        .as_object()
        .filter(|object| !object.is_empty())
        .ok_or_else(|| Error::BadRequest("settings PATCH requires a nonempty object".into()))?;
    for (key, value) in object {
        let valid = match key.as_str() {
            "listen" => value.as_u64().is_some_and(|value| value <= 3),
            "exec" => value.is_boolean(),
            _ => false,
        };
        if !valid {
            return Err(Error::BadRequest(format!("invalid settings field: {key}")));
        }
    }
    Ok(())
}

fn parse_proxy_json(payload: &Value) -> Result<Vec<ProxyLocation>> {
    let values = payload
        .as_array()
        .ok_or_else(|| Error::BadRequest("proxies PUT requires an array".into()))?
        .iter()
        .map(|value| {
            let object = value
                .as_object()
                .filter(|object| object.len() == 2)
                .ok_or_else(|| Error::BadRequest("invalid proxy fields".into()))?;
            let location = object
                .get("location")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::BadRequest("proxy location must be a string".into()))?;
            let upstream = object
                .get("proxy_pass")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::BadRequest("proxy_pass must be a string".into()))?;
            Ok((location.to_owned(), upstream.to_owned()))
        })
        .collect::<Result<Vec<_>>>()?;
    parse_proxies(values).map_err(|error| Error::BadRequest(error.to_string()))
}

fn upstream_text(proxy: &ProxyLocation) -> String {
    format!(
        "{}://{}{}",
        proxy.proxy_pass.scheme.as_ref().unwrap(),
        proxy.proxy_pass.authority.as_ref().unwrap(),
        proxy
            .proxy_pass
            .path_and_query
            .as_ref()
            .map_or("", |path| path.as_str()),
    )
}

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
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use serde_json::{Value, json};

use crate::{Error, Result, routes::reserved};

#[derive(Clone, Debug)]
pub(crate) struct ServerConfig {
    pub(crate) listen: String,
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
mod tests;

pub(crate) fn load_server_config(profile: &IdentityProfile) -> Result<ServerConfig> {
    initialize_config(profile)
        .and_then(|()| {
            let mut conn = open_config(profile, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            read_server_config(&mut conn)
        })
        .map_err(|error| {
            Error::InvalidConfig(format!(
                "configuration database {}: {error}",
                profile.join(".volume/sys/setting/database").display()
            ))
        })
}

// Runtime scans read one snapshot without startup initialization or integrity checks.
pub(crate) fn read_server_config(conn: &mut Connection) -> Result<ServerConfig> {
    let tx = conn.transaction()?;
    let config = read_config(&tx)?;
    check_proxy_mounts(&tx, &config.proxy_locations)?;
    tx.commit()?;
    Ok(config)
}

fn initialize_config(profile: &IdentityProfile) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::create_dir_all(profile.path())?;
    if !profile.path().symlink_metadata()?.file_type().is_dir() {
        return Err(Error::InvalidConfig(
            "identity must be a real directory".into(),
        ));
    }
    let existing = profile.join(".volume/sys/setting/database");
    if existing.try_exists()? {
        if !existing.symlink_metadata()?.file_type().is_file() {
            return Err(Error::InvalidConfig(
                "configuration must be a regular file".into(),
            ));
        }
        let previous = Connection::open_with_flags(&existing, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let version: i64 = previous.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let occupied: bool = previous.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name NOT LIKE 'sqlite_%')",
            [],
            |r| r.get(0),
        )?;
        if matches!(version, 1 | 2) {
            // Old settings are not migration input. Preserve the whole directory,
            // including SQLite sidecars, and initialize the current schema afresh.
            drop(previous);
            let lock = std::fs::File::from(
                rustix::fs::open(
                    profile.path(),
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )
                .map_err(std::io::Error::from)?,
            );
            rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)
                .map_err(std::io::Error::from)?;
            // Another initializer may already have archived this database.
            if existing.try_exists()? {
                let current =
                    Connection::open_with_flags(&existing, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                let current_version: i64 =
                    current.pragma_query_value(None, "user_version", |r| r.get(0))?;
                drop(current);
                if matches!(current_version, 1 | 2) {
                    let parent = existing.parent().unwrap();
                    for path in [
                        profile.join(".volume"),
                        profile.join(".volume/sys"),
                        parent.to_path_buf(),
                    ] {
                        if !path.symlink_metadata()?.file_type().is_dir() {
                            return Err(Error::InvalidConfig(
                                "configuration directory must be a real directory".into(),
                            ));
                        }
                    }
                    let archive = tempfile::Builder::new()
                        .prefix(&format!(".setting-v{current_version}-"))
                        .tempdir_in(parent.parent().unwrap())?;
                    if rustix::process::geteuid().is_root() {
                        use std::os::unix::fs::MetadataExt;
                        let owner = profile.path().metadata()?;
                        rustix::fs::fchown(
                            std::fs::File::open(archive.path())?,
                            Some(rustix::fs::Uid::from_raw(owner.uid())),
                            Some(rustix::fs::Gid::from_raw(owner.gid())),
                        )
                        .map_err(std::io::Error::from)?;
                    }
                    std::fs::rename(parent, archive.path().join("setting"))?;
                    let archive = archive.keep();
                    tracing::warn!(identity = profile.name(), path = %archive.display(), "Old configuration archived; creating fresh v3 defaults");
                } else if current_version != 0 && current_version != 3 {
                    return Err(Error::InvalidConfig(format!(
                        "unsupported configuration schema {current_version}; existing data preserved"
                    )));
                }
            }
        } else if version != 3 && (version != 0 || occupied) {
            return Err(Error::InvalidConfig(format!(
                "unsupported configuration schema {version}; existing data preserved"
            )));
        }
    }
    for name in [
        ".volume",
        ".volume/sys",
        ".volume/sys/setting",
        ".volume/sys/pkg",
        ".volume/sys/access",
        ".volume/sys/contact",
    ] {
        let path = profile.join(name);
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        if !path.symlink_metadata()?.file_type().is_dir() {
            return Err(Error::InvalidConfig(
                "volume must contain real directories".into(),
            ));
        }
    }
    for name in ["setting", "pkg", "access"] {
        let path = profile.join(format!(".volume/sys/{name}/database"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut conn = open_config(profile, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(Error::InvalidConfig(check));
    }
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version == 3 {
        for (schema, table) in [
            ("main", "setting"),
            ("main", "proxy_location"),
            ("pkg_registry", "package"),
            ("pkg_registry", "package_dependency"),
            ("acl", "lib_capability"),
            ("acl", "lib_capability_operation"),
        ] {
            if !conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {schema}.sqlite_master WHERE type='table' AND name=?1)"),[table],|r|r.get::<_,bool>(0))? {
                return Err(Error::InvalidConfig("incomplete package registry schema".into()));
            }
        }
        return Ok(());
    }
    if version != 0 {
        return Err(Error::InvalidConfig(
            "unsupported configuration schema; existing data preserved".into(),
        ));
    }
    for schema in ["main", "pkg_registry", "acl"] {
        conn.execute_batch(&format!(
            "PRAGMA {schema}.journal_mode=DELETE; PRAGMA {schema}.synchronous=FULL;"
        ))?;
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS setting(key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS proxy_location(location TEXT PRIMARY KEY NOT NULL,proxy_pass TEXT NOT NULL);")?;
    tx.execute_batch(include_str!("package/sql/schema-v3.sql"))?;
    tx.pragma_update(None, "user_version", 3)?;
    tx.commit()?;
    inherit_config_files(profile)?;
    Ok(())
}

pub(crate) fn open_config(profile: &IdentityProfile, flags: OpenFlags) -> Result<Connection> {
    let db = profile.join(".volume/sys/setting/database");
    if !db.symlink_metadata()?.file_type().is_file() {
        return Err(Error::InvalidConfig(
            "configuration must be a regular file".into(),
        ));
    }
    let conn = Connection::open_with_flags(&db, flags)?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != 3 && !(version == 0 && flags.contains(OpenFlags::SQLITE_OPEN_READ_WRITE)) {
        return Err(Error::InvalidConfig(format!(
            "unsupported configuration schema {version}; reinitialize the identity service (existing data preserved)"
        )));
    }

    conn.pragma_update(None, "foreign_keys", true)?;
    for (schema, name) in [("pkg_registry", "pkg"), ("acl", "access")] {
        let path = profile.join(format!(".volume/sys/{name}/database"));
        if !path.symlink_metadata()?.file_type().is_file() {
            return Err(Error::InvalidConfig(
                "attached database must be a regular file".into(),
            ));
        }
        // URI mode prevents reads from creating a missing attached database.
        let mode = if flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY) {
            "ro"
        } else {
            "rw"
        };
        let uri = format!(
            "file:{}?mode={mode}",
            url::Url::from_file_path(path)
                .map_err(|_| Error::InvalidConfig("invalid database path".into()))?
                .path()
        );
        conn.execute(&format!("ATTACH DATABASE ?1 AS {schema}"), [uri])?;
    }
    if conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_list WHERE schema='pkg_registry' AND name='package')", [], |r|r.get::<_,bool>(0))? {
        conn.execute_batch("CREATE TEMP VIEW lib AS SELECT * FROM pkg_registry.package WHERE has_lib=1;
            CREATE TEMP VIEW app AS SELECT * FROM pkg_registry.package WHERE has_app=1;")?;
    }
    Ok(conn)
}

fn inherit_config_files(profile: &IdentityProfile) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    use rustix::fs::{Mode, OFlags};
    if !rustix::process::geteuid().is_root() {
        return Ok(());
    }
    let owner = profile.path().symlink_metadata()?;
    for name in ["setting", "pkg", "access"] {
        let directory = std::fs::File::from(
            rustix::fs::open(
                profile.join(format!(".volume/sys/{name}")),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?,
        );
        rustix::fs::fchown(
            &directory,
            Some(rustix::fs::Uid::from_raw(owner.uid())),
            Some(rustix::fs::Gid::from_raw(owner.gid())),
        )
        .map_err(std::io::Error::from)?;
        for file in [
            "database",
            "database-wal",
            "database-shm",
            "database-journal",
        ] {
            let file = match rustix::fs::openat(
                &directory,
                file,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(file) => std::fs::File::from(file),
                Err(rustix::io::Errno::NOENT) => continue,
                Err(error) => return Err(std::io::Error::from(error).into()),
            };
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.nlink() != 1 {
                return Err(Error::InvalidConfig(
                    "database files must be single-link regular files".into(),
                ));
            }
            rustix::fs::fchown(
                &file,
                Some(rustix::fs::Uid::from_raw(owner.uid())),
                Some(rustix::fs::Gid::from_raw(owner.gid())),
            )
            .map_err(std::io::Error::from)?;
            file.set_permissions(metadata.permissions())?;
        }
    }
    Ok(())
}

fn read_config(conn: &Connection) -> Result<ServerConfig> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 3 {
        return Err(Error::InvalidConfig(format!(
            "unsupported schema {version}; reinitialize the identity service (existing data preserved)"
        )));
    }
    use rusqlite::OptionalExtension;
    let listen: Option<String> = conn
        .query_row("SELECT value FROM setting WHERE key='listen'", [], |r| {
            r.get(0)
        })
        .optional()?;
    let listen = listen.unwrap_or_else(|| "off".into());
    if listen != "off" && !valid_listen(&listen) {
        return Err(Error::InvalidConfig("invalid listen mode".into()));
    }
    if listen == "off"
        && conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM setting WHERE key='listen')",
            [],
            |r| r.get::<_, bool>(0),
        )?
    {
        return Err(Error::InvalidConfig(
            "disabled listening must have no KV record".into(),
        ));
    }
    let values = conn
        .prepare("SELECT location,proxy_pass FROM proxy_location ORDER BY location")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ServerConfig {
        listen,
        proxy_locations: parse_proxies(values)?,
    })
}

// /std is reserved; App and ordinary Lib mounts are checked independently.
fn check_proxy_mounts(conn: &Connection, proxies: &[ProxyLocation]) -> Result<()> {
    if proxies.iter().any(|proxy| {
        crate::routes::reserved(proxy.location.strip_prefix("= ").unwrap_or(&proxy.location))
    }) {
        return Err(Error::InvalidConfig("proxy cannot occupy /std".into()));
    }
    let has_apps: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_list WHERE type IN ('table','view') AND name='app')",
        [],
        |row| row.get(0),
    )?;
    if has_apps {
        let mut names = conn.prepare("SELECT name FROM app ORDER BY name")?;
        for name in names.query_map([], |row| row.get::<_, String>(0))? {
            let mount = format!("/app/{}", name?);
            if let Some(proxy) = proxies.iter().find(|proxy| {
                let path = proxy.location.strip_prefix("= ").unwrap_or(&proxy.location);
                path == mount || path.starts_with(&format!("{mount}/"))
            }) {
                return Err(Error::InvalidConfig(format!(
                    "proxy {} conflicts with installed App {mount}",
                    proxy.location
                )));
            }
        }
    }
    let has_libs: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_list WHERE type IN ('table','view') AND name='lib')",
        [],
        |row| row.get(0),
    )?;
    if has_libs {
        let has_author: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('lib') WHERE name='author')",
            [],
            |row| row.get(0),
        )?;
        let mut names = conn.prepare(if has_author {
            "SELECT name FROM lib WHERE author IS NULL OR author<>'std.dhttp.net' ORDER BY name"
        } else {
            "SELECT name FROM lib WHERE substr(id,1,14)<>'std.dhttp.net/' ORDER BY name"
        })?;
        for name in names.query_map([], |row| row.get::<_, String>(0))? {
            let mount = format!("/{}", name?);
            if let Some(proxy) = proxies.iter().find(|proxy| {
                let path = proxy.location.strip_prefix("= ").unwrap_or(&proxy.location);
                path == mount || path.starts_with(&format!("{mount}/"))
            }) {
                return Err(Error::InvalidConfig(format!(
                    "proxy {} conflicts with installed Lib {mount}",
                    proxy.location
                )));
            }
        }
    }
    Ok(())
}

fn upgrade_settings(conn: &Connection) -> Result<()> {
    let config = read_config(conn)?;
    check_proxy_mounts(conn, &config.proxy_locations)
}

fn valid_listen(value: &str) -> bool {
    matches!(value, "loopback" | "internal" | "external" | "all")
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
                    let status = match &error {
                        Error::Io(io) if io.kind() == std::io::ErrorKind::InvalidInput => {
                            StatusCode::CONFLICT
                        }
                        _ => error.status(),
                    };
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
        .route("/std/sys/listen", any(handler.clone()))
        .route("/std/sys/mount", any(handler))
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

    let method = request.method().clone();
    let uri = request.uri().clone();
    if !matches!(method, Method::GET | Method::PATCH | Method::DELETE) {
        return Ok((
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, PATCH, DELETE")],
        )
            .into_response());
    }
    let reading = method == Method::GET || method == Method::DELETE;
    if !reading
        && !request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        return Ok(StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response());
    }
    let bytes = match axum::body::to_bytes(request.into_body(), if reading { 0 } else { 64 * 1024 })
        .await
    {
        Ok(bytes) => bytes,
        Err(error) => {
            use std::error::Error as _;
            if error
                .source()
                .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
            {
                return Ok(if reading {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::PAYLOAD_TOO_LARGE
                }
                .into_response());
            }
            return Err(Error::BadRequest("failed to read request body".into()));
        }
    };
    let payload = if reading {
        None
    } else {
        Some(
            serde_json::from_slice::<Value>(&bytes)
                .map_err(|_| Error::BadRequest("invalid JSON".into()))?,
        )
    };
    let deleting = method == Method::DELETE;
    let value =
        tokio::task::spawn_blocking(move || config_database(&profile, &method, &uri, payload))
            .await??;
    Ok(if deleting {
        StatusCode::NO_CONTENT.into_response()
    } else {
        Json(value).into_response()
    })
}

pub(crate) fn config_database(
    profile: &IdentityProfile,
    method: &Method,
    uri: &http::Uri,
    payload: Option<Value>,
) -> Result<Value> {
    if !matches!(uri.path(), "/std/sys/listen" | "/std/sys/mount") {
        return Err(Error::RouteNotFound);
    }
    let deletion = if let Some(query) = uri.query() {
        if uri.path() != "/std/sys/mount" || *method != Method::DELETE {
            return Err(Error::BadRequest("unsupported query parameters".into()));
        }
        let params = form_urlencoded::parse(query.as_bytes()).collect::<Vec<_>>();
        if params.len() != 2
            || params.iter().filter(|(key, _)| key == "location").count() != 1
            || params.iter().filter(|(key, _)| key == "proxy_pass").count() != 1
        {
            return Err(Error::BadRequest(
                "unmount requires exactly location and proxy_pass".into(),
            ));
        }
        let location = params
            .iter()
            .find(|(key, _)| key == "location")
            .unwrap()
            .1
            .to_string();
        let upstream = params
            .iter()
            .find(|(key, _)| key == "proxy_pass")
            .unwrap()
            .1
            .to_string();
        Some(parse_proxy_json(&json!([{"location":location,"proxy_pass":upstream}]))?.remove(0))
    } else {
        None
    };
    if uri.path() == "/std/sys/mount" && *method == Method::DELETE && deletion.is_none() {
        return Err(Error::BadRequest(
            "unmount requires location and proxy_pass".into(),
        ));
    }
    let config = if *method == Method::GET {
        if payload.is_some() {
            return Err(Error::BadRequest("GET does not accept a body".into()));
        }
        let mut conn = open_config(profile, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        read_server_config(&mut conn)?
    } else {
        let proxy = match *method {
            Method::PATCH if uri.path() == "/std/sys/listen" => {
                validate_listen_patch(
                    payload
                        .as_ref()
                        .ok_or_else(|| Error::BadRequest("missing configuration".into()))?,
                )?;
                None
            }
            Method::PATCH => Some(
                parse_proxy_json(&Value::Array(vec![
                    payload
                        .clone()
                        .ok_or_else(|| Error::BadRequest("missing configuration".into()))?,
                ]))?
                .remove(0),
            ),
            Method::DELETE if payload.is_none() => None,
            _ => return Err(Error::MethodNotAllowed),
        };
        initialize_config(profile)?;
        let mut conn = open_config(profile, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        upgrade_settings(&tx)?;
        if *method == Method::DELETE {
            if let Some(rule) = deletion {
                let current = read_config(&tx)?;
                if let Some(existing) = current
                    .proxy_locations
                    .iter()
                    .find(|existing| existing.location == rule.location)
                {
                    if upstream_text(existing) != upstream_text(&rule) {
                        return Err(Error::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "mount upstream changed",
                        )));
                    }
                    tx.execute(
                        "DELETE FROM proxy_location WHERE location=?1",
                        [&rule.location],
                    )?;
                }
            } else {
                tx.execute("DELETE FROM setting WHERE key='listen'", [])?;
            }
        } else if let Some(rule) = proxy {
            tx.execute(
                "DELETE FROM proxy_location WHERE location=?1",
                [&rule.location],
            )?;
            tx.execute(
                "INSERT INTO proxy_location(location,proxy_pass) VALUES(?1,?2)",
                rusqlite::params![rule.location, upstream_text(&rule)],
            )?;
        } else if payload.as_ref().unwrap()["listen"].as_str() == Some("off") {
            tx.execute("DELETE FROM setting WHERE key='listen'", [])?;
        } else {
            tx.execute("INSERT INTO setting(key,value) VALUES('listen',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [payload.as_ref().unwrap()["listen"].as_str().unwrap()])?;
        }
        let config = read_config(&tx)?;
        check_proxy_mounts(&tx, &config.proxy_locations)?;
        tx.commit()?;
        inherit_config_files(profile)?;
        config
    };
    if *method == Method::DELETE {
        return Ok(Value::Null);
    }
    let forwards = config
        .proxy_locations
        .iter()
        .map(|proxy| json!({"location":proxy.location,"proxy_pass":upstream_text(proxy)}))
        .collect::<Vec<_>>();
    Ok(if uri.path() == "/std/sys/listen" {
        json!({"listen":config.listen})
    } else {
        json!(forwards)
    })
}

fn validate_listen_patch(payload: &Value) -> Result<()> {
    let object = payload
        .as_object()
        .filter(|object| object.len() == 1)
        .ok_or_else(|| {
            Error::BadRequest("listen PATCH requires exactly one listen field".into())
        })?;
    for (key, value) in object {
        let valid = match key.as_str() {
            "listen" => value
                .as_str()
                .is_some_and(|mode| matches!(mode, "internal" | "external" | "all" | "off")),
            _ => false,
        };
        if !valid {
            return Err(Error::BadRequest(format!("invalid listen field: {key}")));
        }
    }
    Ok(())
}

fn parse_proxy_json(payload: &Value) -> Result<Vec<ProxyLocation>> {
    let values = payload
        .as_array()
        .ok_or_else(|| Error::BadRequest("mount requires an array".into()))?
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

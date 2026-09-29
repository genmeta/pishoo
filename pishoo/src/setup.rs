//! Read and validate identity service configuration.
use std::{collections::HashSet, net::SocketAddr};

use dhttp_home::identity::IdentityProfile;
use rusqlite::{Connection, OpenFlags, types::ValueRef};

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
    let db = profile.config_db_path();
    if !db.symlink_metadata()?.file_type().is_file() {
        return Err(Error::InvalidConfig(
            "config.db must be a regular file".into(),
        ));
    }
    let mut conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    let tx = conn.transaction()?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 1 {
        return Err(Error::InvalidConfig(format!(
            "unsupported schema {version}"
        )));
    }
    let mut stmt = tx.prepare("SELECT listen, exec FROM settings")?;
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
        tx.prepare("SELECT location, proxy_pass FROM proxy_locations ORDER BY location")?;
    let values = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut proxy_locations = Vec::new();
    let mut seen = HashSet::new();
    for (location, upstream) in values {
        let path = location.strip_prefix("= ").unwrap_or(&location);
        if !valid_path(path)
            || !seen.insert(location.clone())
            || reserved(path, &["/api", "/.pishoo", "/exec", "/file"])
        {
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
    drop(stmt);
    tx.commit()?;
    Ok(ServerConfig {
        listen,
        exec,
        proxy_locations,
    })
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains("//")
        && !path.contains(['?', '#', '%', '\\'])
        && !path.split('/').any(|p| p == "." || p == "..")
        && !path.chars().any(|c| c.is_whitespace() || c.is_control())
}

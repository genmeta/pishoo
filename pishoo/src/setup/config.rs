//! Read and validate one server's SQLite configuration snapshot.

use dhttp_home::identity::IdentityProfile;
use rusqlite::{Connection, OpenFlags, types::ValueRef};

use super::{Result, invalid};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyLocation {
    pub location: String,
    pub proxy_pass: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub listen: u8,
    pub proxy_locations: Vec<ProxyLocation>,
}

pub fn load_server_config(profile: &IdentityProfile) -> Result<ServerConfig> {
    let db = profile.db_dir().join("config.db");
    if !db.is_file() || db.symlink_metadata()?.file_type().is_symlink() {
        return Err(invalid("config.db missing or not a regular file").into());
    }
    let mut conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    let tx = conn.transaction()?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 1 {
        return Err(invalid(format!("unsupported config.db schema version {version}")).into());
    }
    let mut stmt = tx.prepare("SELECT listen FROM settings")?;
    let mut rows = stmt.query([])?;
    let Some(row) = rows.next()? else {
        return Err(invalid("settings must contain exactly one row").into());
    };
    let listen = match row.get_ref(0)? {
        ValueRef::Integer(value @ 0..=3) => value as u8,
        _ => return Err(invalid("listen must be an integer from 0 to 3").into()),
    };
    if rows.next()?.is_some() {
        return Err(invalid("settings must contain exactly one row").into());
    }
    drop(rows);
    drop(stmt);
    let mut stmt =
        tx.prepare("SELECT location, proxy_pass FROM proxy_locations ORDER BY location")?;
    let locations = stmt
        .query_map([], |row| {
            Ok(ProxyLocation {
                location: row.get(0)?,
                proxy_pass: row.get(1)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for item in &locations {
        if !valid_location(&item.location) || !valid_proxy_pass(&item.proxy_pass) {
            return Err(invalid("invalid proxy location or upstream").into());
        }
    }
    drop(stmt);
    tx.commit()?;
    Ok(ServerConfig {
        listen,
        proxy_locations: locations,
    })
}

fn valid_location(location: &str) -> bool {
    let path = location.strip_prefix("= ").unwrap_or(location);
    path.starts_with('/')
        && !path.contains("//")
        && !path.contains(['?', '#', '%', '\\'])
        && !path.split('/').any(|part| part == "." || part == "..")
        && !path.chars().any(char::is_whitespace)
        && (location.starts_with('/') || location.starts_with("= /"))
}

fn valid_proxy_pass(value: &str) -> bool {
    let Ok(uri) = value.parse::<http::Uri>() else {
        return false;
    };
    let Some(authority) = uri.authority() else {
        return false;
    };
    matches!(uri.scheme_str(), Some("http" | "https"))
        && !authority.as_str().contains(['@', '$'])
        && !value.contains(['?', '#', '$'])
}

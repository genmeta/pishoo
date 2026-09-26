//! Validate configuration and the exact component snapshot to be compiled.
use std::collections::HashSet;

use dhttp_home::identity::IdentityProfile;
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use wasmparser::{Encoding, Parser, Payload};

use crate::{Error, Result};

#[derive(Clone, Debug)]
pub(crate) struct ServerConfig {
    pub(crate) listen: u8,
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

pub(crate) fn load_server_config(profile: &IdentityProfile) -> Result<ServerConfig> {
    let db = profile.db_dir().join("config.db");
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
    let mut stmt = tx.prepare("SELECT listen FROM settings")?;
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
        if !valid_path(path) || !seen.insert(location.clone()) || reserved(path) {
            return Err(Error::InvalidConfig(format!(
                "invalid or reserved proxy location: {location}"
            )));
        }
        let proxy_pass: http::Uri = upstream
            .parse()
            .map_err(|_| Error::InvalidConfig("invalid proxy URI".into()))?;
        let name = dhttp_identity::name::DhttpName::try_from(profile.name().to_owned())
            .map_err(|e| Error::InvalidConfig(e.to_string()))?;
        let proxy_pass = name
            .expand_uri(proxy_pass)
            .map_err(|e| Error::InvalidConfig(e.to_string()))?;
        let host = proxy_pass.host().unwrap_or_default();
        let canonical = dhttp_home::normalize_name(host);
        if !matches!(proxy_pass.scheme_str(), Some("http" | "https"))
            || !host.ends_with(".dhttp.net")
            || canonical.is_none()
            || proxy_pass
                .authority()
                .is_none_or(|a| a.as_str().contains('@'))
            || upstream.contains(['?', '#', '$'])
        {
            return Err(Error::InvalidConfig(
                "proxy_pass must identify a DHTTP endpoint".into(),
            ));
        }
        let mut proxy_pass = proxy_pass.into_parts();
        if upstream
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
        proxy_locations,
    })
}

pub(crate) fn network_config(servers: &[ServerConfig]) -> Result<Option<dhttp::NetworkConfig>> {
    if servers.iter().any(|s| s.listen > 3) {
        return Err(Error::InvalidConfig("invalid listen scope".into()));
    }
    let bits = servers.iter().fold(0, |bits, s| bits | s.listen);
    let scopes = match bits {
        0 => return Ok(None),
        1 => dhttp::Scope::Internal.into(),
        2 => dhttp::Scope::External.into(),
        _ => dhttp::Scope::Internal | dhttp::Scope::External,
    };
    Ok(Some(dhttp::NetworkConfig {
        listen: vec![dhttp::ListenConfig::Scope(scopes)],
    }))
}

pub fn validate_lib(bytes: &[u8]) -> Result<oas3::OpenApiV3Spec> {
    let invalid = |s: &str| Error::InvalidComponent(s.into());
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(invalid("component exceeds 64 MiB"));
    }
    let mut document = None;
    let mut depth = 0;
    let mut component = false;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|e| Error::InvalidComponent(e.to_string()))? {
            Payload::Version {
                encoding: Encoding::Component,
                ..
            } if depth == 0 => component = true,
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => depth += 1,
            Payload::End(_) if depth > 0 => depth -= 1,
            Payload::CustomSection(section) if depth == 0 && section.name() == "pishoo:openapi" => {
                if document.is_some() {
                    return Err(invalid("duplicate pishoo:openapi section"));
                }
                if section.data().len() > 1024 * 1024 {
                    return Err(invalid("OpenAPI exceeds 1 MiB"));
                }
                document = Some(section.data());
            }
            _ => {}
        }
    }
    if !component {
        return Err(invalid("lib.wasm must be a component"));
    }
    let document = document.ok_or_else(|| invalid("missing pishoo:openapi section"))?;
    // Parsing first bounds recursion and validates JSON before the duplicate-key walk.
    let value: serde_json::Value =
        serde_json::from_slice(document).map_err(|e| Error::InvalidComponent(e.to_string()))?;
    check_json_keys(document, &mut 0)?;
    check_refs(&value)?;
    let openapi: oas3::OpenApiV3Spec =
        serde_json::from_value(value).map_err(|e| Error::InvalidComponent(e.to_string()))?;
    if !openapi.openapi.starts_with("3.1.") {
        return Err(invalid("only OpenAPI 3.1.x is supported"));
    }
    let paths = openapi
        .paths
        .as_ref()
        .ok_or_else(|| invalid("missing OpenAPI paths"))?;
    let mut ids = HashSet::new();
    for (path, item) in paths {
        if !valid_path(path) || path.contains(['{', '}']) || reserved(path) {
            return Err(invalid("unsupported or reserved API path"));
        }
        if item.reference.is_some() {
            return Err(invalid("path-level references are unsupported"));
        }
        for (_, operation) in item.methods() {
            if let Some(id) = &operation.operation_id {
                if !ids.insert(id) {
                    return Err(invalid("duplicate operationId"));
                }
            }
        }
    }
    Ok(openapi)
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains("//")
        && !path.contains(['?', '#', '%', '\\'])
        && !path.split('/').any(|p| p == "." || p == "..")
        && !path.chars().any(|c| c.is_whitespace() || c.is_control())
}
fn reserved(path: &str) -> bool {
    [
        "/api",
        "/contact",
        "/contacts",
        "/acl",
        "/workspace",
        "/workspace-api",
        "/.pishoo",
        "/shell",
    ]
    .iter()
    .any(|prefix| {
        path == *prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}
fn check_refs(value: &serde_json::Value) -> Result<()> {
    match value {
        serde_json::Value::Object(values) => {
            if values
                .get("$ref")
                .is_some_and(|v| v.as_str().is_none_or(|s| !s.starts_with("#/")))
            {
                return Err(Error::InvalidComponent(
                    "external references are unsupported".into(),
                ));
            }
            for value in values.values() {
                check_refs(value)?;
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                check_refs(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}
// This walks already validated JSON. Key strings are decoded before comparison.
fn check_json_keys(bytes: &[u8], index: &mut usize) -> Result<()> {
    skip_space(bytes, index);
    match bytes[*index] {
        b'{' => {
            *index += 1;
            let mut keys = HashSet::new();
            loop {
                skip_space(bytes, index);
                if bytes[*index] == b'}' {
                    *index += 1;
                    break;
                }
                let start = *index;
                skip_string(bytes, index);
                let key: String =
                    serde_json::from_slice(&bytes[start..*index]).expect("validated JSON string");
                if !keys.insert(key) {
                    return Err(Error::InvalidComponent("duplicate JSON key".into()));
                }
                skip_space(bytes, index);
                *index += 1; // colon
                check_json_keys(bytes, index)?;
                skip_space(bytes, index);
                if bytes[*index] == b',' {
                    *index += 1;
                }
            }
        }
        b'[' => {
            *index += 1;
            loop {
                skip_space(bytes, index);
                if bytes[*index] == b']' {
                    *index += 1;
                    break;
                }
                check_json_keys(bytes, index)?;
                skip_space(bytes, index);
                if bytes[*index] == b',' {
                    *index += 1;
                }
            }
        }
        b'"' => skip_string(bytes, index),
        _ => {
            while *index < bytes.len()
                && !matches!(
                    bytes[*index],
                    b',' | b']' | b'}' | b' ' | b'\n' | b'\r' | b'\t'
                )
            {
                *index += 1;
            }
        }
    }
    Ok(())
}
fn skip_space(bytes: &[u8], index: &mut usize) {
    while *index < bytes.len() && bytes[*index].is_ascii_whitespace() {
        *index += 1;
    }
}
fn skip_string(bytes: &[u8], index: &mut usize) {
    *index += 1;
    while bytes[*index] != b'"' {
        if bytes[*index] == b'\\' {
            *index += 1;
        }
        *index += 1;
    }
    *index += 1;
}

#[cfg(test)]
mod tests;

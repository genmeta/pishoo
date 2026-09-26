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

pub(crate) async fn proxy(
    endpoint: dhttp::Endpoint,
    route: ProxyLocation,
    mut request: Request<Body>,
) -> Result<http::Response<Body>> {
    let original_host = request.uri().authority().map(|a| a.as_str().to_owned());
    let original_scheme = request.uri().scheme_str().unwrap_or("https").to_owned();
    *request.uri_mut() = proxy_uri(&route, request.uri())?;
    clean_hop_headers(request.headers_mut());
    let protocol = request.extensions_mut().remove::<Arc<str>>();
    request.extensions_mut().clear();
    if let Some(protocol) = protocol {
        request.extensions_mut().insert(protocol);
    }
    request.headers_mut().remove(header::HOST);
    let host = request
        .uri()
        .authority()
        .expect("validated proxy authority")
        .as_str()
        .parse()
        .map_err(|_| Error::BadRequest("invalid authority".into()))?;
    request.headers_mut().insert(header::HOST, host);
    for name in [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
    ] {
        request.headers_mut().remove(name);
    }
    if let Some(host) = original_host {
        request.headers_mut().insert(
            "x-forwarded-host",
            host.parse()
                .map_err(|_| Error::BadRequest("invalid forwarding authority".into()))?,
        );
    }
    request.headers_mut().insert(
        "x-forwarded-proto",
        original_scheme
            .parse()
            .map_err(|_| Error::BadRequest("invalid scheme".into()))?,
    );
    let mut response = endpoint.from_request(request).await?;
    clean_hop_headers(response.headers_mut());
    Ok(response)
}

fn proxy_uri(route: &ProxyLocation, uri: &http::Uri) -> Result<http::Uri> {
    let mut parts = http::uri::Parts::default();
    parts.scheme = route.proxy_pass.scheme.clone();
    parts.authority = route.proxy_pass.authority.clone();
    parts.path_and_query = route.proxy_pass.path_and_query.clone();
    let path = if let Some(base) = &parts.path_and_query {
        let suffix = if route.location.starts_with("= ") {
            ""
        } else {
            uri.path()
                .strip_prefix(&route.location)
                .ok_or(Error::RouteNotFound)?
        };
        format!("{}{suffix}", base.path())
    } else {
        uri.path().to_owned()
    };
    let path = match uri.query() {
        Some(q) => format!("{path}?{q}"),
        None => path,
    };
    parts.path_and_query = Some(
        path.parse()
            .map_err(|_| Error::BadRequest("invalid upstream path".into()))?,
    );
    http::Uri::from_parts(parts).map_err(|_| Error::BadRequest("invalid upstream URI".into()))
}
fn clean_hop_headers(headers: &mut http::HeaderMap) {
    let named = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|s| s.split(','))
        .filter_map(|s| s.trim().parse::<http::HeaderName>().ok())
        .collect::<Vec<_>>();
    for name in named {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
    let reserved = headers
        .keys()
        .filter(|n| n.as_str().starts_with("pishoo-"))
        .cloned()
        .collect::<Vec<_>>();
    for name in reserved {
        headers.remove(name);
    }
}

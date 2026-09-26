pub(crate) fn build_router(
    endpoint: dhttp::Endpoint,
    access: Arc<AccessService>,
    libs: &BTreeMap<String, Arc<Lib>>,
    config: &ServerConfig,
    profile: &dhttp_home::identity::IdentityProfile,
    lib_slots: Arc<tokio::sync::Semaphore>,
    tasks: TaskTracker,
) -> Result<Router> {
    let libs = libs.clone();
    let proxies = config.proxy_locations.clone();
    let root = profile.join("file");
    let app = management_router(access.clone(), profile.name(), endpoint.name())
        .fallback(any(move |request: Request<AxumBody>| {
            let (endpoint, libs, proxies, root, slots, tasks) = (
                endpoint.clone(),
                libs.clone(),
                proxies.clone(),
                root.clone(),
                lib_slots.clone(),
                tasks.clone(),
            );
            async move {
                let result: Result<Response> = async {
                    let path = request.uri().path();
                    if path == "/api" || path.starts_with("/api/") {
                        let tail = path.strip_prefix("/api/").ok_or(Error::RouteNotFound)?;
                        let (id, suffix) = tail
                            .split_once('/')
                            .map_or((tail, "/".to_string()), |(id, p)| (id, format!("/{p}")));
                        let lib = libs.get(id).ok_or(Error::RouteNotFound)?.clone();
                        if lib.id != id {
                            return Err(Error::RouteNotFound);
                        }
                        if lib.cancel.is_cancelled() {
                            return Err(Error::Cancelled);
                        }
                        let item = lib
                            .openapi
                            .paths
                            .as_ref()
                            .and_then(|p| p.get(&suffix))
                            .ok_or(Error::RouteNotFound)?;
                        if !item
                            .methods()
                            .into_iter()
                            .any(|(method, _)| method == *request.method())
                        {
                            return Err(Error::MethodNotAllowed);
                        }
                        let permit = slots.try_acquire_owned().map_err(|_| Error::Capacity)?;
                        let handshake = request
                            .extensions()
                            .get::<dhttp::HandshakeSummary>()
                            .ok_or(Error::MissingHandshake)?;
                        let invocation = Invocation::new(lib, permit, endpoint, handshake, tasks)?;
                        let mut request = request.map(|b| b.map_err(Into::into).boxed_unsync());
                        let mut parts = request.uri().clone().into_parts();
                        let path = match request.uri().query() {
                            Some(q) => format!("{suffix}?{q}"),
                            None => suffix,
                        };
                        parts.path_and_query = Some(
                            path.parse()
                                .map_err(|_| Error::BadRequest("invalid API path".into()))?,
                        );
                        *request.uri_mut() = http::Uri::from_parts(parts)
                            .map_err(|_| Error::BadRequest("invalid request URI".into()))?;
                        return invocation
                            .execute(request)
                            .await
                            .map(|r| r.map(AxumBody::new));
                    }
                    if reserved(path) {
                        return Err(Error::RouteNotFound);
                    }
                    let exact = proxies
                        .iter()
                        .find(|p| p.location.strip_prefix("= ") == Some(path));
                    if exact.is_none() {
                        if let Some(route) = proxies.iter().find(|p| {
                            p.location.starts_with('/')
                                && p.location.ends_with('/')
                                && p.location.trim_end_matches('/') == path
                        }) {
                            let location = match request.uri().query() {
                                Some(q) => format!("{}?{q}", route.location),
                                None => route.location.clone(),
                            };
                            return Ok((
                                StatusCode::MOVED_PERMANENTLY,
                                [(header::LOCATION, location)],
                            )
                                .into_response());
                        }
                    }
                    let route = exact.or_else(|| {
                        proxies
                            .iter()
                            .filter(|p| {
                                p.location.starts_with('/') && path.starts_with(&p.location)
                            })
                            .max_by_key(|p| p.location.len())
                    });
                    if let Some(route) = route {
                        return proxy(
                            endpoint,
                            route.clone(),
                            request.map(|b| b.map_err(Into::into).boxed_unsync()),
                        )
                        .await
                        .map(|r| r.map(AxumBody::new));
                    }
                    static_file(&root, request).await
                }
                .await;
                result.unwrap_or_else(reject)
            }
        }))
        .layer(axum::middleware::from_fn_with_state(access, authorize));
    Ok(app)
}

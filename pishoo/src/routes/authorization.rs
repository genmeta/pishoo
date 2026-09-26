pub(crate) async fn authorize(
    axum::extract::State(access): axum::extract::State<Arc<AccessService>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    request.extensions_mut().remove::<Visitor>();
    let Some(handshake) = request.extensions().get::<dhttp::HandshakeSummary>() else {
        return reject(Error::MissingHandshake);
    };
    if handshake.local.is_none() {
        return reject(Error::MissingHandshake);
    }
    let visitor = match &handshake.remote {
        Some(remote) => match dhttp::certificate::subject_id(remote.certificates())
            .ok()
            .and_then(|s| SubjectId::new(s).ok())
        {
            Some(subject) => Some(Visitor::new(remote.name(), subject)),
            None => return reject(Error::Denied),
        },
        None => None,
    };
    let headers = Headers {
        method: request.method().clone(),
        path: request
            .uri()
            .path_and_query()
            .map_or("/", |p| p.as_str())
            .to_string(),
        fields: request.headers().clone(),
        request_id: None,
    };
    let allowed = match access
        .auth(
            headers,
            visitor.as_ref().map(Visitor::name),
            visitor.as_ref().map(Visitor::subject_id),
        )
        .await
    {
        Ok(AuthResult::Allowed) => true,
        Ok(AuthResult::Denied) => false,
        Ok(AuthResult::Reviewing(id, state, registry)) => {
            let _cleanup = scopeguard::guard((state.clone(), registry), |(state, registry)| {
                state.cancel();
                registry.del(id);
            });
            matches!(state.await, Ok(Action::Allow))
        }
        Err(e) => {
            eprintln!("authorization failed: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !allowed {
        return reject(Error::Denied);
    }
    if let Some(visitor) = visitor {
        request.extensions_mut().insert(visitor);
    }
    next.run(request).await
}

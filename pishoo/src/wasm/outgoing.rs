// Host-authorized outgoing requests use only the current identity Endpoint.

impl WasiHttpHooks for HostOutgoing {
    fn send_request(
        &mut self,
        mut request: Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
    ) -> HttpResult<HostFutureIncomingResponse> {
        let endpoint = self
            .endpoint
            .as_ref()
            .ok_or(types::ErrorCode::HttpRequestDenied)?
            .clone();
        if self.cancel.is_cancelled()
            || self.remaining_requests == 0
            || !outgoing_allowed(&self.policy, request.method(), request.uri())
        {
            return Err(types::ErrorCode::HttpRequestDenied.into());
        }
        self.remaining_requests -= 1;
        let reserved: Vec<_> = request
            .headers()
            .keys()
            .filter(|name| name.as_str().starts_with("pishoo-"))
            .cloned()
            .collect();
        for name in reserved {
            request.headers_mut().remove(name);
        }
        let cancel = self.cancel.clone();
        let children = self.children.clone();
        let pending = self.children.spawn(async move {
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(Err(types::ErrorCode::HttpRequestDenied)),
                response = tokio::time::timeout(config.connect_timeout.saturating_add(config.first_byte_timeout), endpoint.from_request(request)) => {
                    match response {
                        Ok(Ok(response)) => response,
                        Ok(Err(error)) => return Ok(Err(types::ErrorCode::InternalError(Some(error.to_string())))),
                        Err(_) => return Ok(Err(types::ErrorCode::ConnectionTimeout)),
                    }
                }
            };
            let (parts, mut body) = response.into_parts();
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let worker = children.spawn(async move {
                let pumping = async {
                    while let Some(frame) = body.frame().await {
                        match frame {
                            Ok(frame) => match frame.into_data() {
                                Ok(mut bytes) => {
                                    while !bytes.is_empty() {
                                        let chunk = bytes.split_to(bytes.len().min(16 * 1024));
                                        if tx.send(Ok(Frame::data(chunk))).await.is_err() { return; }
                                    }
                                }
                                Err(frame) => { if tx.send(Ok(frame)).await.is_err() { return; } }
                            },
                            Err(error) => {
                                let _ = tx.send(Err(types::ErrorCode::InternalError(Some(error.to_string())))).await;
                                return;
                            }
                        }
                    }
                };
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {},
                    _ = pumping => {},
                }
            });
            let frames = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|frame| (frame, rx)) });
            Ok(Ok(IncomingResponse {
                resp: Response::from_parts(parts, StreamBody::new(Box::pin(frames)).boxed_unsync()),
                worker: Some(worker.into()),
                between_bytes_timeout: config.between_bytes_timeout,
            }))
        });
        Ok(HostFutureIncomingResponse::pending(pending.into()))
    }

    fn outgoing_body_buffer_chunks(&mut self) -> usize {
        1
    }
    fn outgoing_body_chunk_size(&mut self) -> usize {
        16 * 1024
    }
}

fn outgoing_allowed(policy: &LibPolicy, method: &Method, uri: &Uri) -> bool {
    let Some(host) = uri.host().and_then(dhttp_home::normalize_name) else {
        return false;
    };
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri
            .authority()
            .is_none_or(|authority| authority.as_str().contains('@'))
    {
        return false;
    }
    // Reject ambiguous escaping and normalization before comparing a capability
    // prefix or reserved management route. Guest URLs cannot smuggle dot paths.
    let path = uri.path();
    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let Some(high) = bytes.next().and_then(|byte| (byte as char).to_digit(16)) else {
                return false;
            };
            let Some(low) = bytes.next().and_then(|byte| (byte as char).to_digit(16)) else {
                return false;
            };
            let byte = (high * 16 + low) as u8;
            if matches!(byte, b'/' | b'\\' | b'%' | 0) {
                return false;
            }
            decoded.push(byte);
        } else {
            decoded.push(byte);
        }
    }
    let Ok(path) = std::str::from_utf8(&decoded) else {
        return false;
    };
    if path.contains('\\') || path.split('/').any(|part| part == "." || part == "..") {
        return false;
    }
    if [
        "/acl",
        "/contact",
        "/contacts",
        "/workspace",
        "/workspace-api",
    ]
    .iter()
    .any(|prefix| {
        path == *prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    }) {
        return false;
    }
    policy.outgoing.iter().any(|rule| {
        rule.methods.contains(method)
            && rule.origin.scheme() == uri.scheme()
            && rule
                .origin
                .host()
                .and_then(dhttp_home::normalize_name)
                .as_ref()
                == Some(&host)
            && rule
                .origin
                .port_u16()
                .or_else(|| match rule.origin.scheme_str() {
                    Some("http") => Some(80),
                    Some("https") => Some(443),
                    _ => None,
                })
                == uri.port_u16().or_else(|| match uri.scheme_str() {
                    Some("http") => Some(80),
                    Some("https") => Some(443),
                    _ => None,
                })
            && rule.path_prefix.starts_with('/')
            && (path == rule.path_prefix
                || path
                    .strip_prefix(&rule.path_prefix)
                    .is_some_and(|rest| rule.path_prefix.ends_with('/') || rest.starts_with('/')))
    })
}

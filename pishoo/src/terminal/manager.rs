impl TerminalManager {
    pub fn new(policy: TerminalPolicy, _state_dir: &Path) -> Result<Self> {
        let administrators = policy
            .administrators
            .iter()
            .map(|name| {
                dhttp_home::normalize_name(name)
                    .map(Arc::<str>::from)
                    .ok_or_else(|| {
                        Error::InvalidConfig("invalid terminal administrator name".into())
                    })
            })
            .collect::<Result<HashSet<_>>>()?;
        if policy.enabled && administrators.is_empty() {
            return Err(Error::InvalidConfig(
                "enabled terminal requires an administrator".into(),
            ));
        }
        let backend = if policy.enabled {
            let error = Error::BackendUnavailable(
                "terminal helper, file broker, and platform isolation are not implemented".into(),
            );
            eprintln!("terminal unavailable: {error}");
            TerminalBackend::Unavailable(error)
        } else {
            TerminalBackend::Disabled
        };
        Ok(Self {
            backend,
            administrators: Mutex::new(administrators),
            permits: Arc::new(Semaphore::new(MAX_SESSIONS)),
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            tasks: TaskTracker::new(),
        })
    }

    pub async fn handle(
        self: &Arc<Self>,
        server_name: &str,
        server_cancel: CancellationToken,
        request: Request<dhttp::Body>,
    ) -> Result<Response<Body>> {
        if matches!(self.backend, TerminalBackend::Disabled) {
            return Ok(status(StatusCode::NOT_FOUND));
        }
        let Some(summary) = request.extensions().get::<dhttp::HandshakeSummary>() else {
            return Err(Error::MissingHandshake);
        };
        let Some(local) = summary.local.as_ref() else {
            return Err(Error::IdentityMismatch);
        };
        let Some(server_name) = dhttp_home::normalize_name(server_name) else {
            return Err(Error::IdentityMismatch);
        };
        if dhttp_home::normalize_name(local.name()).as_deref() != Some(server_name.as_str()) {
            return Err(Error::IdentityMismatch);
        }
        let Some(caller) = summary
            .remote
            .as_ref()
            .and_then(|remote| dhttp_home::normalize_name(remote.name()))
        else {
            return Err(Error::Denied);
        };
        if !self
            .administrators
            .lock()
            .unwrap()
            .contains(caller.as_str())
        {
            return Err(Error::Denied);
        }
        if self.permits.is_closed() || server_cancel.is_cancelled() {
            return Err(Error::Closed);
        }
        if request.method() != Method::CONNECT {
            let mut response = status(StatusCode::METHOD_NOT_ALLOWED);
            response
                .headers_mut()
                .insert(http::header::ALLOW, "CONNECT".parse().unwrap());
            return Ok(response);
        }
        let versions = request.headers().get_all(VERSION_HEADER);
        let mut versions = versions.iter();
        if versions.next().map(|value| value.as_bytes()) != Some(b"1")
            || versions.next().is_some()
            || request.extensions().get::<Arc<str>>().map(AsRef::as_ref) != Some("pishoo-terminal")
        {
            return Ok(status(StatusCode::BAD_REQUEST));
        }
        if request.uri().path() != "/shell" && !request.uri().path().starts_with("/shell/") {
            return Ok(status(StatusCode::NOT_FOUND));
        }
        // Never return 200 or start a host process for an unverified backend.
        Ok(status(StatusCode::NOT_IMPLEMENTED))
    }

    pub fn revoke(&self, identity: &str) {
        let Some(identity) = dhttp_home::normalize_name(identity) else {
            return;
        };
        let mut administrators = self.administrators.lock().unwrap();
        let sessions = self.sessions.lock().unwrap();
        administrators.remove(identity.as_str());
        for (caller, cancel) in sessions.values() {
            if caller.as_ref() == identity {
                cancel.cancel();
            }
        }
    }

    pub async fn shutdown(&self, deadline: Instant) -> Result<()> {
        {
            let sessions = self.sessions.lock().unwrap();
            self.permits.close();
            for (_, cancel) in sessions.values() {
                cancel.cancel();
            }
            self.tasks.close();
        }
        tokio::time::timeout_at(deadline.into(), self.tasks.wait())
            .await
            .map_err(|_| Error::ShutdownDeadline)?;
        Ok(())
    }
}

fn status(code: StatusCode) -> Response<Body> {
    let mut response = Response::new(
        Empty::<Bytes>::new()
            .map_err(|error| match error {})
            .boxed_unsync(),
    );
    *response.status_mut() = code;
    response
}

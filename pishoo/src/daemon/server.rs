impl Server {
    async fn load(
        profile: IdentityProfile,
        runtime: Arc<Runtime>,
        terminal: Arc<TerminalManager>,
    ) -> Result<Self> {
        let identity = profile
            .load_identity()
            .await
            .map_err(|e| Error::InvalidIdentity(e.to_string()))?;
        if identity.name != profile.name() {
            return Err(Error::IdentityMismatch);
        }
        let endpoint = dhttp::Endpoint::load(profile.name()).await?;
        let config = load_server_config(&profile)?;
        let subject =
            access_control::SubjectId::new(dhttp::certificate::subject_id(&identity.certs)?)
                .map_err(|_| Error::InvalidIdentity("invalid certificate subject".into()))?;
        std::fs::create_dir_all(profile.db_dir())?;
        let uri = format!("sqlite://{}?mode=rwc", profile.access_db_path().display());
        let access = Arc::new(
            access_control::AccessService::load_from_db(&uri, &identity.name, &subject).await?,
        );
        let cancel = CancellationToken::new();
        let libs = load_libs(&profile, &runtime, &BTreeMap::new(), &cancel)?;
        let sandbox = Arc::new(Sandbox::new());
        let router = build_router(
            endpoint.clone(),
            access.clone(),
            &libs,
            &config,
            &profile,
            sandbox.clone(),
        )?;
        Ok(Self {
            profile,
            endpoint,
            config,
            access,
            router: Arc::new(RwLock::new(router)),
            libs,
            runtime,
            sandbox,
            terminal,
            cancel,
        })
    }
    fn name(&self) -> &str {
        self.endpoint.name()
    }

    async fn reload(&mut self) -> Result<()> {
        let config = load_server_config(&self.profile)?;
        if config.listen != self.config.listen {
            return Err(Error::InvalidConfig(
                "listen changes require restarting the instance".into(),
            ));
        }
        let libs = load_libs(&self.profile, &self.runtime, &self.libs, &self.cancel)?;
        let router = build_router(
            self.endpoint.clone(),
            self.access.clone(),
            &libs,
            &config,
            &self.profile,
            self.sandbox.clone(),
        )?;
        if !self.profile.path().symlink_metadata()?.is_dir() {
            return Err(Error::InvalidIdentity(
                "identity directory disappeared".into(),
            ));
        }
        for (id, lib) in &libs {
            if self.libs.get(id).is_some_and(|old| Arc::ptr_eq(old, lib)) {
                continue;
            }
            let path = self.profile.join("lib").join(id).join("lib.wasm");
            let bytes = std::fs::read(path)?;
            if <[u8; 32]>::from(Sha256::digest(bytes)) != lib.digest {
                return Err(Error::InvalidComponent(
                    "component changed during reload".into(),
                ));
            }
        }
        *self.router.write().unwrap() = router;
        for (id, old) in &self.libs {
            if !libs.contains_key(id) {
                old.cancel.cancel();
            }
        }
        self.libs = libs;
        self.config = config;
        Ok(())
    }

    fn listen(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>> {
        let (endpoint, router, cancel, terminal, listen) = (
            self.endpoint.clone(),
            self.router.clone(),
            self.cancel.clone(),
            self.terminal.clone(),
            self.config.listen,
        );
        Box::pin(async move {
            let scopes = match listen {
                0 => return Ok(()),
                1 => dhttp::Scope::Internal.into(),
                2 => dhttp::Scope::External.into(),
                _ => dhttp::Scope::Internal | dhttp::Scope::External,
            };
            let name = endpoint.name().to_owned();
            endpoint.listen(scopes, tower::service_fn(move |mut request: http::Request<Body>| {
                let (router, cancel, terminal, name) = (router.clone(), cancel.clone(), terminal.clone(), name.clone());
                async move {
                    let result: Result<http::Response<Body>> = async {
                        if cancel.is_cancelled() { return Err(Error::Closed); }
                        let summary = request.extensions().get::<dhttp::HandshakeSummary>().ok_or(Error::MissingHandshake)?;
                        if summary.local.as_ref().is_none_or(|l| l.name() != name) { return Err(Error::IdentityMismatch); }
                        let identity = dhttp_identity::name::DhttpName::try_from(name.clone()).map_err(|_| Error::IdentityMismatch)?;
                        *request.uri_mut() = identity.expand_uri(request.uri().clone()).map_err(|_| Error::IdentityMismatch)?;
                        let authority = request.uri().authority().ok_or(Error::IdentityMismatch)?;
                        if authority.as_str().contains('@') || dhttp_home::normalize_name(authority.host()).as_deref() != Some(name.as_str()) { return Err(Error::IdentityMismatch); }
                        if matches!(request.uri().path(), "/shell" | "/.pishoo/terminal") || request.uri().path().starts_with("/shell/") {
                            return terminal.handle(&name, cancel, request).await;
                        }
                        let names = request.headers().keys().filter(|n| n.as_str().starts_with("pishoo-")).cloned().collect::<Vec<_>>();
                        for name in names { request.headers_mut().remove(name); }
                        let app = router.read().unwrap().clone();
                        let response = tokio::select! {
                            _ = cancel.cancelled() => return Err(Error::Closed),
                            response = app.oneshot(request.map(axum::body::Body::new)) => response.expect("Router is infallible"),
                        };
                        Ok(response.map(|b| b.map_err(Into::into).boxed_unsync()))
                    }.await;
                    let response = result.unwrap_or_else(|error| {
                        let status = error.status();
                        if status.is_server_error() { eprintln!("request for {name}: {error}"); }
                        (status, status.canonical_reason().unwrap_or("request failed")).into_response().map(|b| b.map_err(Into::into).boxed_unsync())
                    });
                    Ok::<_, std::convert::Infallible>(response)
                }
            })).await.map_err(Into::into)
        })
    }

    async fn close(&mut self) -> Result<()> {
        self.cancel.cancel();
        self.sandbox.close();
        let result = if dhttp::DhttpNetwork::global().is_ok() {
            self.endpoint.close().map_err(Error::from)
        } else {
            Ok(())
        };
        *self.router.write().unwrap() = axum::Router::new();
        self.libs.clear();
        self.sandbox.wait().await?;
        result
    }
}

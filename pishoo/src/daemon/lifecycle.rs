pub async fn run(config: DaemonConfig) -> Result<()> {
    // The open file owns the instance lock for this entire invocation.
    let expected = std::fs::canonicalize(&config.state_dir)?;
    let home = DhttpHome::load(dhttp_home::HomeScope::User)
        .map_err(|e| Error::InvalidConfig(e.to_string()))?;
    if std::fs::canonicalize(home.as_path())? != expected {
        return Err(Error::InvalidConfig(
            "state_dir and startup DHTTP_HOME must be the same directory".into(),
        ));
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(expected.join("pishoo.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .map_err(|e| Error::InvalidConfig(format!("instance is already running: {e}")))?;
    let mut daemon = Daemon::load(config).await?;
    let result = daemon.run().await;
    let shutdown = daemon.shutdown().await;
    drop(lock);
    result.and(shutdown)
}

impl Daemon {
    async fn load(config: DaemonConfig) -> Result<Self> {
        let home = DhttpHome::new(config.state_dir.clone());
        let runtime = Arc::new(Runtime::new()?);
        let terminal = Arc::new(TerminalManager::new(config.terminal, &config.state_dir)?);
        let mut servers = BTreeMap::new();
        for profile in home.discover_identity_profiles()? {
            match Server::load(profile.clone(), runtime.clone(), terminal.clone()).await {
                Ok(server) => {
                    servers.insert(server.name().to_owned(), server);
                }
                Err(e) => eprintln!("skipping server {}: {e}", profile.name()),
            }
        }
        let configs = servers
            .values()
            .map(|s| s.config.clone())
            .collect::<Vec<_>>();
        if let Some(config) = network_config(&configs)? {
            dhttp::DhttpNetwork::init(config).await?;
        }
        let mut daemon = Self {
            home,
            servers,
            listeners: JoinSet::new(),
            runtime,
            terminal,
        };
        for server in daemon.servers.values().filter(|s| s.config.listen != 0) {
            let (name, listener) = (server.name().to_owned(), server.listen());
            daemon
                .listeners
                .spawn(async move { (name, listener.await) });
        }
        Ok(daemon)
    }

    async fn run(&mut self) -> Result<()> {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        let interrupt = tokio::signal::ctrl_c();
        tokio::pin!(interrupt);
        #[cfg(unix)]
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        loop {
            tokio::select! {
                result = &mut interrupt => { result?; return Ok(()); }
                _ = async { #[cfg(unix)] { terminate.recv().await; } #[cfg(not(unix))] { std::future::pending::<()>().await; } } => return Ok(()),
                _ = interval.tick() => if let Err(e) = self.reload().await { eprintln!("reload rejected: {e}"); },
                result = self.listeners.join_next(), if !self.listeners.is_empty() => {
                    if let Some(result) = result {
                        match result {
                            Ok((name, outcome)) => {
                                if let Err(e) = outcome { eprintln!("listener {name} ended: {e}"); }
                                if let Some(server) = self.servers.get_mut(&name) {
                                    if let Err(e) = server.close().await { eprintln!("closing {name}: {e}"); }
                                }
                            }
                            Err(e) => eprintln!("listener task failed: {e}"),
                        }
                    }
                }
            }
        }
    }

    async fn reload(&mut self) -> Result<()> {
        let profiles = self.home.discover_identity_profiles()?;
        let present = profiles.iter().map(|p| p.name()).collect::<HashSet<_>>();
        for (name, server) in &mut self.servers {
            if !present.contains(name.as_str()) && !server.cancel.is_cancelled() {
                if let Err(e) = server.close().await {
                    eprintln!("closing removed server {name}: {e}");
                }
            }
        }
        for profile in profiles {
            if let Some(server) = self.servers.get_mut(profile.name()) {
                if !server.cancel.is_cancelled() {
                    if let Err(e) = server.reload().await {
                        eprintln!("keeping server {}: {e}", server.name());
                    }
                }
                continue;
            }
            match Server::load(profile.clone(), self.runtime.clone(), self.terminal.clone()).await {
                Ok(server) => {
                    if server.config.listen != 0 {
                        // Endpoint.listen checks its scopes against the immutable startup rules.
                        if dhttp::DhttpNetwork::global().is_err() {
                            eprintln!("new listening server {} requires a restart", server.name());
                            continue;
                        }
                        let (name, listener) = (server.name().to_owned(), server.listen());
                        self.listeners.spawn(async move { (name, listener.await) });
                    }
                    self.servers.insert(server.name().to_owned(), server);
                }
                Err(e) => eprintln!("skipping server {}: {e}", profile.name()),
            }
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut result = Ok(());
        // Cancel every identity before waiting for any application execution.
        for server in self.servers.values() {
            server.cancel.cancel();
            if dhttp::DhttpNetwork::global().is_ok() {
                if let Err(e) = server.endpoint.close() {
                    result = Err(e.into());
                }
            }
            server.tasks.close();
        }
        let own = async {
            let mut outcome = Ok(());
            for server in self.servers.values_mut() {
                if let Err(e) = server.close().await {
                    outcome = Err(e);
                }
            }
            while let Some(joined) = self.listeners.join_next().await {
                match joined {
                    Ok((_, Err(e))) => eprintln!("listener shutdown: {e}"),
                    Err(e) => outcome = Err(e.into()),
                    _ => {}
                }
            }
            outcome
        };
        match tokio::time::timeout_at(deadline, own).await {
            Ok(Err(e)) => result = Err(e),
            Err(_) => result = Err(Error::ShutdownDeadline),
            _ => {}
        }
        if let Err(e) = self.terminal.shutdown(deadline.into_std()).await {
            result = Err(e);
        }
        if let Ok(network) = dhttp::DhttpNetwork::global() {
            if let Err(e) = network.shutdown() {
                result = Err(e.into());
            }
        }
        result
    }
}

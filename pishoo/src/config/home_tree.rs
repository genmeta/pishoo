use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use dhttp::{
    home::{
        DhttpHome,
        identity::{IdentityProfile, ssl::ListIdentityProfilesStrictError},
    },
    name::DhttpName,
};
use gateway::parse::{
    ConfigDocumentParser,
    domain::{ConfigDocumentRole, ConfigSourceSpan},
    error::{ConfigLoadFailure, ConfigQueryError},
    fragment::{ParsedConfigDocument, ParsedPishooFragment, ParsedServerFragment},
    snapshot::{RootConfigSnapshot, RootConfigSnapshotError},
    tree::{ConfigNodeId, HomeConfigTree, HomeConfigTreeError, ServerConfigRef},
};
use snafu::{IntoError, ResultExt, Snafu};

use super::{account::WorkerAccount, source::PishooConfigSource};

#[derive(Clone, Debug)]
pub(crate) enum ServiceScope {
    Global,
    Worker(Arc<WorkerAccount>),
}

#[derive(Clone, Debug)]
pub(crate) enum ServiceBindingOrigin {
    Direct {
        node: ConfigNodeId,
        source: Option<ConfigSourceSpan>,
    },
    Identity {
        node: ConfigNodeId,
        source: Option<ConfigSourceSpan>,
        profile: IdentityProfile,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct ScopedServerConfig {
    server: ServerConfigRef,
    identity_profile: Option<IdentityProfile>,
    service_names: Box<[DhttpName<'static>]>,
}

impl ScopedServerConfig {
    pub(crate) fn server(&self) -> &ServerConfigRef {
        &self.server
    }

    pub(crate) fn identity_profile(&self) -> Option<&IdentityProfile> {
        self.identity_profile.as_ref()
    }

    pub(crate) fn service_names(&self) -> &[DhttpName<'static>] {
        &self.service_names
    }
}

#[derive(Debug)]
pub(crate) struct ScopedHomeConfig {
    scope: ServiceScope,
    tree: Arc<HomeConfigTree>,
    servers: Box<[ScopedServerConfig]>,
}

impl ScopedHomeConfig {
    pub(crate) fn scope(&self) -> &ServiceScope {
        &self.scope
    }

    pub(crate) fn tree(&self) -> &Arc<HomeConfigTree> {
        &self.tree
    }

    pub(crate) fn servers(&self) -> &[ScopedServerConfig] {
        &self.servers
    }

    pub(crate) fn root_snapshot(&self) -> Result<RootConfigSnapshot, RootConfigSnapshotError> {
        self.tree.root_snapshot()
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub(crate) enum BuildHomeConfigError {
    #[snafu(display("failed to inspect configuration source {}", path.display()))]
    SourceMetadata {
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("failed to read configuration source {}", path.display()))]
    ReadSource {
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("failed to parse configuration source {}", path.display()))]
    ParseSource {
        path: PathBuf,
        source: Box<ConfigLoadFailure>,
    },
    #[snafu(display("configuration source {} has the wrong document role", path.display()))]
    DocumentRole { path: PathBuf },
    #[snafu(display("failed to enumerate identity profiles in {}", home.display()))]
    EnumerateProfiles {
        home: PathBuf,
        source: Box<ListIdentityProfilesStrictError>,
    },
    #[snafu(display("identity profile {} contains multiple server blocks", profile.path().display()))]
    MultipleIdentityServers { profile: IdentityProfile },
    #[snafu(display("worker pishoo configuration cannot declare direct server blocks"))]
    WorkerDirectServers,
    #[snafu(display("identity server {} does not bind exactly its profile name", profile.path().display()))]
    IdentityServerNames { profile: IdentityProfile },
    #[snafu(display("identity server {} overrides its profile TLS paths", profile.path().display()))]
    IdentityTlsOverride { profile: IdentityProfile },
    #[snafu(display("direct server {node:?} does not provide a complete TLS pair"))]
    DirectTlsPair { node: ConfigNodeId },
    #[snafu(display("failed to query sealed server configuration"))]
    QueryServer { source: ConfigQueryError },
    #[snafu(display("failed to seal the home configuration tree"))]
    SealTree { source: HomeConfigTreeError },
    #[snafu(display("duplicate service identity {name}"))]
    DuplicateServiceIdentity {
        name: DhttpName<'static>,
        first: Box<ServiceBindingOrigin>,
        second: Box<ServiceBindingOrigin>,
    },
}

pub(crate) async fn build_global_home_config(
    source: &PishooConfigSource,
) -> Result<Arc<ScopedHomeConfig>, BuildHomeConfigError> {
    let registry = gateway::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let root_text = read_required(source.config_path()).await?;
    let root = parse_root(
        &mut parser,
        &root_text,
        source.config_path(),
        source.dhttp_home(),
    )?;
    let direct_count = root.servers().len();

    let (identity_fragments, identity_profiles) = match source.dhttp_home() {
        Some(home) => load_identity_fragments(&mut parser, home).await?,
        None => (Vec::new(), Vec::new()),
    };
    let tree = gateway::parse::tree::build_global_tree(&registry, root, identity_fragments)
        .context(build_home_config_error::SealTreeSnafu)?;
    let all_servers = tree.servers().collect::<Vec<_>>();
    let (direct, identity) = all_servers.split_at(direct_count);
    let mut sealed = Vec::with_capacity(all_servers.len());
    for (server, profile) in identity.iter().cloned().zip(identity_profiles) {
        sealed.push(seal_server(server, Some(profile))?);
    }
    for server in direct.iter().cloned() {
        sealed.push(seal_server(server, None)?);
    }
    reject_duplicate_names(&sealed)?;

    Ok(Arc::new(ScopedHomeConfig {
        scope: ServiceScope::Global,
        tree,
        servers: sealed.into_boxed_slice(),
    }))
}

pub(crate) async fn build_worker_home_config(
    account: WorkerAccount,
    root: Arc<RootConfigSnapshot>,
) -> Result<Arc<ScopedHomeConfig>, BuildHomeConfigError> {
    let registry = gateway::parse::default_registry();
    let mut parser = ConfigDocumentParser::new(&registry);
    let home = account.dhttp_home();
    let worker_path = home.join(PishooConfigSource::CONFIG_FILE_NAME);
    let worker = match read_optional(&worker_path).await? {
        Some(text) => {
            let worker = parse_worker(&mut parser, &text, &worker_path, home)?;
            if !worker.servers().is_empty() {
                return build_home_config_error::WorkerDirectServersSnafu.fail();
            }
            Some(worker)
        }
        None => None,
    };
    let (identity_fragments, identity_profiles) =
        load_identity_fragments(&mut parser, home).await?;
    let tree = gateway::parse::tree::build_worker_tree(
        &registry,
        root.as_ref().clone(),
        worker,
        identity_fragments,
    )
    .context(build_home_config_error::SealTreeSnafu)?;
    let mut sealed = Vec::new();
    for (server, profile) in tree.servers().zip(identity_profiles) {
        sealed.push(seal_server(server, Some(profile))?);
    }
    reject_duplicate_names(&sealed)?;

    Ok(Arc::new(ScopedHomeConfig {
        scope: ServiceScope::Worker(Arc::new(account)),
        tree,
        servers: sealed.into_boxed_slice(),
    }))
}

async fn load_identity_fragments(
    parser: &mut ConfigDocumentParser<'_>,
    home: &DhttpHome,
) -> Result<(Vec<ParsedServerFragment>, Vec<IdentityProfile>), BuildHomeConfigError> {
    let profiles = home
        .list_identity_profiles_strict()
        .await
        .map_err(|source| BuildHomeConfigError::EnumerateProfiles {
            home: home.as_path().to_path_buf(),
            source: Box::new(source),
        })?;
    let mut fragments = Vec::new();
    let mut owners = Vec::new();
    for profile in profiles.into_vec() {
        let path = profile.server_conf_path();
        let Some(text) = read_optional(&path).await? else {
            continue;
        };
        let ParsedConfigDocument::IdentityServers(servers) = parser
            .parse_text(
                &text,
                &path,
                ConfigDocumentRole::IdentityServer {
                    home,
                    profile: &profile,
                },
            )
            .map_err(|source| BuildHomeConfigError::ParseSource {
                path: path.clone(),
                source: Box::new(source),
            })?
        else {
            return build_home_config_error::DocumentRoleSnafu { path }.fail();
        };
        if servers.len() != 1 {
            return build_home_config_error::MultipleIdentityServersSnafu { profile }.fail();
        }
        fragments.push(
            servers
                .into_vec()
                .pop()
                .expect("one identity server was validated"),
        );
        owners.push(profile);
    }
    Ok((fragments, owners))
}

fn parse_root(
    parser: &mut ConfigDocumentParser<'_>,
    text: &str,
    path: &Path,
    home: Option<&DhttpHome>,
) -> Result<ParsedPishooFragment, BuildHomeConfigError> {
    let parsed = parser
        .parse_text(text, path, ConfigDocumentRole::HypervisorRoot { home })
        .map_err(|source| BuildHomeConfigError::ParseSource {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
    match parsed {
        ParsedConfigDocument::HypervisorRoot(root) => Ok(root),
        _ => build_home_config_error::DocumentRoleSnafu { path }.fail(),
    }
}

fn parse_worker(
    parser: &mut ConfigDocumentParser<'_>,
    text: &str,
    path: &Path,
    home: &DhttpHome,
) -> Result<ParsedPishooFragment, BuildHomeConfigError> {
    let parsed = parser
        .parse_text(text, path, ConfigDocumentRole::WorkerPishoo { home })
        .map_err(|source| BuildHomeConfigError::ParseSource {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
    match parsed {
        ParsedConfigDocument::WorkerPishoo(worker) => Ok(worker),
        _ => build_home_config_error::DocumentRoleSnafu { path }.fail(),
    }
}

async fn read_required(path: &Path) -> Result<String, BuildHomeConfigError> {
    tokio::fs::read_to_string(path)
        .await
        .context(build_home_config_error::ReadSourceSnafu { path })
}

async fn read_optional(path: &Path) -> Result<Option<String>, BuildHomeConfigError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => tokio::fs::read_to_string(path)
            .await
            .map(Some)
            .context(build_home_config_error::ReadSourceSnafu { path }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => {
            Err(build_home_config_error::SourceMetadataSnafu { path }.into_error(source))
        }
    }
}

fn seal_server(
    server: ServerConfigRef,
    identity_profile: Option<IdentityProfile>,
) -> Result<ScopedServerConfig, BuildHomeConfigError> {
    let names = server
        .node()
        .local(gateway::parse::keys::server::SERVER_NAME)
        .context(build_home_config_error::QueryServerSnafu)?
        .map(|names| {
            names
                .0
                .iter()
                .map(|name| name.name.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let certificate = server
        .node()
        .local(gateway::parse::keys::server::SSL_CERTIFICATE)
        .context(build_home_config_error::QueryServerSnafu)?;
    let key = server
        .node()
        .local(gateway::parse::keys::server::SSL_CERTIFICATE_KEY)
        .context(build_home_config_error::QueryServerSnafu)?;

    if let Some(profile) = &identity_profile {
        if names.as_slice() != [profile.name().clone()] {
            return build_home_config_error::IdentityServerNamesSnafu {
                profile: profile.clone(),
            }
            .fail();
        }
        if certificate.is_some() || key.is_some() {
            return build_home_config_error::IdentityTlsOverrideSnafu {
                profile: profile.clone(),
            }
            .fail();
        }
    } else if certificate.is_none() || key.is_none() {
        return build_home_config_error::DirectTlsPairSnafu {
            node: server.node().id(),
        }
        .fail();
    }

    Ok(ScopedServerConfig {
        server,
        identity_profile,
        service_names: names.into_boxed_slice(),
    })
}

fn reject_duplicate_names(servers: &[ScopedServerConfig]) -> Result<(), BuildHomeConfigError> {
    let mut seen = HashMap::<DhttpName<'static>, ServiceBindingOrigin>::new();
    for server in servers {
        let origin = match &server.identity_profile {
            Some(profile) => ServiceBindingOrigin::Identity {
                node: server.server.node().id(),
                source: server.server.node().source_span(),
                profile: profile.clone(),
            },
            None => ServiceBindingOrigin::Direct {
                node: server.server.node().id(),
                source: server.server.node().source_span(),
            },
        };
        for name in &server.service_names {
            if let Some(first) = seen.insert(name.clone(), origin.clone()) {
                return Err(BuildHomeConfigError::DuplicateServiceIdentity {
                    name: name.clone(),
                    first: Box::new(first),
                    second: Box::new(origin),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc, time::SystemTime};

    use dhttp::home::DhttpHome;
    use nix::unistd::{Gid, Uid};

    use super::{
        BuildHomeConfigError, ServiceScope, build_global_home_config, build_worker_home_config,
    };
    use crate::config::{account::WorkerAccount, source::PishooConfigSource};

    struct TempHome(PathBuf);

    impl TempHome {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "pishoo-home-tree-{label}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn home(&self) -> DhttpHome {
            DhttpHome::new(self.0.clone())
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn profile(&self, name: &str, server: Option<&str>) {
            std::fs::create_dir_all(self.0.join(name).join("ssl")).unwrap();
            if let Some(server) = server {
                self.write(&format!("{name}/server.conf"), server);
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn global_source(temp: &TempHome, config: &str) -> PishooConfigSource {
        temp.write("pishoo.conf", config);
        PishooConfigSource::from_global_home_at(temp.home(), std::path::Path::new("/ignored"))
            .unwrap()
    }

    async fn root_snapshot() -> Arc<gateway::parse::snapshot::RootConfigSnapshot> {
        let temp = TempHome::new("root-snapshot");
        let global = build_global_home_config(&global_source(&temp, "pishoo {}"))
            .await
            .unwrap();
        Arc::new(global.root_snapshot().unwrap())
    }

    fn account(temp: &TempHome) -> WorkerAccount {
        WorkerAccount::new(
            "alice".to_owned(),
            Uid::from_raw(1000),
            Gid::from_raw(100),
            PathBuf::from("/home/alice"),
            temp.home(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn missing_worker_pishoo_file_is_an_empty_overlay() {
        let worker = TempHome::new("missing-worker-overlay");
        let config = build_worker_home_config(account(&worker), root_snapshot().await)
            .await
            .unwrap();
        assert!(matches!(config.scope(), ServiceScope::Worker(_)));
        assert!(config.servers().is_empty());
    }

    #[tokio::test]
    async fn bad_worker_pishoo_file_fails_the_whole_home() {
        let worker = TempHome::new("bad-worker-overlay");
        worker.write("pishoo.conf", "pishoo {");
        let error = build_worker_home_config(account(&worker), root_snapshot().await)
            .await
            .unwrap_err();
        assert!(matches!(error, BuildHomeConfigError::ParseSource { .. }));
    }

    #[tokio::test]
    async fn missing_identity_server_conf_is_skipped() {
        let worker = TempHome::new("missing-identity-server");
        worker.profile("reimu.pilot", None);
        let config = build_worker_home_config(account(&worker), root_snapshot().await)
            .await
            .unwrap();
        assert!(config.servers().is_empty());
    }

    #[tokio::test]
    async fn bad_identity_server_fails_the_whole_home() {
        let worker = TempHome::new("bad-identity-server");
        worker.profile("reimu.pilot", Some("server {"));
        let error = build_worker_home_config(account(&worker), root_snapshot().await)
            .await
            .unwrap_err();
        assert!(matches!(error, BuildHomeConfigError::ParseSource { .. }));
    }

    #[tokio::test]
    async fn identity_server_without_server_name_uses_captured_profile_name() {
        let worker = TempHome::new("identity-default-name");
        worker.profile("reimu.pilot", Some("server { listen all 443; }"));
        let config = build_worker_home_config(account(&worker), root_snapshot().await)
            .await
            .unwrap();
        assert_eq!(config.servers().len(), 1);
        assert_eq!(
            config.servers()[0].service_names()[0].as_full(),
            "reimu.pilot.dhttp.net"
        );
        assert_eq!(
            config.servers()[0].identity_profile().unwrap().path(),
            worker.0.join("reimu.pilot")
        );
    }

    #[tokio::test]
    async fn identity_server_rejects_mismatched_or_multiple_server_names() {
        let worker = TempHome::new("identity-mismatch");
        worker.profile(
            "reimu.pilot",
            Some("server { listen all 443; server_name youmu.pilot; }"),
        );
        let error = build_worker_home_config(account(&worker), root_snapshot().await)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            BuildHomeConfigError::IdentityServerNames { .. }
        ));
    }

    #[tokio::test]
    async fn identity_server_rejects_multiple_server_blocks() {
        let worker = TempHome::new("identity-multiple-servers");
        worker.profile(
            "reimu.pilot",
            Some("server { listen all 443; } server { listen all 444; }"),
        );
        let error = build_worker_home_config(account(&worker), root_snapshot().await)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            BuildHomeConfigError::MultipleIdentityServers { .. }
        ));
    }

    #[tokio::test]
    async fn identity_server_rejects_explicit_tls_path_override() {
        let worker = TempHome::new("identity-tls-override");
        worker.profile(
            "reimu.pilot",
            Some(
                "server { listen all 443; ssl_certificate /tmp/cert; ssl_certificate_key /tmp/key; }",
            ),
        );
        let error = build_worker_home_config(account(&worker), root_snapshot().await)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            BuildHomeConfigError::IdentityTlsOverride { .. }
        ));
    }

    #[tokio::test]
    async fn duplicate_service_name_between_canonical_profile_paths_fails_whole_home() {
        let worker = TempHome::new("identity-duplicate-canonical");
        worker.profile("reimu.pilot", Some("server { listen all 443; }"));
        worker.profile("reimu.pilot.dhttp.net", Some("server { listen all 444; }"));
        let error = build_worker_home_config(account(&worker), root_snapshot().await)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            BuildHomeConfigError::DuplicateServiceIdentity { .. }
        ));
    }

    #[tokio::test]
    async fn explicit_config_does_not_enumerate_home_identities() {
        let temp = TempHome::new("explicit-no-identities");
        temp.profile("123", None);
        temp.write(
            "explicit.conf",
            "pishoo { server { listen all 443; server_name reimu.pilot; ssl_certificate /tmp/cert; ssl_certificate_key /tmp/key; } }",
        );
        let source = PishooConfigSource::explicit_at(
            temp.0.join("explicit.conf"),
            std::path::Path::new("/ignored"),
        )
        .unwrap();
        let config = build_global_home_config(&source).await.unwrap();
        assert_eq!(config.servers().len(), 1);
        assert!(config.servers()[0].identity_profile().is_none());
    }

    #[tokio::test]
    async fn global_home_servers_are_identity_total_order_then_direct_source_order() {
        let temp = TempHome::new("global-order");
        temp.profile("youmu.pilot", Some("server { listen all 445; }"));
        temp.profile("reimu.pilot", Some("server { listen all 444; }"));
        let source = global_source(
            &temp,
            "pishoo { server { listen all 443; server_name sanae.pilot; ssl_certificate /tmp/cert; ssl_certificate_key /tmp/key; } }",
        );
        let config = build_global_home_config(&source).await.unwrap();
        let names = config
            .servers()
            .iter()
            .map(|server| server.service_names()[0].as_partial())
            .collect::<Vec<_>>();
        assert_eq!(names, ["reimu.pilot", "youmu.pilot", "sanae.pilot"]);
        assert!(matches!(config.scope(), ServiceScope::Global));
        assert_eq!(config.tree().servers().count(), 3);
    }
}

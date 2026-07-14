use std::{collections::HashSet, net::SocketAddr};

use snafu::{ResultExt, Snafu};

use crate::parse::{error::ConfigQueryError, tree::ServerConfigRef};

/// 本节点的 STUN 运行时配置。
#[derive(Debug, Clone)]
pub struct StunNodeConfig {
    /// 是否加入 forward 中转网络（默认 false）
    pub relay: bool,
    /// 每个 bind 块的配置（支持多地址族 / 多绑定）
    pub binds: Vec<StunBindConfig>,
}

/// 单个 bind 块的 STUN 配置
#[derive(Debug, Clone)]
pub struct StunBindConfig {
    pub bind_address: SocketAddr,
    pub outer_address: Option<SocketAddr>,
    pub change_address: Option<SocketAddr>,
    pub change_port: Option<u16>,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum StunConfigError {
    #[snafu(display("failed to query stun directive"))]
    Stun { source: ConfigQueryError },
    #[snafu(display("failed to query relay directive"))]
    Relay { source: ConfigQueryError },
    #[snafu(display("failed to query stun_server compounds"))]
    Servers { source: ConfigQueryError },
}

impl StunNodeConfig {
    /// Materializes STUN configuration exclusively from the sealed SERVER-local typed slots.
    pub fn from_server_ref(server: &ServerConfigRef) -> Result<Option<Self>, StunConfigError> {
        let stun_enabled = server
            .node()
            .local(crate::parse::keys::server::STUN)
            .context(stun_config_error::StunSnafu)?
            .is_some_and(|value| value.0);
        let compounds = server
            .node()
            .repeated(crate::parse::keys::server::STUN_SERVERS)
            .context(stun_config_error::ServersSnafu)?;
        let binds = compounds
            .iter()
            .flat_map(|compound| compound.binds.iter())
            .map(|bind| StunBindConfig {
                bind_address: bind.bind,
                outer_address: bind.outer_addr,
                change_address: bind.change_addr,
                change_port: bind.change_port,
            })
            .collect::<Vec<_>>();
        if !stun_enabled && binds.is_empty() {
            return Ok(None);
        }

        let relay = server
            .node()
            .local(crate::parse::keys::server::RELAY)
            .context(stun_config_error::RelaySnafu)?
            .is_some_and(|value| value.0);
        Ok(Some(Self { relay, binds }))
    }

    pub fn has_configured_binds(&self) -> bool {
        !self.binds.is_empty()
    }

    pub fn configured_addrs(&self) -> HashSet<SocketAddr> {
        self.binds.iter().map(|bind| bind.bind_address).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::StunNodeConfig;
    use crate::parse::{
        ConfigDocumentParser, domain::ConfigDocumentRole, fragment::ParsedConfigDocument,
        tree::build_global_tree,
    };

    fn server(extra: &str) -> crate::parse::tree::ServerConfigRef {
        let text = format!(
            "pishoo {{ server {{ listen all 5378; server_name example.com; ssl_certificate /tmp/cert; ssl_certificate_key /tmp/key; {extra} }} }}"
        );
        let registry = crate::parse::default_registry();
        let mut parser = ConfigDocumentParser::new(&registry);
        let ParsedConfigDocument::HypervisorRoot(root) = parser
            .parse_text(
                &text,
                std::path::Path::new("/tmp/pishoo.conf"),
                ConfigDocumentRole::HypervisorRoot { home: None },
            )
            .unwrap()
        else {
            panic!("expected root fragment")
        };
        build_global_tree(&registry, root, [])
            .unwrap()
            .servers()
            .next()
            .unwrap()
    }

    #[test]
    fn stun_on_without_bind_remains_dynamic() {
        let config = StunNodeConfig::from_server_ref(&server("stun on;"))
            .unwrap()
            .unwrap();
        assert!(!config.has_configured_binds());
    }

    #[test]
    fn stun_off_with_bind_remains_configured() {
        let config = StunNodeConfig::from_server_ref(&server(
            "stun off; stun_server { bind 127.0.0.1:1000; }",
        ))
        .unwrap()
        .unwrap();
        assert!(config.has_configured_binds());
    }

    #[test]
    fn relay_on_alone_remains_disabled_without_stun_or_bind() {
        assert!(
            StunNodeConfig::from_server_ref(&server("relay on;"))
                .unwrap()
                .is_none()
        );
    }
}

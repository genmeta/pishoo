use std::{collections::HashSet, net::SocketAddr};

use crate::parse::config::ServerConfig;

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

impl StunNodeConfig {
    /// Materializes STUN configuration exclusively from the sealed SERVER-local typed slots.
    pub fn from_server(server: &ServerConfig) -> Option<Self> {
        let stun_enabled = server.stun().is_some_and(|value| value.0);
        let compounds = server.stun_servers();
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
            return None;
        }

        let relay = server.relay().is_some_and(|value| value.0);
        Some(Self { relay, binds })
    }

    pub fn has_configured_binds(&self) -> bool {
        !self.binds.is_empty()
    }

    pub fn configured_addrs(&self) -> HashSet<SocketAddr> {
        self.binds.iter().map(|bind| bind.bind_address).collect()
    }
}

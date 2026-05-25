use std::{collections::HashSet, net::SocketAddr, sync::Arc};

use crate::parse::{
    document::ConfigNode,
    types::{BoolConfig, SocketAddrs, StunBindConfigValue, StunChangePort},
};

/// 本节点的 STUN 运行时配置。
///
/// 这不是配置文件 AST 的原样映射，而是把 `server` 块里的以下配置归一化后的结果：
/// - `stun on|off;`
/// - `relay on|off;`
/// - 多个 `stun_server { ... }`
///
/// 最终行为由内容决定：
/// - 配置了 `stun_server { ... }` → 走 configured 模式，独立绑定主/辅 socket
/// - 只有 `stun on;` → 走 dynamic 模式，等本地 listener 被判定为 `FullCone` 后寄生启动
///
/// `relay` 目前只是配置侧保留字段，本文件中的 STUN server 生命周期并不会直接使用它。
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
    /// 绑定地址
    pub bind_address: SocketAddr,
    /// 对外发布地址
    pub outer_address: Option<SocketAddr>,
    /// STUN `ChangedAddress` / `CHANGE_IP` 对应的完整替代地址（包含 IP 和端口）
    pub change_address: Option<SocketAddr>,
    /// 辅助端口
    pub change_port: Option<u16>,
}

impl StunNodeConfig {
    /// 从 `server` 配置节点提取 STUN 相关配置。
    ///
    /// 两种启用方式满足其一即可：
    /// - `stun on;`
    /// - 存在至少一个 `stun_server { ... }`
    pub fn from_server_node(server: &Arc<ConfigNode>) -> Option<Self> {
        let stun_enabled = server
            .get::<BoolConfig>("stun")
            .ok()
            .flatten()
            .map(|value| value.0)
            .unwrap_or(false);

        let binds: Vec<StunBindConfig> = server
            .children_optional("stun_server")
            .iter()
            .flat_map(|node| {
                let outer_address = first_addr(node, "outer_addr");
                let change_address = first_addr(node, "change_addr");
                let change_port = node
                    .get::<StunChangePort>("change_port")
                    .ok()
                    .flatten()
                    .map(|port| port.0);

                node.get_all::<StunBindConfigValue>("bind")
                    .ok()
                    .into_iter()
                    .flatten()
                    .map(move |bind| StunBindConfig {
                        bind_address: bind.bind,
                        outer_address: bind.outer_addr.or(outer_address),
                        change_address: bind.change_addr.or(change_address),
                        change_port: bind.change_port.or(change_port),
                    })
            })
            .collect();

        if !stun_enabled && binds.is_empty() {
            return None;
        }

        let relay = server
            .get::<BoolConfig>("relay")
            .ok()
            .flatten()
            .map(|value| value.0)
            .unwrap_or(false);

        Some(Self { relay, binds })
    }

    /// 是否配置了 bind 块
    pub fn has_configured_binds(&self) -> bool {
        !self.binds.is_empty()
    }

    /// 收集 configured 模式下需要绑定的所有地址
    pub fn configured_addrs(&self) -> HashSet<SocketAddr> {
        self.binds.iter().map(|b| b.bind_address).collect()
    }
}

fn first_addr(node: &ConfigNode, name: &str) -> Option<SocketAddr> {
    node.get::<SocketAddrs>(name)
        .ok()
        .flatten()
        .and_then(|addrs| addrs.0.first().copied())
}

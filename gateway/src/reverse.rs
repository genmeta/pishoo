mod file;
pub(crate) mod gzip;
pub mod location;
pub(crate) mod log;
pub mod middleware;
mod proxy;
pub mod router;
#[cfg(feature = "sshd")]
mod sshd;
mod upstream_tls;

/*
 - PhysicalInterfaces:
    - 监听网络设备变化
    - 自动触发Interface的rebind
    - 发布InterfaceEvents供其他模块订阅网络变化，来添加/移除监听地址等

 - QuicListeners：
    - 初始化时
     - 根据listen配置，进行第一次绑定
    - 订阅Locations监听变化
     - 根据server的listen配置，响应变化（移除/添加bind地址）

 - DNS发布任务
    - 订阅Locations监听变化
     - 根据server的listen和resolver配置
    - 响应变化（移除/添加mDNS Resolver）
     - 进行重新发布

 - QuicClient：
    - 初始化时
     - 根据listen配置，进行第一次绑定
    - 订阅Locations监听变化
     - 根据client的listen配置，响应变化（移除/添加bind地址）
*/

#[derive(Debug, Clone, Copy)]
pub enum MissingRulePolicy {
    Allow,
    Deny,
}

pub fn normalize_server_name(server_name: &str) -> String {
    match server_name.strip_suffix('~') {
        Some(prefix) => format!("{prefix}.genmeta.net"),
        None => server_name.to_string(),
    }
}

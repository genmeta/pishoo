use std::sync::Arc;

use gateway::{forward, reverse};
use tokio::{sync::Mutex, task::JoinSet};

// 启动所有服务：reverse 与多个 forward
pub fn start_services(
    handler: &mut JoinSet<()>,
    servers: &[Arc<gateway::parse::Node>],
    proxys: &[Arc<gateway::parse::Node>],
) {
    // reverse
    {
        let servers_cloned = servers.to_vec();
        let task = async move {
            if let Err(error) = reverse::serve(servers_cloned).await {
                tracing::error!(target: "reverse", "Reverse proxy failed: {error:?}")
            }
        };
        handler.spawn(task);
    }

    // forwards
    for proxy in proxys.iter().cloned() {
        let task = async move {
            match forward::serve(proxy).await {
                Ok((_bind_addr, forward_proxy)) => {
                    if let Err(error) = forward_proxy.await {
                        tracing::error!(target: "forward", "Forward proxy error: {error:?}");
                    }
                }
                Err(launch_error) => {
                    tracing::error!(target: "forward", "Failed to launch forward proxy: {launch_error:?}");
                }
            };
        };
        handler.spawn(task);
    }
}

// 停止所有服务并等待退出
pub async fn stop_services(handler: &Arc<Mutex<JoinSet<()>>>) -> anyhow::Result<()> {
    use tokio::time::{Duration, timeout};

    tracing::info!(target: "services", "Stopping services...");

    let mut h = handler.lock().await;

    h.abort_all();

    // 设置一个合理的超时，避免无限等待
    let join_all = async {
        while let Some(res) = h.join_next().await {
            if let Err(e) = res {
                if e.is_cancelled() {
                    continue;
                }
                tracing::error!(target: "services", "Service task error: {}", e);
            }
        }
    };
    match timeout(Duration::from_secs(5), join_all).await {
        Ok(_) => {
            tracing::info!(target: "services", "Services stopped");
            *h = JoinSet::new();
            Ok(())
        }
        Err(_) => {
            tracing::error!(target: "services", "Stop services timeout");
            *h = JoinSet::new();
            Err(anyhow::anyhow!("Stop services timeout"))
        }
    }
}

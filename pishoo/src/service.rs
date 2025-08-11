use std::sync::Arc;

use gateway::{forward, reverse};
use tokio::{sync::broadcast, task::JoinSet};

// 启动所有服务：reverse 与多个 forward
pub fn start_services(
    handler: &mut JoinSet<anyhow::Result<()>>,
    servers: &[Arc<gateway::parse::Node>],
    proxys: &[Arc<gateway::parse::Node>],
    mut stop_rx: Option<broadcast::Receiver<()>>,
) {
    // reverse
    {
        let servers_cloned = servers.to_vec();
        let rx_for_reverse = stop_rx.take();
        let reverse_fut = reverse::serve(servers_cloned, rx_for_reverse);
        handler.spawn(async move {
            reverse_fut
                .await
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!(e.to_string()))
        });
    }

    // forwards
    for proxy in proxys {
        let proxy_node = Arc::clone(proxy);
        let rx_for_forward = stop_rx.take();
        let forward_fut = forward::serve(proxy_node, rx_for_forward);
        handler.spawn(async move {
            forward_fut
                .await
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!(e.to_string()))
        });
    }
}

// 停止所有服务并等待退出
pub async fn stop_services(
    shutdown_tx: &broadcast::Sender<()>,
    handler: &Arc<tokio::sync::Mutex<JoinSet<anyhow::Result<()>>>>,
) -> anyhow::Result<()> {
    use tokio::time::{Duration, timeout};

    tracing::info!("stopping services...");
    let _ = shutdown_tx.send(());

    let mut h = handler.lock().await;

    // 设置一个合理的超时，避免无限等待
    let res = timeout(Duration::from_secs(5), async {
        while let Some(res) = h.join_next().await {
            if let Err(e) = res {
                tracing::error!("service task error: {}", e);
            }
        }
    })
    .await;

    match res {
        Ok(_) => {
            tracing::info!("services stopped");
            *h = JoinSet::new();
            Ok(())
        }
        Err(_) => {
            tracing::error!("stop services timeout");
            Err(anyhow::anyhow!("stop services timeout"))
        }
    }
}

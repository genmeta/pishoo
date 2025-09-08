use std::sync::Arc;

use anyhow::Context;
use firewall_db::{
    base::matcher::{DomainRulesMatcher, LocationRulesMatcher},
    sea_orm::Database,
    service::{domain_service::DomainService, location_service::LocationService},
};
use gateway::{
    forward,
    parse::{Node, Value},
    reverse,
};
use tokio::{sync::Mutex, task::JoinSet};

use crate::{PID_FILE_DEFAULT, signal};

pub async fn start_services_from_pishoo_block(
    handler: &Mutex<JoinSet<()>>,
    pishoo: &Node, // Pishoo block
) -> anyhow::Result<()> {
    let pid_file = if let Some(Value::String(pid)) = pishoo.get("pid") {
        pid
    } else {
        PID_FILE_DEFAULT
    };

    let access_rules = if let Some(Value::String(db_uri)) = pishoo.get("access_rules") {
        db_uri
    } else {
        firewall_db::DEFAULT_DB_URI
    };

    let db = Database::connect(access_rules)
        .await
        .context("Failed to connect to firewall database")?;
    firewall_db::initial_db(&db)
        .await
        .context("Failed to initialize firewall database")?;
    let domain_rules = DomainService::new(&db)
        .list_all_rules()
        .await
        .context("Failed to load domain rules from firewall database")?;
    let location_rules = LocationService::new(&db)
        .list_all_rules()
        .await
        .context("Failed to load location rules from firewall database")?;
    let access_rules = (
        Arc::new(DomainRulesMatcher::from(domain_rules)),
        Arc::new(LocationRulesMatcher::from(location_rules)),
    );

    #[cfg(unix)]
    signal::init_pid_file(pid_file).await?;

    let pishoo = if let Some(Value::Nodes(pishoo)) = pishoo.get("pishoo") {
        Arc::clone(pishoo.first().unwrap())
    } else {
        return Err(anyhow::anyhow!("pishoo block not found"));
    };

    let proxys = if let Some(Value::Nodes(pishoo)) = pishoo.get("proxy") {
        pishoo
    } else {
        &Vec::new()
    };

    let servers = if let Some(Value::Nodes(servers)) = pishoo.get("server") {
        servers
    } else {
        &Vec::new()
    };

    start_services(&mut *handler.lock().await, access_rules, servers, proxys);

    Ok(())
}

// 启动所有服务：reverse 与多个 forward
pub fn start_services(
    handler: &mut JoinSet<()>,
    access_rules: (Arc<DomainRulesMatcher>, Arc<LocationRulesMatcher>),
    servers: &[Arc<gateway::parse::Node>],
    proxys: &[Arc<gateway::parse::Node>],
) {
    // reverse
    {
        let access_rules = access_rules.clone();
        let servers = servers.to_vec();
        let task = async move {
            if let Err(error) = reverse::serve(access_rules, servers).await {
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
pub async fn stop_services(handler: &Arc<Mutex<JoinSet<()>>>) {
    tracing::info!(target: "services", "Stopping services...");

    let mut h = handler.lock().await;

    h.abort_all();

    while let Some(res) = h.join_next().await {
        if let Err(e) = res {
            if e.is_cancelled() {
                continue;
            }
            tracing::error!(target: "services", "Service task error: {}", e);
        }
    }

    tracing::info!(target: "services", "Services stopped");
    *h = JoinSet::new();
}

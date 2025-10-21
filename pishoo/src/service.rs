use std::sync::Arc;

use firewall_db::{
    base::matcher::{DomainRulesMatcher, LocationRulesMatcher},
    service::{domain_service::DomainService, location_service::LocationService},
};
use gateway::{
    error::Whatever,
    forward,
    parse::{Node, Value},
    reverse,
};
use sea_orm::{ConnectOptions, Database};
use snafu::ResultExt;
use tokio::{sync::Mutex, task::JoinSet};

pub async fn start_services_from_pishoo_block(
    handler: &Mutex<JoinSet<()>>,
    pishoo: &Node, // Pishoo block
) -> Result<(), Whatever> {
    #[cfg(unix)]
    let pid_file = if let Some(Value::String(pid)) = pishoo.get("pid") {
        pid
    } else {
        crate::PID_FILE_DEFAULT
    };

    let access_rules = if let Some(Value::String(db_uri)) = pishoo.get("access_rules") {
        db_uri
    } else {
        firewall_db::DEFAULT_DB_URI
    };

    let access_rules = async {
        let mut connect_options = ConnectOptions::new(access_rules);
        connect_options.sqlx_logging_level("debug".parse().unwrap());
        let db = Database::connect(connect_options)
            .await
            .whatever_context("Failed to connect to firewall database")?;
        firewall_db::initial_database(&db)
            .await
            .whatever_context("Failed to initialize firewall database")?;
        let domain_rules = DomainService::new(&db)
            .list_all_rules()
            .await
            .whatever_context("Failed to load domain rules from firewall database")?;
        let location_rules = LocationService::new(&db)
            .list_all_rules()
            .await
            .whatever_context("Failed to load location rules from firewall database")?;
        Result::<_, Whatever>::Ok((
            Arc::new(DomainRulesMatcher::from(domain_rules)),
            Arc::new(LocationRulesMatcher::from(location_rules)),
        ))
    }
    .await
    .whatever_context(format!("Failed to load access rules `{}`", access_rules))?;

    #[cfg(unix)]
    crate::signal::init_pid_file(pid_file).await?;

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
    tracing::info!(target: "services", "Services started");
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

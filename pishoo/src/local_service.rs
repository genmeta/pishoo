use std::{collections::HashSet, sync::Arc};

use firewall_db::base::matcher::LocationRulesMatcher;
use gateway::{
    error::Whatever,
    parse::{Node, Value},
    reverse::{self, MissingRulePolicy},
};
use gm_quic::qinterface::device::Devices;
use h3x::dhttp::settings::Settings;
use snafu::{FromString, Report, ResultExt, whatever};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::{
    bind::resolve_bind_uris, naming::canonicalize_server_nodes, root_state::RootState, tls,
};

struct LocalServerDef {
    server_name: String,
    bind: Vec<String>,
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
}

pub struct LocalServerRuntime {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl LocalServerRuntime {
    pub fn stop(self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

pub async fn validate_local_servers(servers: &[Arc<Node>]) -> Result<(), Whatever> {
    let _ = collect_local_server_defs(servers).await?;
    Ok(())
}

pub async fn register_local_servers(
    state: &Arc<Mutex<RootState>>,
    servers: &[Arc<Node>],
    access_rules: Arc<LocationRulesMatcher>,
) -> Result<Vec<LocalServerRuntime>, Whatever> {
    let canonicalized_servers = canonicalize_server_nodes(servers)?;
    let defs = collect_local_server_defs(&canonicalized_servers).await?;
    let router = reverse::build_router_for_servers(&canonicalized_servers);
    let h3_settings = Arc::new(Settings::default());
    let mut runtimes = Vec::with_capacity(defs.len());

    for def in defs {
        let (tx, mut rx) = mpsc::channel(128);
        let cancel = CancellationToken::new();
        let cancel_child = cancel.clone();
        let server_name = def.server_name.clone();
        let server_name_for_task = server_name.clone();
        let router_for_task = router.clone();
        let access_rules_for_task = access_rules.clone();
        let h3_settings_for_task = h3_settings.clone();
        let task = tokio::spawn(
            async move {
                loop {
                    tokio::select! {
                        () = cancel_child.cancelled() => break,
                        accepted = rx.recv() => {
                            let Some(conn) = accepted else {
                                break;
                            };
                            let server_name = server_name_for_task.clone();
                            let router = router_for_task.clone();
                            let access_rules = access_rules_for_task.clone();
                            let h3_settings = h3_settings_for_task.clone();
                            tokio::spawn(async move {
                                if let Err(error) = reverse::handle_single_connection_for_server(
                                    conn,
                                    server_name,
                                    h3_settings,
                                    router,
                                    access_rules,
                                    MissingRulePolicy::Deny,
                                ).await {
                                    tracing::warn!(error = %Report::from_error(&error), "local connection handling failed");
                                }
                            }.in_current_span());
                        }
                    }
                }
            }
            .in_current_span(),
        );

        state
            .lock()
            .await
            .register_local_server(
                server_name,
                def.bind,
                &def.cert_pem,
                &def.key_pem,
                tx,
                cancel.clone(),
            )
            .await
            .map_err(|error| {
                Whatever::without_source(format!("failed to register local server: {error}"))
            })?;

        runtimes.push(LocalServerRuntime { cancel, task });
    }

    Ok(runtimes)
}

async fn collect_local_server_defs(servers: &[Arc<Node>]) -> Result<Vec<LocalServerDef>, Whatever> {
    let device_names = Devices::global()
        .interfaces()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let mut seen_server_names = HashSet::new();
    let mut defs = Vec::new();

    for server in servers {
        let Some(Value::Listen(listens)) = server.get("listen") else {
            whatever!("local server missing `listen`");
        };
        let listens = listens.clone();
        let Some(Value::ServerName(server_names)) = server.get("server_name") else {
            whatever!("local server missing `server_name`");
        };
        let server_names = server_names.clone();
        let Some(Value::Path(cert_path)) = server.get("ssl_certificate") else {
            whatever!("local server missing `ssl_certificate`");
        };
        let cert_path = cert_path.clone();
        let Some(Value::Path(key_path)) = server.get("ssl_certificate_key") else {
            whatever!("local server missing `ssl_certificate_key`");
        };
        let key_path = key_path.clone();

        let cert_pem = tokio::fs::read(&cert_path).await.whatever_context(format!(
            "failed to read local certificate file `{}`",
            cert_path.display()
        ))?;
        let key_pem = tokio::fs::read(&key_path).await.whatever_context(format!(
            "failed to read local private key file `{}`",
            key_path.display()
        ))?;
        let _ = tls::validate_tls_material(&cert_pem, &key_pem)
            .whatever_context("invalid local tls material")?;

        let bind = resolve_bind_uris(&listens, &device_names);
        if bind.is_empty() {
            whatever!("local server has no resolved bind uris");
        }

        for configured_name in server_names {
            let server_name = configured_name.name;
            if !seen_server_names.insert(server_name.clone()) {
                whatever!("duplicate local server_name `{server_name}` in entry config");
            }
            defs.push(LocalServerDef {
                server_name,
                bind: bind.clone(),
                cert_pem: cert_pem.clone(),
                key_pem: key_pem.clone(),
            });
        }
    }

    Ok(defs)
}

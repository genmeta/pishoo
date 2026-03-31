use std::{collections::HashSet, sync::Arc};

use gateway::{
    error::Whatever,
    parse::{Node, Value},
};
use snafu::{ResultExt, whatever};

use crate::tls;

#[allow(dead_code)]
struct LocalServerDef {
    server_name: String,
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
}

pub async fn validate_local_servers(servers: &[Arc<Node>]) -> Result<(), Whatever> {
    let _ = collect_local_server_defs(servers).await?;
    Ok(())
}

async fn collect_local_server_defs(servers: &[Arc<Node>]) -> Result<Vec<LocalServerDef>, Whatever> {
    let mut seen_server_names = HashSet::new();
    let mut defs = Vec::new();

    for server in servers {
        let Some(Value::Listen(listens)) = server.get("listen") else {
            whatever!("local server missing `listen`");
        };
        if listens.is_empty() {
            whatever!("local server has empty listen specification");
        }
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

        for configured_name in server_names {
            let server_name = configured_name.name;
            if !seen_server_names.insert(server_name.clone()) {
                whatever!("duplicate local server_name `{server_name}` in entry config");
            }
            defs.push(LocalServerDef {
                server_name,
                cert_pem: cert_pem.clone(),
                key_pem: key_pem.clone(),
            });
        }
    }

    Ok(defs)
}

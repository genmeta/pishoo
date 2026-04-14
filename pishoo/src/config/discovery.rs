use std::sync::Arc;

use dhttp_home::identity::IdentityHome;
use gateway::{
    error::Whatever,
    parse::{Node, Value},
};
use snafu::ResultExt;

use crate::naming::canonicalize_server_nodes;

pub async fn load_identity_servers(
    identity_home: &IdentityHome,
) -> Result<Vec<Arc<Node>>, Whatever> {
    let conf_path = identity_home.server_conf_path();
    let raw = tokio::fs::read(&conf_path).await.whatever_context(format!(
        "failed to read identity config `{}`",
        conf_path.display()
    ))?;
    let parsed =
        gateway::parse::parse_server_config(&raw, identity_home).whatever_context(format!(
            "failed to parse identity server config `{}`",
            conf_path.display()
        ))?;
    let Some(Value::Nodes(server_nodes)) = parsed.get("server") else {
        return Ok(Vec::new());
    };

    canonicalize_server_nodes(server_nodes)
}

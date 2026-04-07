use std::sync::Arc;

use dhttp_home::identity::Name;
use gateway::{
    error::Whatever,
    parse::{Node, ServerName, Value},
};
use snafu::{FromString, whatever};

pub fn canonicalize_genmeta_name(name: &str) -> Result<String, Whatever> {
    if let Some(expanded) = Name::try_expand_from(name).map_err(|error| {
        Whatever::with_source(Box::new(error), format!("invalid server_name `{name}`"))
    })? {
        return Ok(expanded.as_full().to_string());
    }

    Name::try_from_str_partial(name)
        .map(|expanded| expanded.as_full().to_string())
        .map_err(|error| {
            Whatever::with_source(Box::new(error), format!("invalid server_name `{name}`"))
        })
}

pub fn canonicalize_server_names(server_names: &[ServerName]) -> Result<Vec<ServerName>, Whatever> {
    server_names
        .iter()
        .map(|server_name| {
            canonicalize_genmeta_name(&server_name.name).map(|name| ServerName { name })
        })
        .collect()
}

pub fn canonicalize_server_nodes(servers: &[Arc<Node>]) -> Result<Vec<Arc<Node>>, Whatever> {
    servers.iter().map(canonicalize_server_node).collect()
}

pub fn canonicalize_server_node(server: &Arc<Node>) -> Result<Arc<Node>, Whatever> {
    let Value::ValueMap(mut values) = server.value().clone() else {
        whatever!("server node must be a value map");
    };

    let Some(Value::ServerName(server_names)) = values.get("server_name").cloned() else {
        whatever!("server node missing `server_name`");
    };

    values.insert(
        "server_name".to_string(),
        Value::ServerName(canonicalize_server_names(&server_names)?),
    );

    Ok(Arc::new(Node::new(Value::ValueMap(values))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_accepts_partial_and_tilde_names() {
        assert_eq!(
            canonicalize_genmeta_name("borber.pilot").expect("partial name should expand"),
            "borber.pilot.genmeta.net"
        );
        assert_eq!(
            canonicalize_genmeta_name("borber.pilot~").expect("tilde name should expand"),
            "borber.pilot.genmeta.net"
        );
        assert_eq!(
            canonicalize_genmeta_name("borber.pilot.genmeta.net")
                .expect("full name should stay full"),
            "borber.pilot.genmeta.net"
        );
    }
}

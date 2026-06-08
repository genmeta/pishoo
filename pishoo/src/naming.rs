use std::sync::Arc;

use dhttp::name::DhttpName;
use gateway::{
    error::Whatever,
    parse::{document::ConfigNode, types::ServerName},
};
use snafu::ResultExt;

pub fn canonicalize_genmeta_name(name: &str) -> Result<DhttpName<'static>, Whatever> {
    DhttpName::try_from(name.to_owned()).whatever_context(format!("invalid server_name `{name}`"))
}

pub fn canonicalize_server_names(server_names: &[ServerName]) -> Result<Vec<ServerName>, Whatever> {
    server_names
        .iter()
        .map(|server_name| {
            Ok(ServerName {
                name: server_name.name.clone(),
            })
        })
        .collect()
}

pub fn canonicalize_server_nodes(
    servers: &[Arc<ConfigNode>],
) -> Result<Vec<Arc<ConfigNode>>, Whatever> {
    Ok(servers.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_accepts_partial_and_tilde_names() {
        assert_eq!(
            canonicalize_genmeta_name("borber.pilot").expect("partial name should expand"),
            DhttpName::try_from("borber.pilot").unwrap()
        );
        assert_eq!(
            canonicalize_genmeta_name("borber.pilot~").expect("tilde name should expand"),
            DhttpName::try_from("borber.pilot").unwrap()
        );
        assert_eq!(
            canonicalize_genmeta_name("borber.pilot.dhttp.net")
                .expect("full name should stay full"),
            DhttpName::try_from("borber.pilot").unwrap()
        );
    }
}

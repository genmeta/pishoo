use std::collections::HashSet;

use gateway::parse::Listens;

pub fn resolve_bind_uris(listens: &[Listens], device_names: &[String]) -> Vec<String> {
    listens
        .iter()
        .flat_map(|listen| listen.resolve(device_names.iter().map(String::as_str)))
        .filter(|uri| uri.resolve().is_ok())
        .map(|uri| uri.to_string())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

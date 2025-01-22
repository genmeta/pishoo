#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum ServerType {
    Forward,
    #[default]
    Reverse,
}

pub fn parse_server_type(version: &str) -> ServerType {
    match version {
        "http3" => ServerType::Forward,
        _ => ServerType::Reverse,
    }
}

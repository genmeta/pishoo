//! Identity-bound HTTP services, components and management routes.
mod contact;
mod dns;
mod error;
mod package;
mod routes;
mod sandbox;
mod server;
mod setup;

#[cfg(test)]
#[path = "../tests/support/test_identity.rs"]
mod test_identity;

#[cfg(test)]
#[path = "../tests/support/network_credentials.rs"]
mod test_credentials;

pub use error::Error;
pub use package::{validate_lib, validate_openapi};
pub use sandbox::WasmRuntime;
pub use server::run;

type Body = dhttp::Body;
type Result<T> = std::result::Result<T, Error>;

/// Local service configuration used by gmutils; Pishoo owns schema initialization.
pub fn configure(
    profile: &dhttp_home::identity::IdentityProfile,
    mode: Option<&str>,
    initialize: bool,
) -> Result<serde_json::Value> {
    if initialize {
        setup::load_server_config(profile)?;
    }
    setup::config_database(
        profile,
        &if mode.is_some() {
            http::Method::PATCH
        } else {
            http::Method::GET
        },
        &"/std/sys/listen".parse().unwrap(),
        mode.map(|listen| serde_json::json!({"listen":listen})),
    )
}

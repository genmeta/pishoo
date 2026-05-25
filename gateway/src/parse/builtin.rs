pub mod access;
pub mod core;
pub mod http;
pub mod location;
pub mod net;
pub mod pishoo;
pub mod proxy;
pub mod server;
pub mod ssh;
pub mod stun;

use crate::parse::registry::ConfigRegistry;

pub fn register_gateway_directives(registry: &mut ConfigRegistry) {
    pishoo::register(registry);
    server::register(registry);
    location::register(registry);
    proxy::register(registry);
    stun::register(registry);
}

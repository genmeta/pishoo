//! Guest bindings for the identity interfaces provided by the Pishoo host.
//!
//! These functions call WASM imports. Execute the resulting component in Pishoo;
//! HTTP headers and guest data cannot supply the authenticated identity.

mod bindings {
    wit_bindgen::generate!({
        path: "pishoo-identity",
        inline: "package pishoo:sdk; world pishoo-sdk { import pishoo:identity/request-identity@0.1.0; import pishoo:identity/signatures@0.1.0; }",
        generate_all,
    });
}

pub use bindings::pishoo::identity::{request_identity, signatures};

mod bindings_v2 {
    wit_bindgen::generate!({
        path: "pishoo-identity-v2",
        inline: "package pishoo:sdk-v2; world pishoo-sdk-v2 { import pishoo:identity/request-identity@0.2.0; }",
        generate_all,
    });
}

/// Current identity API; compare both tuple elements to identify the owner.
pub use bindings_v2::pishoo::identity::request_identity as identity;

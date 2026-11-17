//! Guest bindings for the identity interfaces provided by the Pishoo host.
//!
//! These functions call WASM imports. Execute the resulting component in Pishoo;
//! HTTP headers and guest data cannot supply the authenticated identity.

mod bindings {
    wit_bindgen::generate!({
        path: ".",
        world: "sdk",
        generate_all,
    });
}

/// Current identity API; compare both tuple elements to identify the owner.
pub use bindings::pishoo::identity0_2_0::request_identity as identity;
pub use bindings::pishoo::{
    identity0_1_0::{request_identity, signatures},
    identity0_3_0::call_context as context,
};

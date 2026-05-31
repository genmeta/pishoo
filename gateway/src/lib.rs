mod command;
pub mod control_plane;
pub mod dns;
pub mod error;
pub mod forward;
pub mod parse;
pub mod reverse;
pub mod stun;

pub use h3x::dquic::prelude::EndpointAddr;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_does_not_define_custom_root_ca_wrapper_or_build_script() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let build_script = manifest_dir.join("build.rs");
        let common_source = manifest_dir.join("src/common.rs");

        assert!(!build_script.exists());
        assert!(!common_source.exists());
    }
}

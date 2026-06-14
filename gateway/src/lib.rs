mod command;
pub mod control_plane;
pub mod error;
pub mod forward;
pub mod parse;
pub mod reverse;
pub mod stun;

pub use dhttp::h3x::dquic::prelude::EndpointAddr;

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

    #[test]
    fn crate_does_not_export_legacy_dns_module() {
        let lib_source = include_str!("lib.rs");
        let public_dns_module = ["pub mod ", "dns;"].concat();
        let private_dns_module = ["mod ", "dns;"].concat();

        assert!(!lib_source.contains(&public_dns_module));
        assert!(!lib_source.contains(&private_dns_module));
    }
}

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::parse::document::{ConfigDocument, ConfigNode};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) fn create_temp_file(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "gateway_{prefix}_{}_{}.pem",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, "dummy").expect("write temp config fixture");
    path
}

pub(crate) fn cleanup_temp_files(paths: &[&Path]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

pub(crate) fn parse_doc(conf: &str) -> ConfigDocument {
    crate::parse::parse_config_str_for_test(conf).expect("config should parse")
}

pub(crate) fn first_pishoo(document: &ConfigDocument) -> Arc<ConfigNode> {
    document.root.children("pishoo").expect("pishoo children")[0].clone()
}

pub(crate) fn first_server(document: &ConfigDocument) -> Arc<ConfigNode> {
    first_pishoo(document)
        .children("server")
        .expect("server children")[0]
        .clone()
}

pub(crate) fn build_server_conf(
    server_cert: &Path,
    server_key: &Path,
    server_body: &str,
) -> String {
    format!(
        "pishoo {{ server {{ listen all 5378; server_name example.com; ssl_certificate {}; ssl_certificate_key {}; {} }} }}",
        server_cert.display(),
        server_key.display(),
        server_body
    )
}

pub(crate) fn build_proxy_conf(
    server_cert: &Path,
    server_key: &Path,
    location_body: &str,
) -> String {
    format!(
        r#"
pishoo {{
    server {{
        listen all 5378;
        server_name example.com;
        ssl_certificate {};
        ssl_certificate_key {};
        location /api {{
            {}
        }}
    }}
}}
"#,
        server_cert.display(),
        server_key.display(),
        location_body
    )
}

pub(crate) fn assert_error_chain_display_single_line(error: &(dyn std::error::Error + 'static)) {
    let mut current = Some(error);
    while let Some(error) = current {
        assert!(
            !error.to_string().contains('\n'),
            "error display should be single-line: {}",
            error
        );
        current = error.source();
    }
}

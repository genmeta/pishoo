#[test]
fn main_uses_shared_root_cert_store() {
    let main_source = include_str!("../src/main.rs");
    assert!(
        main_source.contains("pishoo::tls::root_cert_store()"),
        "main must use the shared root cert entry"
    );
}

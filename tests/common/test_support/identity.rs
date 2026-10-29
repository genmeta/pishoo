//! In-memory credentials for tests that do not exercise profile loading or TLS.

pub(crate) fn endpoint(name: &str) -> dhttp::Endpoint {
    let name = dhttp_home::normalize_name(name).unwrap();
    let mut params = rcgen::CertificateParams::new(vec![name.clone()]).unwrap();
    let ski = format!("0:0:{}", "a".repeat(64));
    let mut der = vec![4, ski.len() as u8];
    der.extend_from_slice(ski.as_bytes());
    params
        .custom_extensions
        .push(rcgen::CustomExtension::from_oid_content(
            &[2, 5, 29, 14],
            der,
        ));
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    let identity = qbase::endpoint::Endpoint::new(
        &name,
        vec![cert.der().clone()],
        qtls::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap();
    dhttp::Endpoint::new(identity)
}

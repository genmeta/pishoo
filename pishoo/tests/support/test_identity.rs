//! In-memory credentials for tests that do not exercise profile loading or TLS.

pub(crate) fn endpoint(name: &str) -> dhttp::Endpoint {
    let name = dhttp_home::normalize_name(name).unwrap();
    let certificate = rcgen::generate_simple_self_signed(vec![name.clone()]).unwrap();
    let identity = qbase::endpoint::Endpoint::new(
        &name,
        vec![certificate.cert.der().clone()],
        qtls::PrivateKeyDer::try_from(certificate.signing_key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap();
    dhttp::Endpoint::new(identity)
}

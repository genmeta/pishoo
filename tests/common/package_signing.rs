use super::*;

pub(super) fn verify_fixture_chain(
    chain: &[qtls::CertificateDer<'_>],
    domain: &str,
    at: i64,
) -> Result<()> {
    let root = crate::package::tests::packages::signing_root();
    verify_chain_with_root(chain, domain, at, root.as_ref()).or_else(|_| {
        // A second explicit TEST ONLY anchor consumes the same real Jade
        // bytes as Registry. Packages can never nominate their own root.
        let (_, pem) = x509_parser::pem::parse_x509_pem(include_bytes!(
            "../fixtures/code-signing/root.cert.pem"
        ))
        .map_err(|_| {
            policy(
                "signing_certificate_untrusted",
                http::StatusCode::UNPROCESSABLE_ENTITY,
            )
        })?;
        verify_chain_with_root(chain, domain, at, &pem.contents)
    })
}

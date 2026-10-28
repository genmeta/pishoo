use super::*;

fn endpoint(kind: u8) -> Endpoint {
    let name = "dns-authority-test.dhttp.net";
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
    let ski = format!("1:{kind}:{}", "0123456789abcdef".repeat(4));
    let mut extension = vec![0x04, ski.len() as u8];
    extension.extend_from_slice(ski.as_bytes());
    params
        .custom_extensions
        .push(rcgen::CustomExtension::from_oid_content(
            &[2, 5, 29, 14],
            extension,
        ));
    let cert = params.self_signed(&key).unwrap();
    Endpoint::new(
        qbase::endpoint::Endpoint::new(
            name,
            vec![cert.der().clone()],
            qtls::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
            vec![1],
        )
        .unwrap(),
    )
}

#[test]
fn publication_requires_primary_credentials() {
    let primary = endpoint(0);
    assert_eq!(authority(&primary).unwrap().name(), primary.name());
    assert!(publisher(&primary).is_ok());
    assert_eq!(
        authority(&endpoint(1)).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(publisher(&endpoint(1)).is_err());
}

#[tokio::test(start_paused = true)]
async fn failed_publication_schedules_a_finite_retry() {
    let endpoint = endpoint(0);
    let publisher =
        Arc::new(H3Resolver::new("https://dns.invalid/".parse().unwrap(), &endpoint).unwrap());
    let started = Instant::now();
    // Unit tests do not install a network; the error must schedule maintenance.
    let due = publish(
        endpoint.name().to_owned(),
        publisher,
        Arc::from(["127.0.0.1:8080".parse::<SocketAddr>().unwrap().into()]),
    )
    .await
    .unwrap();
    assert_eq!(due, started + MAINTENANCE_RETRY);
}

#[tokio::test]
async fn empty_publication_stops_renewal_without_network() {
    let endpoint = endpoint(0);
    let publisher =
        Arc::new(H3Resolver::new("https://dns.invalid/".parse().unwrap(), &endpoint).unwrap());
    assert_eq!(
        publish(endpoint.name().to_owned(), publisher, Arc::from([])).await,
        None
    );
}

#[tokio::test]
async fn empty_mdns_maintenance_needs_no_network() {
    let mdns = MdnsResolverSet::new(ddns::resolvers::DHTTP_MDNS_SERVICE_DOMAIN);
    maintain_mdns(&mdns, &[], &[]).await.unwrap();
    mdns.shutdown().await.unwrap();
}

mod mdns;

use super::*;

#[test]
fn outbound_prefix_preserves_target_path_and_query() {
    for (incoming, expected) in [
        (
            "https://alice.dhttp.net/.pishoo/dhttp/bob.dhttp.net",
            "https://bob.dhttp.net/",
        ),
        (
            "https://alice.dhttp.net/.pishoo/dhttp/bob~/contact?x=1",
            "https://bob.dhttp.net/contact?x=1",
        ),
        (
            "https://alice.dhttp.net/.pishoo/dhttp/bob.dhttp.net:7/a%2Fb?q=%2F",
            "https://bob.dhttp.net:7/a%2Fb?q=%2F",
        ),
    ] {
        let (uri, _) = outbound_uri(&incoming.parse().unwrap()).unwrap();
        assert_eq!(uri.to_string(), expected);
    }
}

#[test]
fn outbound_prefix_rejects_invalid_targets() {
    for incoming in [
        "https://alice.dhttp.net/.pishoo/dhttp/",
        "https://alice.dhttp.net/.pishoo/dhttp/bad_name/a",
        "https://alice.dhttp.net/.pishoo/dhttp/bob.dhttp.net:wrong/a",
        "https://alice.dhttp.net/.pishoo/dhttp/bob.dhttp.net:2147483648/a",
    ] {
        assert!(
            outbound_uri(&incoming.parse().unwrap()).is_err(),
            "{incoming}"
        );
    }
}

#[tokio::test]
async fn anonymous_caller_cannot_use_outbound_proxy() {
    use std::sync::Arc;

    let name = "alice.dhttp.net";
    let endpoint = dhttp::Endpoint::load(name).await.unwrap();
    let cert = rcgen::generate_simple_self_signed(vec![name.into()]).unwrap();
    let local = dhttp::LocalAuthority::new(
        &qtls::default_provider(),
        Arc::from(name),
        vec![cert.cert.der().clone()],
        qtls::PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap();
    let mut request = Request::builder()
        .uri(format!(
            "https://{name}/.pishoo/dhttp/bob.dhttp.net/contact"
        ))
        .body(AxumBody::empty())
        .unwrap();
    request.extensions_mut().insert(dhttp::HandshakeSummary {
        alpn: None,
        local: Some(local),
        remote: None,
    });

    let response = forward_dhttp(endpoint, request).await;
    assert_eq!(response.status(), http::StatusCode::FORBIDDEN);
}

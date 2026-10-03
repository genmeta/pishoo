use super::*;

#[tokio::test]
async fn upload_streams_through_a_bounded_writer_and_sends_eof() {
    use bytes::Bytes;
    use http_body::Frame;
    use http_body_util::StreamBody;
    use tokio::io::AsyncReadExt;

    let payload = Bytes::from(vec![42; 128 * 1024]);
    let frames = futures::stream::iter([
        Ok::<_, dhttp::BoxError>(Frame::data(payload.slice(..1024))),
        Ok(Frame::data(payload.slice(1024..))),
    ]);
    let (mut writer, mut reader) = tokio::io::duplex(1024);
    let (upload, received) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(
            upload_body(AxumBody::new(StreamBody::new(frames)), &mut writer),
            async {
                let mut received = Vec::new();
                reader.read_to_end(&mut received).await.unwrap();
                received
            }
        )
    })
    .await
    .unwrap();
    upload.unwrap();
    assert_eq!(received, payload);
}

#[tokio::test]
async fn upload_rejects_trailers_and_body_errors() {
    use bytes::Bytes;
    use http_body::Frame;
    use http_body_util::StreamBody;

    let mut trailers = http::HeaderMap::new();
    trailers.insert("x-tag", http::HeaderValue::from_static("value"));
    let frames = futures::stream::iter([
        Ok::<_, dhttp::BoxError>(Frame::data(Bytes::from_static(b"before trailers"))),
        Ok(Frame::trailers(trailers)),
    ]);
    let result = upload_body(
        AxumBody::new(StreamBody::new(frames)),
        &mut tokio::io::sink(),
    )
    .await;
    assert!(matches!(result, Err(Error::BadRequest(message)) if message.contains("trailers")));

    let frames = futures::stream::iter([Err::<Frame<Bytes>, dhttp::BoxError>(Box::new(
        std::io::Error::other("source failed"),
    ))]);
    let result = upload_body(
        AxumBody::new(StreamBody::new(frames)),
        &mut tokio::io::sink(),
    )
    .await;
    assert!(matches!(result, Err(Error::Io(_))));
}

#[test]
fn outbound_prefix_preserves_target_path_and_query() {
    for (incoming, expected) in [
        (
            "https://alice.dhttp.net/.pishoo/dhttp/bob.dhttp.net",
            "https://bob.dhttp.net/",
        ),
        (
            "https://alice.dhttp.net/.pishoo/dhttp/bob.dhttp.net/?x=1",
            "https://bob.dhttp.net/?x=1",
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
    let endpoint = crate::test_identity::endpoint(name);
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

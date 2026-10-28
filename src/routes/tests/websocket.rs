use std::{convert::Infallible, io, net::SocketAddr, time::Duration};

use bytes::Bytes;
use futures::{SinkExt, StreamExt, TryStreamExt};
use http_body::Frame;
use http_body_util::{Empty, Full, StreamBody};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, handshake::derive_accept_key, protocol::Role},
};
use tokio_util::io::{ReaderStream, StreamReader};

use super::*;
use crate::Body;

pub(crate) async fn ha_upstream() -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let service = service_fn(move |mut request: Request<Incoming>| async move {
                    if request.method() == Method::GET && request.uri().path() == "/api/states" {
                        return Ok::<_, Infallible>(
                            http::Response::builder()
                                .header(header::CONTENT_TYPE, "application/json")
                                .body(
                                    Full::new(Bytes::from_static(b"[]"))
                                        .map_err(|never| match never {})
                                        .boxed_unsync(),
                                )
                                .unwrap(),
                        );
                    }
                    assert_eq!(request.method(), Method::GET);
                    assert_eq!(request.uri(), "/api/websocket?case=ha");
                    assert_eq!(request.headers()[header::HOST], address.to_string());
                    assert_eq!(request.headers()[header::UPGRADE], "websocket");
                    assert_eq!(request.headers()[header::CONNECTION], "Upgrade");
                    assert_eq!(request.headers()["sec-websocket-version"], "13");
                    assert_eq!(
                        request.headers()[header::ORIGIN],
                        "https://receiver.dhttp.net"
                    );
                    assert_eq!(request.headers()[header::COOKIE], "session=test");
                    assert_eq!(request.headers()[header::AUTHORIZATION], "Bearer test");
                    assert!(!request.headers().contains_key("x-drop"));
                    assert!(!request.headers().contains_key(header::CONTENT_LENGTH));
                    let accept =
                        derive_accept_key(request.headers()["sec-websocket-key"].as_bytes());
                    let upgrade = hyper::upgrade::on(&mut request);
                    tokio::spawn(async move {
                        let upstream = upgrade.await.unwrap();
                        let mut ws = WebSocketStream::from_raw_socket(
                            TokioIo::new(upstream),
                            Role::Server,
                            None,
                        )
                        .await;
                        ws.send(Message::Text(
                            r#"{"type":"auth_required","ha_version":"test"}"#.into(),
                        ))
                        .await
                        .unwrap();
                        while let Some(message) = ws.next().await {
                            let Ok(message) = message else { break };
                            let reply = match message {
                                Message::Text(text) => {
                                    let value: serde_json::Value =
                                        serde_json::from_str(&text).unwrap();
                                    match value["type"].as_str().unwrap() {
                                        "auth" => {
                                            assert_eq!(value["access_token"], "test-token");
                                            Message::Text(
                                                r#"{"type":"auth_ok","ha_version":"test"}"#.into(),
                                            )
                                        }
                                        "subscribe_events" => {
                                            ws.send(Message::Text(r#"{"id":1,"type":"result","success":true,"result":null}"#.into())).await.unwrap();
                                            for id in 0..8 {
                                                ws.send(Message::Text(serde_json::json!({"id":1,"type":"event","event":{"sequence":id}}).to_string())).await.unwrap();
                                            }
                                            continue;
                                        }
                                        "ping" => Message::Text(format!(
                                            r#"{{"id":{},"type":"pong"}}"#,
                                            value["id"]
                                        )),
                                        _ => panic!("unexpected HA command"),
                                    }
                                }
                                Message::Ping(data) => Message::Pong(data),
                                Message::Pong(_) => continue,
                                Message::Close(_) => {
                                    let _ = ws.flush().await;
                                    break;
                                }
                                message => message,
                            };
                            if ws.send(reply).await.is_err() {
                                break;
                            }
                        }
                    });
                    Ok::<_, Infallible>(
                        http::Response::builder()
                            .status(StatusCode::SWITCHING_PROTOCOLS)
                            .header(header::CONNECTION, "Upgrade")
                            .header(header::UPGRADE, "websocket")
                            .header("sec-websocket-accept", accept)
                            .header("sec-websocket-protocol", "ha")
                            .header("sec-websocket-extensions", "permessage-deflate")
                            .header(header::SET_COOKIE, "upstream=ok")
                            .body(
                                Empty::<Bytes>::new()
                                    .map_err(|never| match never {})
                                    .boxed_unsync(),
                            )
                            .unwrap(),
                    )
                });
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(socket), service)
                    .with_upgrades()
                    .await
                    .unwrap();
            });
        }
    });
    (address, task)
}

pub(crate) fn ha_request<B>(uri: &str, body: B) -> Request<B> {
    Request::builder()
        .method(Method::CONNECT)
        .version(http::Version::HTTP_3)
        .uri(uri)
        .extension(Arc::<str>::from("websocket"))
        .header("sec-websocket-version", "13")
        .header("sec-websocket-protocol", "other, ha")
        .header("sec-websocket-extensions", "permessage-deflate")
        .header(header::ORIGIN, "https://receiver.dhttp.net")
        .header(header::COOKIE, "session=test")
        .header(header::AUTHORIZATION, "Bearer test")
        .header(header::CONNECTION, "x-drop")
        .header("x-drop", "hidden")
        .body(body)
        .unwrap()
}

pub(crate) async fn ha_exchange<I>(ws: &mut WebSocketStream<I>)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let first = timeout(Duration::from_secs(3), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(first.to_text().unwrap()).unwrap()["type"],
        "auth_required"
    );
    ws.send(Message::Text(
        r#"{"type":"auth","access_token":"test-token"}"#.into(),
    ))
    .await
    .unwrap();
    let auth = timeout(Duration::from_secs(3), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(auth.to_text().unwrap()).unwrap()["type"],
        "auth_ok"
    );
    ws.send(Message::Text(
        r#"{"id":1,"type":"subscribe_events"}"#.into(),
    ))
    .await
    .unwrap();
    for index in 0..9 {
        let message = timeout(Duration::from_secs(3), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
        if index == 0 {
            assert_eq!(value["type"], "result");
        } else {
            assert_eq!(value["event"]["sequence"], index - 1);
        }
    }
    for id in 2..12 {
        ws.send(Message::Text(format!(r#"{{"id":{id},"type":"ping"}}"#)))
            .await
            .unwrap();
        let reply = timeout(Duration::from_secs(3), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            reply.to_text().unwrap(),
            format!(r#"{{"id":{id},"type":"pong"}}"#)
        );
    }
    ws.send(Message::Ping(vec![1, 2, 3])).await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(3), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Message::Pong(vec![1, 2, 3])
    );
    ws.send(Message::Binary(vec![42; 32768])).await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(3), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Message::Binary(vec![42; 32768])
    );
    ws.close(None).await.unwrap();
    assert!(matches!(
        timeout(Duration::from_secs(3), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Message::Close(_)
    ));
}

#[tokio::test]
async fn extended_connect_proxies_ha_auth_events_heartbeat_close_and_reconnect() {
    let (address, server) = ha_upstream().await;
    let _server = scopeguard::guard(server, |task| task.abort());
    let route = ProxyLocation {
        location: "/ws".into(),
        proxy_pass: format!("http://{address}/api/websocket")
            .parse::<http::Uri>()
            .unwrap()
            .into_parts(),
    };
    for _ in 0..2 {
        let (writer, upload) = tokio::io::duplex(64 * 1024);
        let upload = ReaderStream::new(upload)
            .map_ok(Frame::data)
            .map_err(|e| Box::new(e) as dhttp::BoxError);
        let response = timeout(
            Duration::from_secs(3),
            proxy(
                route.clone(),
                ha_request(
                    "https://receiver.dhttp.net/ws?case=ha",
                    StreamBody::new(upload).boxed_unsync(),
                ),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["sec-websocket-protocol"], "ha");
        assert_eq!(
            response.headers()["sec-websocket-extensions"],
            "permessage-deflate"
        );
        assert_eq!(response.headers()[header::SET_COOKIE], "upstream=ok");
        for name in [
            "connection",
            "upgrade",
            "sec-websocket-accept",
            "content-length",
        ] {
            assert!(!response.headers().contains_key(name));
        }
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::time::resume();
        let reader = StreamReader::new(
            response
                .into_body()
                .into_data_stream()
                .map_err(io::Error::other),
        );
        let mut ws =
            WebSocketStream::from_raw_socket(tokio::io::join(reader, writer), Role::Client, None)
                .await;
        ha_exchange(&mut ws).await;
    }
}

#[tokio::test]
async fn handshake_rejection_stays_http_and_bad_accept_is_a_gateway_error() {
    for (status, headers) in [
        ("401 Unauthorized", ""),
        ("200 OK", ""),
        (
            "101 Switching Protocols",
            "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: wrong\r\n",
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut wire = Vec::new();
            while !wire.ends_with(b"\r\n\r\n") {
                wire.push(socket.read_u8().await.unwrap());
            }
            socket
                .write_all(
                    format!("HTTP/1.1 {status}\r\n{headers}Content-Length: 6\r\n\r\ndenied")
                        .as_bytes(),
                )
                .await
                .unwrap();
        });
        let _server = scopeguard::guard(server, |task| task.abort());
        let route = ProxyLocation {
            location: "/".into(),
            proxy_pass: format!("http://{address}")
                .parse::<http::Uri>()
                .unwrap()
                .into_parts(),
        };
        let result = proxy(
            route,
            ha_request(
                "https://receiver.dhttp.net/ws",
                Full::new(Bytes::new())
                    .map_err(|never| match never {})
                    .boxed_unsync(),
            ),
        )
        .await;
        if status.starts_with("401") {
            let response = result.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "denied"
            );
        } else {
            assert!(matches!(result, Err(Error::Io(_))));
        }
    }
}

#[tokio::test]
async fn websocket_rejects_unsupported_version_without_connecting() {
    for version in [None, Some("12")] {
        let route = ProxyLocation {
            location: "/".into(),
            proxy_pass: "http://127.0.0.1:1"
                .parse::<http::Uri>()
                .unwrap()
                .into_parts(),
        };
        let mut request = ha_request("https://receiver.dhttp.net/ws", Body::default());
        request.headers_mut().remove("sec-websocket-version");
        if let Some(version) = version {
            request
                .headers_mut()
                .insert("sec-websocket-version", version.parse().unwrap());
        }
        assert!(matches!(
            proxy(route, request).await,
            Err(Error::BadRequest(_))
        ));
    }
}

async fn raw_upgrade(socket: &mut tokio::net::TcpStream) {
    let mut wire = Vec::new();
    while !wire.ends_with(b"\r\n\r\n") {
        wire.push(socket.read_u8().await.unwrap());
    }
    let wire = String::from_utf8(wire).unwrap();
    let key = wire
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-key")
                .then(|| value.trim())
        })
        .unwrap();
    socket.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {}\r\n\r\n", derive_accept_key(key.as_bytes())).as_bytes()).await.unwrap();
}

#[tokio::test]
async fn websocket_upload_eof_preserves_response_and_opaque_frame_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        raw_upgrade(&mut socket).await;
        let mut input = Vec::new();
        socket.read_to_end(&mut input).await.unwrap();
        // RSV1 and fragmented/compressed payloads are opaque to the proxy.
        assert_eq!(input, b"\xc1\x03\x01\x02\x03");
        socket.write_all(b"after-upload-eof").await.unwrap();
    });
    let _server = scopeguard::guard(server, |task| task.abort());
    let route = ProxyLocation {
        location: "/".into(),
        proxy_pass: format!("http://{address}")
            .parse::<http::Uri>()
            .unwrap()
            .into_parts(),
    };
    let request = ha_request(
        "https://receiver.dhttp.net/ws",
        Full::new(Bytes::from_static(b"\xc1\x03\x01\x02\x03"))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    let response = proxy(route, request).await.unwrap();
    let bytes = timeout(Duration::from_secs(3), response.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    assert_eq!(bytes, "after-upload-eof");
}

#[tokio::test]
async fn websocket_body_drop_and_upload_error_release_upstream() {
    for fail_upload in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            raw_upgrade(&mut socket).await;
            let mut input = Vec::new();
            socket.read_to_end(&mut input).await.unwrap();
            assert!(input.is_empty());
        });
        let route = ProxyLocation {
            location: "/".into(),
            proxy_pass: format!("http://{address}")
                .parse::<http::Uri>()
                .unwrap()
                .into_parts(),
        };
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        if fail_upload {
            sender
                .send(Err::<Frame<Bytes>, dhttp::BoxError>(Box::new(
                    io::Error::other("failed upload"),
                )))
                .await
                .unwrap();
        }
        let upload = futures::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|frame| (frame, receiver))
        });
        let response = proxy(
            route,
            ha_request(
                "https://receiver.dhttp.net/ws",
                StreamBody::new(upload).boxed_unsync(),
            ),
        )
        .await
        .unwrap();
        if fail_upload {
            let error = timeout(Duration::from_secs(3), response.into_body().collect())
                .await
                .unwrap()
                .unwrap_err();
            assert!(error.to_string().contains("failed upload"));
        } else {
            drop(response);
        }
        timeout(Duration::from_secs(3), sender.closed())
            .await
            .unwrap();
        timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    }
}

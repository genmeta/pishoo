//! Native dhttp client for QUIC.
use std::{error::Error, io, time::Duration};

use bytes::Bytes;
use http_body_util::BodyExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

type Failure = Box<dyn Error + Send + Sync>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    if let Err(error) = run().await {
        eprintln!("pishoo client: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Failure> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "echo".to_owned());
    if command == "run" {
        return pishoo::run().await.map_err(Into::into);
    }
    let identity =
        std::env::var("PISHOO_CLIENT_IDENTITY").unwrap_or_else(|_| "demo.dhttp.net".to_owned());
    // A standalone client must explicitly install its name sources.
    dhttp::resolve::Resolver::add(std::sync::Arc::new(dhttp::resolve::SystemResolver));
    let h3_dns = std::sync::Arc::new(ddns::H3Resolver::anonymous(
        ddns::resolvers::DHTTP_NAME_SERVICE.parse()?,
    )?);
    dhttp::resolve::Resolver::add(h3_dns.clone());
    let mdns = ddns::mdns::MdnsResolverSet::new(ddns::resolvers::DHTTP_MDNS_SERVICE_DOMAIN);
    let h3_only = matches!(
        command.as_str(),
        "probe" | "nat-get" | "serve" | "query" | "publish"
    );
    if !h3_only {
        dhttp::resolve::Resolver::add(std::sync::Arc::new(mdns.clone()));
    }
    dhttp::DhttpNetwork::init().await?;
    for (bound, device) in dhttp::AddressBook::global()
        .inner_bindings()
        .into_iter()
        .filter(|_| !h3_only)
    {
        if qprotocol::Dock::global().find_socket(bound).is_some() {
            if let Err(error) = mdns
                .upsert(ddns::mdns::MdnsBinding::new(device.name(), bound.ip()))
                .await
            {
                eprintln!("client mDNS {} at {}: {error}", device.name(), bound.ip());
            }
        }
    }
    let endpoint = dhttp::Endpoint::load(&identity).await?;
    match command.as_str() {
        "probe" | "nat-get" | "serve" => {
            let target = if command == "nat-get" {
                Some(
                    args.next()
                        .ok_or("usage: pishoo-client nat-get URL")?
                        .parse::<http::Uri>()?,
                )
            } else {
                None
            };
            let addresses = dhttp::AddressBook::global();
            let mut published = addresses.subscribe_ddns();
            // Ordinary Network startup owns classification and heartbeat maintenance.
            // Wait for its public mapping rather than probing the same socket twice.
            tokio::time::timeout(Duration::from_secs(60), async {
                while published.borrow_and_update().is_empty() {
                    published.changed().await.map_err(io::Error::other)?;
                }
                Ok::<_, io::Error>(())
            }).await??;
            eprintln!("Network DDNS after NAT: {:?}", published.borrow());
            let dock = qprotocol::Dock::global();
            let mut replay = addresses.subscribe_punch(dhttp::Scopes::ALL);
            while let Ok(event) = replay.try_recv() {
                if let qprotocol::AddressEvent::Added { bound, endpoint, nat } = event {
                    if endpoint.scope() == Some(dhttp::Scope::External) {
                        eprintln!("{bound}: NAT {nat:?}, mapped {endpoint}");
                    } else if command != "probe" {
                        // Acceptance deliberately excludes LAN/loopback shortcuts.
                        addresses.remove(endpoint);
                    }
                }
            }
            // Every advertised public endpoint must already have a live QUIC alias.
            for endpoint in published.borrow().iter() {
                if dock.topology().quic().find_socket(*endpoint).is_none() {
                    return Err(io::Error::other("public NAT mapping has no socket").into());
                }
            }
            if command == "serve" {
                eprintln!("NAT server ready");
                pishoo::run().await?;
            }
            if let Some(uri) = target {
                let response = endpoint.get(uri.clone()).await?;
                eprintln!("{:?} {}", response.version(), response.status());
                if response.version() != http::Version::HTTP_3
                    || response.status() != http::StatusCode::OK
                {
                    return Err(io::Error::other("NAT HTTP/3 request failed").into());
                }
                let body = response.into_body().collect().await?.to_bytes();
                tokio::io::stdout().write_all(&body).await?;
                tokio::io::stdout().flush().await?;
                // Keep the authenticated connection alive while punch frames and
                // path validation complete, then prove another HTTP exchange.
                tokio::time::sleep(Duration::from_secs(8)).await;
                let response = endpoint.get(uri).await?;
                eprintln!(
                    "after punching: {:?} {}",
                    response.version(),
                    response.status()
                );
                if response.version() != http::Version::HTTP_3
                    || response.status() != http::StatusCode::OK
                    || response.into_body().collect().await?.to_bytes() != body
                {
                    return Err(io::Error::other("HTTP/3 response changed after punching").into());
                }
            }
        }
        "query" => {
            use futures::StreamExt as _;
            let resolver =
                ddns::H3Resolver::new(ddns::resolvers::DHTTP_NAME_SERVICE.parse()?, &endpoint)?;
            let target = args.next().unwrap_or(identity);
            let mut records = resolver.lookup(&target, "443", None).await?;
            while let Some((source, address)) = records.next().await {
                println!("{source}: {address}");
            }
        }
        "publish" => {
            let addresses = args
                .map(|address| address.parse::<dhttp::resolve::EndpointAddr>())
                .collect::<Result<Vec<_>, _>>()?;
            let resolver =
                ddns::H3Resolver::new(ddns::resolvers::DHTTP_NAME_SERVICE.parse()?, &endpoint)?;
            let lease = resolver.publish_endpoints(&identity, addresses).await?;
            println!("published {identity}: lease {lease:?}");
        }
        "smoke" => {
            for (path, expected) in [
                ("/std/file/hello.txt", "static from demo\n"),
                ("/proxy/hello.txt", "static from upstream\n"),
                ("/exact", "static from upstream\n"),
                ("/std/api/info/info", "info Lib handled /info\n"),
            ] {
                let uri: http::Uri = format!("https://demo.dhttp.net{path}").parse()?;
                let response = endpoint.get(uri).await?;
                if response.version() != http::Version::HTTP_3
                    || response.status() != http::StatusCode::OK
                    || response.into_body().collect().await?.to_bytes() != expected
                {
                    return Err(io::Error::other(format!("GET {path} failed")).into());
                }
                println!("HTTP/3 GET {path}: 200");
            }
            let uri: http::Uri = "https://demo.dhttp.net/unlisted".parse()?;
            if endpoint.get(uri).await?.status() != http::StatusCode::FORBIDDEN {
                return Err(io::Error::other("unlisted demo API was not denied").into());
            }
            println!("HTTP/3 GET /unlisted: 403");
            let uri: http::Uri = "https://demo.dhttp.net/unlisted".parse()?;
            if post_bytes(&endpoint, uri, Bytes::from_static(b"denied upload"))
                .await?
                .status()
                != http::StatusCode::FORBIDDEN
            {
                return Err(io::Error::other("unlisted upload was not denied").into());
            }
            println!("HTTP/3 POST /unlisted: 403");
            let concurrent = (0..8).map(|index| {
                let endpoint = endpoint.clone();
                async move {
                    let (path, expected) = if index % 2 == 0 {
                        ("/std/file/hello.txt", "static from demo\n")
                    } else {
                        ("/proxy/hello.txt", "static from upstream\n")
                    };
                    let uri: http::Uri = format!("https://demo.dhttp.net{path}").parse()?;
                    let response = endpoint.get(uri).await?;
                    let version = response.version();
                    let status = response.status();
                    let body = response.into_body().collect().await?.to_bytes();
                    Ok::<_, Failure>((version, status, body, expected))
                }
            });
            for result in futures::future::join_all(concurrent).await {
                let (version, status, body, expected) = result?;
                if version != http::Version::HTTP_3
                    || status != http::StatusCode::OK
                    || body != expected
                {
                    return Err(io::Error::other("concurrent H3 streams failed").into());
                }
            }
            println!("HTTP/3 eight concurrent static/proxy streams: 200");
            let uri: http::Uri = "https://demo.dhttp.net/std/api/echo/echo".parse()?;
            let response =
                post_bytes(&endpoint, uri.clone(), Bytes::from_static(b"hello h3x")).await?;
            if response.version() != http::Version::HTTP_3
                || response.status() != http::StatusCode::OK
                || response.into_body().collect().await?.to_bytes() != "hello h3x"
            {
                return Err(io::Error::other("POST Echo failed").into());
            }
            println!("HTTP/3 POST /std/api/echo/echo: 200");

            let large = Bytes::from(vec![b'x'; 256 * 1024]);
            let uri: http::Uri = "https://demo.dhttp.net/std/api/echo/echo".parse()?;
            let response = post_bytes(&endpoint, uri.clone(), large.clone()).await?;
            let echoed =
                tokio::time::timeout(Duration::from_secs(10), response.into_body().collect())
                    .await??
                    .to_bytes();
            if echoed != large {
                return Err(io::Error::other("large duplex Echo mismatch").into());
            }
            println!("HTTP/3 256 KiB Echo with stream backpressure: 200");

            let trailers_uri: http::Uri = "https://demo.dhttp.net/std/api/trailers/read".parse()?;
            let response = post_bytes(
                &endpoint,
                trailers_uri,
                Bytes::from_static(b"hello-hello-hello-hello-hello-hello-hello-hello"),
            )
            .await?;
            if response.version() != http::Version::HTTP_3
                || response.status() != http::StatusCode::CREATED
            {
                return Err(io::Error::other("WASM trailer response status failed").into());
            }
            let collected = response.into_body().collect().await?;
            let trailers = collected
                .trailers()
                .ok_or_else(|| io::Error::other("WASM response trailers missing"))?;
            if trailers.get_all("x-response-trailer").iter().count() != 2
                || collected.to_bytes() != b"world-world-".repeat(64)
            {
                return Err(io::Error::other("WASM response body/trailers mismatch").into());
            }
            println!("HTTP/3 WASM upload and duplicate response trailers: 201");

            let (mut upload, response) = endpoint.post(uri).await?;
            let mut response = response.await?;
            if response.version() != http::Version::HTTP_3
                || response.status() != http::StatusCode::OK
            {
                return Err(io::Error::other("streaming Echo status failed").into());
            }
            for chunk in [b"first\n".as_slice(), b"second\n".as_slice()] {
                upload.write_all(chunk).await?;
                let mut echoed = Vec::new();
                while echoed.len() < chunk.len() {
                    let frame =
                        tokio::time::timeout(Duration::from_secs(5), response.body_mut().frame())
                            .await?
                            .ok_or_else(|| io::Error::other("Echo response ended early"))??;
                    echoed.extend_from_slice(
                        &frame
                            .into_data()
                            .map_err(|_| io::Error::other("Echo returned non-data frame"))?,
                    );
                }
                if echoed != chunk {
                    return Err(io::Error::other("Echo chunk mismatch").into());
                }
            }
            upload.shutdown().await?;
            response.into_body().collect().await?;
            println!("HTTP/3 duplex Echo: each chunk arrived before upload EOF");

            let duplex_uri: http::Uri = "https://demo.dhttp.net/duplex".parse()?;
            let (mut upload, response) = endpoint.post(duplex_uri).await?;
            let mut response = tokio::time::timeout(Duration::from_secs(5), response).await??;
            if response.version() != http::Version::HTTP_3
                || response.status() != http::StatusCode::OK
            {
                return Err(io::Error::other("local proxy duplex status failed").into());
            }
            for chunk in [b"proxy-first\n".as_slice(), b"proxy-second\n".as_slice()] {
                upload.write_all(chunk).await?;
                let mut echoed = Vec::new();
                while echoed.len() < chunk.len() {
                    let frame =
                        tokio::time::timeout(Duration::from_secs(5), response.body_mut().frame())
                            .await?
                            .ok_or_else(|| {
                                io::Error::other("local proxy response ended early")
                            })??;
                    echoed.extend_from_slice(
                        &frame.into_data().map_err(|_| {
                            io::Error::other("local proxy returned a non-data frame")
                        })?,
                    );
                }
                if echoed != chunk {
                    return Err(io::Error::other("local proxy duplex chunk mismatch").into());
                }
            }
            upload.shutdown().await?;
            tokio::time::timeout(Duration::from_secs(5), response.into_body().collect()).await??;
            println!("HTTP/3 local proxy duplex: each chunk arrived before upload EOF");

            let uri: http::Uri = "https://demo.dhttp.net/std/api/echo/echo".parse()?;
            let (mut upload, response) = endpoint.post(uri).await?;
            let mut response = response.await?;
            upload.write_all(b"abandoned\n").await?;
            let _ = tokio::time::timeout(Duration::from_secs(5), response.body_mut().frame())
                .await?
                .ok_or_else(|| io::Error::other("cancel test response ended early"))??;
            drop(response);
            drop(upload);
            let uri: http::Uri = "https://demo.dhttp.net/std/file/hello.txt".parse()?;
            let follow_up =
                tokio::time::timeout(Duration::from_secs(5), endpoint.get(uri)).await??;
            if follow_up.status() != http::StatusCode::OK
                || follow_up.into_body().collect().await?.to_bytes() != "static from demo\n"
            {
                return Err(io::Error::other("request after dropped Echo failed").into());
            }
            println!("HTTP/3 dropped Echo response: next request still succeeds");
        }
        "ha-websocket" => {
            use futures::{SinkExt, StreamExt, TryStreamExt};
            use tokio_tungstenite::{WebSocketStream, tungstenite::{Message, protocol::Role}};
            use tokio_util::io::StreamReader;

            let origin: http::Uri = args
                .next()
                .ok_or("usage: pishoo-client ha-websocket ORIGIN [ROUNDS]")?
                .parse()?;
            if origin.scheme_str() != Some("https") {
                return Err("HA WebSocket requires an https origin".into());
            }
            let authority = origin.authority().ok_or("HA WebSocket requires an authority")?;
            let rounds: usize = args.next().map_or(Ok(3), |value| value.parse())?;
            if rounds == 0 {
                return Err("HA WebSocket requires at least one round".into());
            }
            for round in 0..rounds {
                tokio::time::timeout(Duration::from_secs(10), async {
                    let request = http::Request::builder()
                        .method(http::Method::CONNECT)
                        .uri(format!("https://{authority}/api/websocket"))
                        .extension(std::sync::Arc::<str>::from("websocket"))
                        .header("sec-websocket-version", "13")
                        .body(dhttp::WndBuf::new(64 * 1024))?;
                    let (writer, response) = endpoint.from_request(request).await?;
                    let response = response.await?;
                    if response.status() != http::StatusCode::OK
                        || response.version() != http::Version::HTTP_3
                    {
                        return Err(io::Error::other(format!(
                            "HA WebSocket: {:?} {}", response.version(), response.status()
                        )).into());
                    }
                    let reader = StreamReader::new(
                        response.into_body().into_data_stream().map_err(io::Error::other),
                    );
                    let mut ws = WebSocketStream::from_raw_socket(
                        tokio::io::join(reader, writer), Role::Client, None,
                    ).await;
                    let first = ws.next().await.ok_or("HA WebSocket ended before greeting")??;
                    let Message::Text(text) = first else {
                        return Err("HA WebSocket did not send a text greeting".into());
                    };
                    let greeting: serde_json::Value = serde_json::from_str(&text)?;
                    if greeting["type"] != "auth_required" {
                        return Err("HA WebSocket did not request authentication".into());
                    }
                    let ping = format!("pishoo-{round}").into_bytes();
                    ws.send(Message::Ping(ping.clone())).await?;
                    if ws.next().await.ok_or("HA WebSocket ended before pong")?? != Message::Pong(ping) {
                        return Err("HA WebSocket pong mismatch".into());
                    }
                    ws.close(None).await?;
                    if !matches!(ws.next().await, Some(Ok(Message::Close(_)))) {
                        return Err("HA WebSocket did not acknowledge close".into());
                    }
                    println!("HTTP/3 HA WebSocket round {}: 200, auth_required, ping/pong, close", round + 1);
                    Ok::<_, Failure>(())
                }).await??;
            }
        }
        "ha-burst" => {
            let origin: http::Uri = args
                .next()
                .ok_or("usage: pishoo-client ha-burst ORIGIN PATHS.json [ROUNDS]")?
                .parse()?;
            if origin.scheme_str() != Some("https") {
                return Err("HA burst requires an https origin".into());
            }
            let authority = origin.authority().ok_or("HA burst requires an authority")?;
            let paths: Vec<String> = serde_json::from_slice(&std::fs::read(
                args.next().ok_or("HA burst requires PATHS.json")?,
            )?)?;
            if paths.is_empty()
                || paths.iter().any(|path| !path.starts_with('/') || path.starts_with("//"))
            {
                return Err("HA burst requires nonempty origin-relative paths".into());
            }
            let rounds: usize = args.next().map_or(Ok(3), |value| value.parse())?;
            let warmup = endpoint
                .get(format!("https://{authority}/manifest.json").parse()?)
                .await?;
            if warmup.status() != http::StatusCode::OK || warmup.version() != http::Version::HTTP_3 {
                return Err("HA burst warmup did not return HTTP/3 200".into());
            }
            warmup.into_body().collect().await?;
            for round in 0..rounds {
                let requests = paths.iter().enumerate().map(|(index, path)| {
                    let endpoint = &endpoint;
                    async move {
                        let response = endpoint
                            .get(format!("https://{authority}{path}").parse()?)
                            .header(
                                http::HeaderName::from_static("x-qpack-regression"),
                                format!("{round}-{index}").parse()?,
                            )
                            .await?;
                        if response.status() != http::StatusCode::OK
                            || response.version() != http::Version::HTTP_3
                        {
                            return Err(io::Error::other(format!(
                                "HA burst {path}: {:?} {}", response.version(), response.status()
                            )).into());
                        }
                        Ok::<_, Failure>(response.into_body().collect().await?.to_bytes().len())
                    }
                });
                let sizes = tokio::time::timeout(
                    Duration::from_secs(60), futures::future::try_join_all(requests),
                ).await??;
                println!(
                    "HTTP/3 HA burst round {}: {} responses all 200; {} bytes",
                    round + 1, sizes.len(), sizes.iter().sum::<usize>(),
                );
            }
        }
        "get" | "post" | "put" | "patch" => {
            let target = args
                .next()
                .ok_or("usage: pishoo-client get|post|put|patch URL [BODY]")?;
            let uri: http::Uri = if target.starts_with('/') {
                format!("https://{identity}{target}").parse()?
            } else {
                target.parse()?
            };
            let response = if command == "get" {
                endpoint.get(uri).await?
            } else {
                let body = args.next().unwrap_or_default();
                let method: http::Method = command.to_ascii_uppercase().parse()?;
                send_bytes(&endpoint, method, uri, Bytes::from(body)).await?
            };
            eprintln!("{:?} {}", response.version(), response.status());
            let body = response.into_body().collect().await?.to_bytes();
            let mut stdout = tokio::io::stdout();
            stdout.write_all(&body).await?;
            stdout.flush().await?;
        }
        "echo" => {
            let target = args
                .next()
                .unwrap_or_else(|| format!("https://{identity}/std/api/echo/echo"));
            let uri: http::Uri = if target.starts_with('/') {
                format!("https://{identity}{target}").parse()?
            } else {
                target.parse()?
            };
            let (mut upload, response) = endpoint.post(uri).await?;
            let mut input = scopeguard::guard(
                tokio::spawn(async move {
                    let mut lines = BufReader::new(tokio::io::stdin()).lines();
                    while let Some(line) = lines.next_line().await? {
                        upload.write_all(format!("{line}\n").as_bytes()).await?;
                    }
                    upload.shutdown().await
                }),
                |task| task.abort(),
            );
            let mut response = response.await?;
            if response.status() != http::StatusCode::OK {
                return Err(io::Error::other(format!("Echo: {}", response.status())).into());
            }
            eprintln!("Type a line and press Enter. Ctrl-D ends the stream; Ctrl-C exits.");
            let mut stdout = tokio::io::stdout();
            let mut line_start = true;
            while let Some(frame) = response.body_mut().frame().await {
                if let Ok(data) = frame?.into_data() {
                    for chunk in data.split_inclusive(|byte| *byte == b'\n') {
                        if line_start {
                            stdout.write_all(b"echo> ").await?;
                        }
                        stdout.write_all(chunk).await?;
                        line_start = chunk.ends_with(b"\n");
                    }
                    stdout.flush().await?;
                }
            }
            if input.is_finished() {
                (&mut *input).await??;
            }
        }
        _ => return Err("usage: pishoo-client [echo [URL]|get URL|post URL BODY|put URL JSON|patch URL JSON|ha-websocket ORIGIN [ROUNDS]|ha-burst ORIGIN PATHS.json [ROUNDS]|smoke|probe|nat-get URL|serve|query [NAME]|publish [ADDRESS ...]]".into()),
    }
    Ok(())
}

// Finish finite byte uploads independently so an early streaming response can
// be consumed while h3x is still sending the request.
async fn post_bytes(
    endpoint: &dhttp::Endpoint,
    uri: http::Uri,
    bytes: Bytes,
) -> Result<http::Response<dhttp::Body>, Failure> {
    send_bytes(endpoint, http::Method::POST, uri, bytes).await
}

async fn send_bytes(
    endpoint: &dhttp::Endpoint,
    method: http::Method,
    uri: http::Uri,
    bytes: Bytes,
) -> Result<http::Response<dhttp::Body>, Failure> {
    let json = matches!(method, http::Method::PUT | http::Method::PATCH)
        || serde_json::from_slice::<serde_json::Value>(&bytes).is_ok();
    let mut request = endpoint.request(method, uri);
    if json {
        request = request.header(http::header::CONTENT_TYPE, "application/json".parse()?);
    }
    let (mut writer, response) = request.body(dhttp::WndBuf::new(64 * 1024)).await?;
    let upload = scopeguard::guard(
        tokio::spawn(async move {
            writer.write_all(&bytes).await?;
            writer.shutdown().await
        }),
        |task| task.abort(),
    );
    let response = response.await?;
    drop(scopeguard::ScopeGuard::into_inner(upload));
    Ok(response)
}

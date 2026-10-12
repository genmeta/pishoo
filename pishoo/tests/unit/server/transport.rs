//! Opt-in transport acceptance through production Pishoo routers and real QUIC/TLS/H3.
//! Run alone: TLS roots, Resolver and DHTTP_HOME are process-global.
use std::{convert::Infallible, io, path::Path, time::Duration};

use bytes::Bytes;
use futures::future::try_join_all;
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncWriteExt,
    time::{Instant, timeout},
};

use super::*;

const MIB: usize = 1024 * 1024;
const CHUNK: usize = 64 * 1024;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit two-machine NAT test; requires isolated home and SSH-exchanged public endpoints"]
async fn transport_public_peer() {
    use hyper::{body::Incoming, service::service_fn};
    use hyper_util::rt::TokioIo;
    let home = std::path::PathBuf::from(std::env::var_os("PISHOO_PUBLIC_HOME").unwrap());
    let role = std::env::var("PISHOO_PUBLIC_ROLE").unwrap();
    if role == "prepare" {
        assert!(!home.exists(), "use a fresh isolated test home");
        credentials::generate(
            &home,
            &[
                ("receiver", "receiver.dhttp.net"),
                ("alice", "alice.dhttp.net"),
            ],
        );
        return;
    }
    assert!(matches!(role.as_str(), "receiver" | "alice"));
    unsafe {
        std::env::set_var("DHTTP_HOME", &home);
    }
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter("warn,dhttp=trace,qconnection=debug,qtls=debug")
        .try_init();
    let profile = profile(&home, &role);
    std::fs::create_dir_all(profile.join("file")).unwrap();
    std::fs::create_dir_all(profile.join("lib/echo")).unwrap();
    std::fs::write(
        profile.join("lib/echo/lib.wasm"),
        include_bytes!("../../../assets/tcp-demo-echo.wasm"),
    )
    .unwrap();
    let lengths = if role == "receiver" {
        vec![MIB, 100 * MIB, 1024 * MIB]
    } else {
        vec![MIB, 100 * MIB]
    };
    let mut files = Vec::new();
    for length in lengths {
        let name = format!("bytes-{length}.bin");
        let digest = fixture(&profile.join(&format!("file/{name}")), length);
        files.push(serde_json::json!({"name":name,"bytes":length,"sha256":digest}));
    }
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = upstream.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        loop {
            let (socket, _) = upstream.accept().await.unwrap();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(socket),
                        service_fn(|request: Request<Incoming>| async move {
                            Ok::<_, Infallible>(http::Response::new(AxumBody::new(
                                request.into_body(),
                            )))
                        }),
                    )
                    .await;
            });
        }
    });
    let _upstream = scopeguard::guard(upstream, |task| task.abort());
    rusqlite::Connection::open(profile.config_db_path())
        .unwrap()
        .execute(
            "INSERT INTO proxy_locations VALUES('/transport/',?1)",
            [format!("http://{address}/")],
        )
        .unwrap();
    dhttp::DhttpNetwork::init().await.unwrap();
    qtls::RootCerts::set([dhttp::CertificateDer::from(
        std::fs::read(home.join("ca.der")).unwrap(),
    )])
    .unwrap();
    let mut server = Server::load(profile, Arc::new(WasmRuntime::new().unwrap()))
        .await
        .unwrap();
    for path in ["/std/file", "/std/api/echo", "/transport"] {
        server
            .access
            .set_policy(
                access_control::Method::Unspecified,
                path,
                access_control::Effect::Allow,
                access_control::Grantee::Named,
            )
            .await
            .unwrap();
    }
    let app = server.router.clone();
    let listener = server
        .endpoint
        .listen(
            dhttp::Scopes::ALL,
            tower::service_fn(move |request: Request<Body>| {
                let app = app.read().unwrap().clone();
                async move { app.oneshot(request.map(AxumBody::new)).await }
            }),
        )
        .await
        .unwrap();
    let _listener = scopeguard::guard(tokio::spawn(listener), |task| task.abort());
    let addresses = dhttp::AddressBook::global();
    let mut published = addresses.subscribe_ddns();
    timeout(Duration::from_secs(90), async {
        while published.borrow_and_update().is_empty() {
            published.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let endpoints = published
        .borrow()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let ready = serde_json::json!({"role":role,"pid":std::process::id(),"name":server.name(),"endpoints":endpoints,"files":files});
    std::fs::write(
        home.join(format!("{role}-ready.json")),
        serde_json::to_vec_pretty(&ready).unwrap(),
    )
    .unwrap();
    println!("PUBLIC_READY {ready}");
    let mut results = Vec::new();
    if role == "receiver" {
        let until = Instant::now() + Duration::from_secs(3600);
        while !home.join("stop").exists() && Instant::now() < until {
            if home.join("reverse-trigger").exists() {
                std::fs::remove_file(home.join("reverse-trigger")).unwrap();
                let peer: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(home.join("alice-ready.json")).unwrap())
                        .unwrap();
                for address in peer["endpoints"].as_array().unwrap() {
                    dhttp::resolve::Resolver::add(Arc::new(PeerResolver(
                        address.as_str().unwrap().parse().unwrap(),
                    )));
                }
                let file = peer["files"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|f| f["bytes"] == 100 * MIB)
                    .unwrap();
                let started = Instant::now();
                let result = bounded(download(
                    &server.endpoint,
                    &format!(
                        "https://alice.dhttp.net/std/file/{}",
                        file["name"].as_str().unwrap()
                    ),
                    100 * MIB,
                    file["sha256"].as_str().unwrap(),
                ))
                .await;
                record(
                    &mut results,
                    "public_reverse_download_100_mib",
                    result,
                    started,
                );
                std::fs::write(home.join("reverse-finished"), []).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    } else {
        let peer: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.join("receiver-ready.json")).unwrap())
                .unwrap();
        for address in peer["endpoints"].as_array().unwrap() {
            dhttp::resolve::Resolver::add(Arc::new(PeerResolver(
                address.as_str().unwrap().parse().unwrap(),
            )));
        }
        for file in peer["files"].as_array().unwrap() {
            let length = file["bytes"].as_u64().unwrap() as usize;
            let started = Instant::now();
            let result = bounded(download(
                &server.endpoint,
                &format!(
                    "https://receiver.dhttp.net/std/file/{}",
                    file["name"].as_str().unwrap()
                ),
                length,
                file["sha256"].as_str().unwrap(),
            ))
            .await;
            record(
                &mut results,
                &format!("public_download_{length}_bytes"),
                result,
                started,
            );
            if length == MIB && results.last().unwrap()["status"] == "passed" {
                std::fs::write(home.join("connected"), []).unwrap();
            }
        }
        for (kind, uri) in [
            ("wasm", "https://receiver.dhttp.net/std/api/echo/echo"),
            ("proxy", "https://receiver.dhttp.net/transport/echo"),
        ] {
            let started = Instant::now();
            record(
                &mut results,
                &format!("public_{kind}_duplex"),
                bounded(duplex(&server.endpoint, uri, Duration::ZERO)).await,
                started,
            );
            for concurrent in [1, 8, 32] {
                let started = Instant::now();
                let batch = async {
                    let values = try_join_all((0..concurrent).map(|index| {
                        echo(
                            &server.endpoint,
                            uri,
                            MIB,
                            index + 900,
                            Duration::ZERO,
                            Duration::ZERO,
                        )
                    }))
                    .await?;
                    Ok(
                        serde_json::json!({"requests":values.len(),"bytes":values.len()*MIB,"elapsed_s":started.elapsed().as_secs_f64()}),
                    )
                };
                record(
                    &mut results,
                    &format!("public_{kind}_concurrent_{concurrent}"),
                    bounded(batch).await,
                    started,
                );
            }
        }
        let started = Instant::now();
        record(
            &mut results,
            "public_proxy_upload_echo_100_mib",
            bounded(echo(
                &server.endpoint,
                "https://receiver.dhttp.net/transport/echo",
                100 * MIB,
                950,
                Duration::ZERO,
                Duration::ZERO,
            ))
            .await,
            started,
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    server.close().await.unwrap();
    assert!(
        results.iter().all(|row| row["status"] == "passed"),
        "{results:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit read-only probe of the user-provided remote machine over native QUIC"]
async fn transport_existing_remote_connectivity() {
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter("warn,dhttp=trace,qconnection=debug,qtls=debug")
        .try_init();
    dhttp::DhttpNetwork::init().await.unwrap();
    dhttp::resolve::Resolver::add(Arc::new(PeerResolver(EndpointAddr::direct(
        "119.28.45.191:443".parse().unwrap(),
    ))));
    let endpoint = dhttp::Endpoint::load("alice.smith").await.unwrap();
    let response = timeout(Duration::from_secs(45), async {
        endpoint
            .get("https://usopp.li.dhttp.net/std/profile".parse().unwrap())
            .await
    })
    .await
    .unwrap()
    .unwrap();
    let version = response.version();
    let status = response.status();
    let peer = response
        .extensions()
        .get::<dhttp::RemoteAuthority>()
        .unwrap()
        .name()
        .to_owned();
    let body = timeout(Duration::from_secs(15), response.into_body().collect())
        .await
        .unwrap()
        .unwrap();
    println!(
        "REMOTE_CONNECTIVITY version={version:?} status={status} peer={peer} bytes={}",
        body.to_bytes().len()
    );
    assert_eq!(version, http::Version::HTTP_3);
    assert_eq!(peer, "usopp.li.dhttp.net");
}

// Each offset and seed affects the bytes; concurrent exchanges use distinct seeds.
fn payload(offset: usize, length: usize, seed: usize) -> Vec<u8> {
    (offset..offset + length)
        .map(|position| {
            let value = position.wrapping_add(seed.wrapping_mul(7919));
            (value.wrapping_mul(31) ^ (value >> 8) ^ (value >> 16)) as u8
        })
        .collect()
}

fn fixture(path: &Path, length: usize) -> String {
    use std::io::Write;
    let mut file = std::fs::File::create(path).unwrap();
    let mut digest = Sha256::new();
    for offset in (0..length).step_by(CHUNK) {
        let bytes = payload(offset, CHUNK.min(length - offset), 0);
        file.write_all(&bytes).unwrap();
        digest.update(&bytes);
    }
    format!("{:x}", digest.finalize())
}

fn check_response(response: &http::Response<Body>) -> std::result::Result<(), dhttp::BoxError> {
    if response.status() != StatusCode::OK || response.version() != http::Version::HTTP_3 {
        return Err(io::Error::other(format!(
            "expected HTTP/3 200, got {:?} {}",
            response.version(),
            response.status()
        ))
        .into());
    }
    if response
        .extensions()
        .get::<dhttp::RemoteAuthority>()
        .is_none()
    {
        return Err(io::Error::other("response has no verified remote authority").into());
    }
    Ok(())
}

async fn receive(
    mut response: http::Response<Body>,
    delay: Duration,
) -> std::result::Result<serde_json::Value, dhttp::BoxError> {
    check_response(&response)?;
    let started = Instant::now();
    let mut first = None;
    let mut length = 0usize;
    let mut digest = Sha256::new();
    while let Some(frame) = response.body_mut().frame().await {
        let frame = frame?;
        if let Ok(data) = frame.into_data() {
            if !data.is_empty() {
                first.get_or_insert(started.elapsed().as_secs_f64());
            }
            length += data.len();
            digest.update(&data);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
    }
    Ok(serde_json::json!({
        "bytes": length, "sha256": format!("{:x}", digest.finalize()),
        "body_first_byte_s": first, "body_elapsed_s": started.elapsed().as_secs_f64()
    }))
}

async fn download(
    endpoint: &dhttp::Endpoint,
    uri: &str,
    length: usize,
    digest: &str,
) -> std::result::Result<serde_json::Value, dhttp::BoxError> {
    let started = Instant::now();
    let response = endpoint.get(uri.parse()?).await?;
    let headers = started.elapsed().as_secs_f64();
    if response.headers()[http::header::CONTENT_LENGTH] != length.to_string() {
        return Err(io::Error::other("Content-Length mismatch").into());
    }
    let mut result = receive(response, Duration::ZERO).await?;
    if result["bytes"] != length || result["sha256"] != digest {
        return Err(io::Error::other(format!("download mismatch: {result}")).into());
    }
    result["elapsed_s"] = started.elapsed().as_secs_f64().into();
    result["headers_s"] = headers.into();
    result["mib_per_s"] = (length as f64 / MIB as f64 / started.elapsed().as_secs_f64()).into();
    Ok(result)
}

async fn echo(
    endpoint: &dhttp::Endpoint,
    uri: &str,
    length: usize,
    seed: usize,
    write_delay: Duration,
    read_delay: Duration,
) -> std::result::Result<serde_json::Value, dhttp::BoxError> {
    let started = Instant::now();
    let (mut writer, response) = endpoint
        .post(uri.parse()?)
        .header(
            http::header::CONTENT_TYPE,
            "application/octet-stream".parse()?,
        )
        .header(
            http::HeaderName::from_static("x-transport-case"),
            seed.to_string().parse()?,
        )
        .await?;
    let upload = async move {
        let mut digest = Sha256::new();
        for offset in (0..length).step_by(CHUNK) {
            let bytes = payload(offset, CHUNK.min(length - offset), seed);
            digest.update(&bytes);
            writer.write_all(&bytes).await?;
            if !write_delay.is_zero() {
                tokio::time::sleep(write_delay).await;
            }
        }
        writer.shutdown().await?;
        Ok::<_, dhttp::BoxError>(format!("{:x}", digest.finalize()))
    };
    let read = async move { receive(response.await?, read_delay).await };
    let (digest, mut result) = tokio::try_join!(upload, read)?;
    if result["bytes"] != length || result["sha256"] != digest {
        return Err(io::Error::other(format!("echo mismatch for seed {seed}: {result}")).into());
    }
    result["elapsed_s"] = started.elapsed().as_secs_f64().into();
    // Payload rate is one direction. Total echo traffic is twice the payload size.
    result["mib_per_s"] = (length as f64 / MIB as f64 / started.elapsed().as_secs_f64()).into();
    Ok(result)
}

async fn duplex(
    endpoint: &dhttp::Endpoint,
    uri: &str,
    idle: Duration,
) -> std::result::Result<serde_json::Value, dhttp::BoxError> {
    let (mut writer, response) = endpoint.post(uri.parse()?).await?;
    let mut response = response.await?;
    check_response(&response)?;
    let mut maximum = 0.0_f64;
    for index in 0..3 {
        if index == 1 {
            tokio::time::sleep(idle).await;
        }
        // Keep upload open until every response block has arrived.
        let bytes = payload(index * 4096, 4096, 42);
        let started = Instant::now();
        writer.write_all(&bytes).await?;
        let mut echoed = Vec::new();
        while echoed.len() < bytes.len() {
            let frame = response
                .body_mut()
                .frame()
                .await
                .ok_or_else(|| io::Error::other("duplex ended before upload EOF"))??;
            let data = frame
                .into_data()
                .map_err(|_| io::Error::other("unexpected trailer"))?;
            echoed.extend_from_slice(&data);
        }
        if echoed != bytes {
            return Err(io::Error::other("duplex block mismatch").into());
        }
        maximum = maximum.max(started.elapsed().as_secs_f64());
    }
    writer.shutdown().await?;
    let tail = receive(response, Duration::ZERO).await?;
    if tail["bytes"] != 0 {
        return Err(io::Error::other("unexpected duplex tail").into());
    }
    Ok(
        serde_json::json!({"chunks_before_upload_eof": 3, "idle_s": idle.as_secs_f64(), "max_chunk_rtt_s": maximum}),
    )
}

fn record(
    results: &mut Vec<serde_json::Value>,
    name: &str,
    result: std::result::Result<serde_json::Value, dhttp::BoxError>,
    started: Instant,
) {
    use std::io::Write;
    let value = match result {
        Ok(metrics) => {
            serde_json::json!({"case": name, "status": "passed", "wall_s": started.elapsed().as_secs_f64(), "metrics": metrics})
        }
        Err(error) => {
            serde_json::json!({"case": name, "status": "failed", "wall_s": started.elapsed().as_secs_f64(), "error": error.to_string()})
        }
    };
    let mut value = value;
    value["recorded_utc"] = chrono::Utc::now().to_rfc3339().into();
    println!("TRANSPORT_RESULT {value}");
    if let Some(path) = std::env::var_os("PISHOO_TRANSPORT_RESULTS") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{value}").unwrap();
    }
    results.push(value);
}

async fn bounded<F>(future: F) -> std::result::Result<serde_json::Value, dhttp::BoxError>
where
    F: std::future::Future<Output = std::result::Result<serde_json::Value, dhttp::BoxError>>,
{
    timeout(Duration::from_secs(900), future)
        .await
        .map_err(|_| io::Error::other("case exceeded 900 second deadline"))?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run alone; isolates real QUIC setup on a multi-thread runtime"]
async fn transport_multithread_connectivity() {
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter("warn,dhttp=trace,qconnection=debug,qtls=debug")
        .try_init();
    let root = tempfile::tempdir().unwrap();
    credentials::generate(
        root.path(),
        &[
            ("receiver", "receiver.dhttp.net"),
            ("alice", "alice.dhttp.net"),
        ],
    );
    unsafe {
        std::env::set_var("DHTTP_HOME", root.path());
    }
    dhttp::DhttpNetwork::init().await.unwrap();
    qtls::RootCerts::set([dhttp::CertificateDer::from(
        std::fs::read(root.path().join("ca.der")).unwrap(),
    )])
    .unwrap();
    let peer = Arc::new(qprotocol::UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap());
    qprotocol::Dock::global()
        .add(peer.clone())
        .unwrap()
        .unwrap();
    dhttp::resolve::Resolver::add(Arc::new(PeerResolver(EndpointAddr::direct(
        peer.local_addr().unwrap(),
    ))));
    let receiver = dhttp::Endpoint::load("receiver").await.unwrap();
    let alice = dhttp::Endpoint::load("alice").await.unwrap();
    let mut listeners = Vec::new();
    for endpoint in [&receiver, &alice] {
        listeners.push(tokio::spawn(
            endpoint
                .listen(
                    dhttp::Scope::Loopback | dhttp::Scope::Internal,
                    tower::service_fn(|_: Request<Body>| async {
                        Ok::<_, Infallible>(http::Response::new(AxumBody::from("probe")))
                    }),
                )
                .await
                .unwrap(),
        ));
    }
    let _listeners = scopeguard::guard(listeners, |tasks| {
        for task in tasks {
            task.abort();
        }
    });
    let response = timeout(
        Duration::from_secs(30),
        alice
            .get("https://receiver.dhttp.net/probe".parse().unwrap())
            .into_future(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "probe"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run alone; spawns one Pishoo router process and two native QUIC client processes"]
async fn transport_cross_process_acceptance() {
    use std::process::{Command, Stdio};
    let cold_probe = std::env::var_os("PISHOO_TRANSPORT_COLD_PROBE").is_some();
    let file_length = if cold_probe { MIB } else { 100 * MIB };
    let discovery =
        std::env::var("PISHOO_TRANSPORT_DISCOVERY").unwrap_or_else(|_| "system-loopback".into());
    assert!(matches!(discovery.as_str(), "system-loopback" | "mdns"));
    let role = std::env::var("PISHOO_TRANSPORT_PROCESS_ROLE").ok();
    if let Some(role) = role {
        let _ = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter("warn,dhttp=trace,qconnection=debug,qtls=debug")
            .try_init();
        let home = std::path::PathBuf::from(std::env::var_os("DHTTP_HOME").unwrap());
        dhttp::DhttpNetwork::init().await.unwrap();
        qtls::RootCerts::set([dhttp::CertificateDer::from(
            std::fs::read(home.join("ca.der")).unwrap(),
        )])
        .unwrap();
        let mdns = if discovery == "mdns" {
            let bindings = dhttp::AddressBook::global().inner_bindings();
            let (bound, device) = bindings
                .iter()
                .find(|(bound, device)| bound.is_ipv4() && matches!(device.name(), "en0" | "eth0"))
                .expect("LAN IPv4 interface");
            Some(Arc::new(
                ddns::mdns::MdnsResolver::bind(
                    ddns::resolvers::DHTTP_MDNS_SERVICE_DOMAIN,
                    device.name(),
                    bound.ip(),
                )
                .await
                .unwrap(),
            ))
        } else {
            None
        };
        if role == "server" {
            let bind = mdns
                .as_ref()
                .map_or("127.0.0.1:0".parse().unwrap(), |resolver| {
                    std::net::SocketAddr::new(resolver.bound_ip(), 0)
                });
            let peer = Arc::new(qprotocol::UdpSocket::bind(bind).unwrap());
            qprotocol::Dock::global()
                .add(peer.clone())
                .unwrap()
                .unwrap();
            let _socket = scopeguard::guard(peer.clone(), |peer| {
                qprotocol::Dock::global().remove(&peer);
            });
            let profile = IdentityProfile::try_from(home.join("receiver")).unwrap();
            let mut server = Server::load(profile, Arc::new(WasmRuntime::new().unwrap()))
                .await
                .unwrap();
            for path in ["/std/file", "/std/api/echo"] {
                server
                    .access
                    .set_policy(
                        access_control::Method::Unspecified,
                        path,
                        access_control::Effect::Allow,
                        access_control::Grantee::Named,
                    )
                    .await
                    .unwrap();
            }
            let app = server.router.clone();
            let listener = server
                .endpoint
                .listen(
                    dhttp::Scope::Loopback | dhttp::Scope::Internal,
                    tower::service_fn(move |request: Request<Body>| {
                        let app = app.read().unwrap().clone();
                        async move { app.oneshot(request.map(AxumBody::new)).await }
                    }),
                )
                .await
                .unwrap();
            let _listener = scopeguard::guard(tokio::spawn(listener), |task| task.abort());
            if let Some(mdns) = &mdns {
                mdns.publish_endpoints(
                    &server.endpoint.local_authority().unwrap(),
                    server.name(),
                    [EndpointAddr::direct(peer.local_addr().unwrap())],
                )
                .unwrap();
            }
            std::fs::write(home.join("ready"), peer.local_addr().unwrap().to_string()).unwrap();
            while !home.join("stop").exists() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            server.close().await.unwrap();
            if let Some(mdns) = &mdns {
                mdns.shutdown().await.unwrap();
            }
        } else {
            assert_eq!(role, "client");
            let address = std::fs::read_to_string(home.join("ready"))
                .unwrap()
                .parse()
                .unwrap();
            if let Some(mdns) = &mdns {
                dhttp::resolve::Resolver::add(mdns.clone());
            } else {
                dhttp::resolve::Resolver::add(Arc::new(PeerResolver(EndpointAddr::direct(
                    address,
                ))));
            }
            let identity = std::env::var("PISHOO_TRANSPORT_PROCESS_IDENTITY").unwrap();
            let client = dhttp::Endpoint::load(&identity).await.unwrap();
            let digest = std::fs::read_to_string(home.join("file.sha256")).unwrap();
            let mut results = Vec::new();
            let started = Instant::now();
            record(
                &mut results,
                &format!("process_{identity}_download_{file_length}_bytes"),
                bounded(download(
                    &client,
                    "https://receiver.dhttp.net/std/file/process.bin",
                    file_length,
                    &digest,
                ))
                .await,
                started,
            );
            if cold_probe {
                if let Some(mdns) = &mdns {
                    let records = timeout(Duration::from_secs(5), async {
                        mdns.lookup("receiver.dhttp.net", "", None)
                            .await
                            .unwrap()
                            .collect::<Vec<_>>()
                            .await
                    })
                    .await
                    .unwrap();
                    assert!(!records.is_empty());
                    for (source, peer) in records {
                        assert_eq!(
                            source,
                            Source::Mdns {
                                nic: mdns.bound_device().into(),
                                family: Family::V4
                            }
                        );
                        let paths = dhttp::AddressBook::global().pathways_to(peer, &source);
                        assert!(!paths.is_empty());
                        assert!(
                            paths
                                .iter()
                                .all(|path| path.local().addr().ip() == mdns.bound_ip())
                        );
                        println!(
                            "COLD_SOURCE source={source:?} peer={peer:?} candidates={paths:?}"
                        );
                    }
                }
                for attempt in 2..=3 {
                    let started = Instant::now();
                    record(
                        &mut results,
                        &format!("process_{identity}_request_{attempt}"),
                        bounded(download(
                            &client,
                            "https://receiver.dhttp.net/std/file/process.bin",
                            file_length,
                            &digest,
                        ))
                        .await,
                        started,
                    );
                }
                assert!(
                    results.iter().all(|value| value["status"] == "passed"),
                    "{results:#?}"
                );
                return;
            }
            let started = Instant::now();
            let batch = async {
                let values = try_join_all((0..8).map(|index| {
                    echo(
                        &client,
                        "https://receiver.dhttp.net/std/api/echo/echo",
                        MIB,
                        index + if identity == "alice" { 600 } else { 700 },
                        Duration::ZERO,
                        Duration::ZERO,
                    )
                }))
                .await?;
                Ok(
                    serde_json::json!({"requests": values.len(), "bytes": values.len() * MIB, "elapsed_s": started.elapsed().as_secs_f64()}),
                )
            };
            record(
                &mut results,
                &format!("process_{identity}_eight_wasm_streams"),
                bounded(batch).await,
                started,
            );
            // Synchronize two downloads on already established connections.
            let started = Instant::now();
            record(
                &mut results,
                &format!("process_{identity}_duplex_no_idle"),
                bounded(duplex(
                    &client,
                    "https://receiver.dhttp.net/std/api/echo/echo",
                    Duration::ZERO,
                ))
                .await,
                started,
            );
            let started = Instant::now();
            record(
                &mut results,
                &format!("process_{identity}_slow_reader_1ms"),
                bounded(echo(
                    &client,
                    "https://receiver.dhttp.net/std/api/echo/echo",
                    2 * MIB,
                    800,
                    Duration::ZERO,
                    Duration::from_millis(1),
                ))
                .await,
                started,
            );
            // Synchronize two downloads on already established connections.
            // Keep the cold-start result separately; no failed request is retried silently.
            std::fs::write(home.join(format!("{identity}-ready")), []).unwrap();
            timeout(Duration::from_secs(900), async {
                while !home.join("transfer").exists() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            let started = Instant::now();
            record(
                &mut results,
                &format!("process_{identity}_warm_download_100_mib"),
                bounded(download(
                    &client,
                    "https://receiver.dhttp.net/std/file/process.bin",
                    100 * MIB,
                    &digest,
                ))
                .await,
                started,
            );
            assert!(
                results.iter().all(|value| value["status"] == "passed"),
                "{results:#?}"
            );
        }
        return;
    }

    let root = tempfile::tempdir().unwrap();
    credentials::generate(
        root.path(),
        &[
            ("receiver", "receiver.dhttp.net"),
            ("alice", "alice.dhttp.net"),
            ("bob", "bob.dhttp.net"),
        ],
    );
    let receiver = profile(root.path(), "receiver");
    std::fs::create_dir_all(receiver.join("file")).unwrap();
    std::fs::create_dir_all(receiver.join("lib/echo")).unwrap();
    std::fs::write(
        receiver.join("lib/echo/lib.wasm"),
        include_bytes!("../../../assets/tcp-demo-echo.wasm"),
    )
    .unwrap();
    let digest = fixture(&receiver.join("file/process.bin"), file_length);
    std::fs::write(root.path().join("file.sha256"), digest).unwrap();
    let evidence = std::env::var_os("PISHOO_TRANSPORT_PROCESS_EVIDENCE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.path().join("evidence"));
    std::fs::create_dir_all(&evidence).unwrap();
    let executable = std::env::current_exe().unwrap();
    let case = "server::network_tests::transport::transport_cross_process_acceptance";
    let spawn = |role: &str, identity: &str| {
        let log = std::fs::File::create(evidence.join(format!("{identity}.log"))).unwrap();
        let mut command = Command::new(&executable);
        command
            .args([
                case,
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("DHTTP_HOME", root.path())
            .env("PISHOO_TRANSPORT_PROCESS_ROLE", role)
            .env("PISHOO_TRANSPORT_PROCESS_IDENTITY", identity)
            .env(
                "PISHOO_TRANSPORT_RESULTS",
                evidence.join(format!("{identity}.jsonl")),
            )
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));
        scopeguard::guard(command.spawn().unwrap(), |mut child| {
            let _ = child.kill();
            let _ = child.wait();
        })
    };
    let started = Instant::now();
    let mut server = spawn("server", "receiver");
    timeout(Duration::from_secs(60), async {
        while !root.path().join("ready").exists() {
            assert!(
                server.try_wait().unwrap().is_none(),
                "server exited before ready; see {}",
                evidence.display()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let mut clients = [spawn("client", "alice"), spawn("client", "bob")];
    if !cold_probe {
        timeout(Duration::from_secs(900), async {
            while !["alice", "bob"]
                .iter()
                .all(|identity| root.path().join(format!("{identity}-ready")).exists())
            {
                assert!(
                    clients
                        .iter_mut()
                        .all(|client| client.try_wait().unwrap().is_none()),
                    "client exited before transfer barrier; see {}",
                    evidence.display()
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        std::fs::write(root.path().join("transfer"), []).unwrap();
    }
    let mut client_statuses = Vec::new();
    for client in &mut clients {
        let status = timeout(Duration::from_secs(900), async {
            loop {
                if let Some(status) = client.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        client_statuses.push(status);
    }
    std::fs::write(root.path().join("stop"), []).unwrap();
    let status = timeout(Duration::from_secs(30), async {
        loop {
            if let Some(status) = server.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    assert!(status.success(), "server shutdown failed");
    let values = ["alice", "bob"]
        .into_iter()
        .flat_map(|identity| {
            std::fs::read_to_string(evidence.join(format!("{identity}.jsonl")))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    println!(
        "PROCESS_RESULTS {}",
        serde_json::json!({"elapsed_s": started.elapsed().as_secs_f64(), "processes": 3, "cases": values})
    );
    assert!(
        client_statuses.iter().all(|status| status.success()),
        "client failed; see {}",
        evidence.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run alone; real UDP/TLS/H3, OpenSSL, 1.1 GiB disk and optional 30 minute soak"]
async fn transport_large_duplex_concurrency_acceptance() {
    use hyper::{body::Incoming, service::service_fn};
    use hyper_util::rt::TokioIo;
    use tokio_util::io::ReaderStream;

    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,dhttp=debug,qtls=debug,qconnection=info".into()),
        )
        .try_init();
    let root = tempfile::tempdir().unwrap();
    println!("TRANSPORT_SETUP temporary_home={}", root.path().display());
    credentials::generate(
        root.path(),
        &[
            ("receiver", "receiver.dhttp.net"),
            ("alice", "alice.dhttp.net"),
            ("bob", "bob.dhttp.net"),
        ],
    );
    let old_home = std::env::var_os("DHTTP_HOME");
    unsafe {
        std::env::set_var("DHTTP_HOME", root.path());
    }
    let _environment = scopeguard::guard(old_home, |previous| unsafe {
        match previous {
            Some(value) => std::env::set_var("DHTTP_HOME", value),
            None => std::env::remove_var("DHTTP_HOME"),
        }
    });
    dhttp::DhttpNetwork::init().await.unwrap();
    qtls::RootCerts::set([dhttp::CertificateDer::from(
        std::fs::read(root.path().join("ca.der")).unwrap(),
    )])
    .unwrap();
    let peer = Arc::new(qprotocol::UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap());
    let address = EndpointAddr::direct(peer.local_addr().unwrap());
    qprotocol::Dock::global()
        .add(peer.clone())
        .unwrap()
        .unwrap();
    let _socket = scopeguard::guard(peer, |peer| {
        qprotocol::Dock::global().remove(&peer);
    });
    dhttp::resolve::Resolver::add(Arc::new(PeerResolver(address)));

    let receiver_profile = profile(root.path(), "receiver");
    std::fs::create_dir_all(receiver_profile.join("file")).unwrap();
    std::fs::create_dir_all(receiver_profile.join("lib/echo")).unwrap();
    std::fs::write(
        receiver_profile.join("lib/echo/lib.wasm"),
        include_bytes!("../../../assets/tcp-demo-echo.wasm"),
    )
    .unwrap();
    let mut files = Vec::new();
    println!("TRANSPORT_SETUP generating_file_fixtures");
    for length in [
        0,
        1,
        4095,
        4096,
        65535,
        65536,
        65537,
        MIB,
        100 * MIB,
        1024 * MIB,
    ] {
        let name = format!("bytes-{length}.bin");
        files.push((
            name.clone(),
            length,
            fixture(&receiver_profile.join(&format!("file/{name}")), length),
        ));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = listener.local_addr().unwrap();
    let file_root = receiver_profile.join("file");
    let upstream = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let file_root = file_root.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<Incoming>| {
                    let file_root = file_root.clone();
                    async move {
                        let response = if request.method() == Method::POST
                            && request.uri().path() == "/echo"
                        {
                            http::Response::new(AxumBody::new(request.into_body()))
                        } else if request.uri().path().starts_with("/file/") {
                            let name = request.uri().path().trim_start_matches("/file/");
                            let file = tokio::fs::File::open(file_root.join(name)).await.unwrap();
                            let length = file.metadata().await.unwrap().len();
                            http::Response::builder()
                                .header(http::header::CONTENT_LENGTH, length)
                                .body(AxumBody::from_stream(ReaderStream::with_capacity(
                                    file, CHUNK,
                                )))
                                .unwrap()
                        } else {
                            http::Response::builder()
                                .status(404)
                                .body(AxumBody::empty())
                                .unwrap()
                        };
                        Ok::<_, Infallible>(response)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(socket), service)
                    .await;
            });
        }
    });
    let _upstream = scopeguard::guard(upstream, |task| task.abort());
    let db = rusqlite::Connection::open(receiver_profile.config_db_path()).unwrap();
    db.execute(
        "INSERT INTO proxy_locations VALUES('/transport/',?1)",
        [format!("http://{upstream_address}/")],
    )
    .unwrap();
    drop(db);
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    println!("TRANSPORT_SETUP loading_servers");
    let mut receiver = Server::load(receiver_profile, runtime.clone())
        .await
        .unwrap();
    let alice_profile = profile(root.path(), "alice");
    std::fs::create_dir_all(alice_profile.join("file")).unwrap();
    let reverse_digest = fixture(&alice_profile.join("file/reverse.bin"), 100 * MIB);
    let mut alice = Server::load(alice_profile, runtime).await.unwrap();
    let bob = dhttp::Endpoint::load("bob").await.unwrap();
    let mut listeners = Vec::new();
    for server in [&receiver, &alice] {
        for path in ["/std/file", "/std/api/echo", "/transport"] {
            server
                .access
                .set_policy(
                    access_control::Method::Unspecified,
                    path,
                    access_control::Effect::Allow,
                    access_control::Grantee::Named,
                )
                .await
                .unwrap();
        }
        let app = server.router.clone();
        let listener = server
            .endpoint
            .listen(
                // The global Network can use an Internal source socket even when
                // the peer destination is 127.0.0.1. Permit that actual path.
                dhttp::Scope::Loopback | dhttp::Scope::Internal,
                tower::service_fn(move |request: Request<Body>| {
                    let app = app.read().unwrap().clone();
                    async move { app.oneshot(request.map(AxumBody::new)).await }
                }),
            )
            .await
            .unwrap();
        listeners.push(tokio::spawn(listener));
    }
    let _listeners = scopeguard::guard(listeners, |tasks| {
        for task in tasks {
            task.abort();
        }
    });
    let mut results = Vec::new();
    println!("TRANSPORT_SETUP ready");
    let client = &alice.endpoint;
    let wasm_uri = "https://receiver.dhttp.net/std/api/echo/echo";
    let proxy_uri = "https://receiver.dhttp.net/transport/echo";
    let small = &files[7];
    let small_uri = format!("https://receiver.dhttp.net/std/file/{}", small.0);

    for (name, length, digest) in &files {
        let started = Instant::now();
        let uri = format!("https://receiver.dhttp.net/std/file/{name}");
        record(
            &mut results,
            &format!("static_{length}_bytes"),
            bounded(download(client, &uri, *length, digest)).await,
            started,
        );
    }
    let started = Instant::now();
    let head = async {
        let response = client
            .head(format!("https://receiver.dhttp.net/std/file/{}", files[9].0).parse()?)
            .await?;
        check_response(&response)?;
        if response.headers()[http::header::CONTENT_LENGTH] != (1024 * MIB).to_string() {
            return Err(io::Error::other("HEAD Content-Length mismatch").into());
        }
        let body = receive(response, Duration::ZERO).await?;
        if body["bytes"] != 0 {
            return Err(io::Error::other("HEAD returned data").into());
        }
        Ok(body)
    };
    record(&mut results, "head_1_gib", bounded(head).await, started);
    let started = Instant::now();
    record(
        &mut results,
        "proxy_download_100_mib",
        bounded(download(
            client,
            &format!("https://receiver.dhttp.net/transport/file/{}", files[8].0),
            files[8].1,
            &files[8].2,
        ))
        .await,
        started,
    );
    for (kind, uri) in [("wasm", wasm_uri), ("proxy", proxy_uri)] {
        for length in [0, 1, 65535, 65536, 65537, MIB, 100 * MIB] {
            let started = Instant::now();
            record(
                &mut results,
                &format!("{kind}_echo_{length}_bytes"),
                bounded(echo(
                    client,
                    uri,
                    length,
                    17,
                    Duration::ZERO,
                    Duration::ZERO,
                ))
                .await,
                started,
            );
        }
        let started = Instant::now();
        record(
            &mut results,
            &format!("{kind}_duplex_idle_31s"),
            bounded(duplex(client, uri, Duration::from_secs(31))).await,
            started,
        );
        for (name, write_delay, read_delay) in [
            ("slow_writer", Duration::from_millis(20), Duration::ZERO),
            ("slow_reader", Duration::ZERO, Duration::from_millis(20)),
        ] {
            let started = Instant::now();
            record(
                &mut results,
                &format!("{kind}_{name}"),
                bounded(echo(client, uri, 2 * MIB, 19, write_delay, read_delay)).await,
                started,
            );
        }
    }
    let started = Instant::now();
    record(
        &mut results,
        "proxy_echo_1_gib",
        bounded(echo(
            client,
            proxy_uri,
            1024 * MIB,
            23,
            Duration::ZERO,
            Duration::ZERO,
        ))
        .await,
        started,
    );
    for concurrent in [1, 8, 32, 128] {
        for (kind, uri) in [
            ("static", small_uri.as_str()),
            ("wasm", wasm_uri),
            ("proxy", proxy_uri),
        ] {
            let started = Instant::now();
            let batch = async {
                let requests = (0..concurrent).map(|index| async move {
                    if kind == "static" {
                        download(client, uri, small.1, &small.2).await
                    } else {
                        echo(
                            client,
                            uri,
                            MIB,
                            index + 100,
                            Duration::ZERO,
                            Duration::ZERO,
                        )
                        .await
                    }
                });
                let values = try_join_all(requests).await?;
                let mut latencies = values
                    .iter()
                    .map(|value| value["elapsed_s"].as_f64().unwrap())
                    .collect::<Vec<_>>();
                latencies.sort_by(f64::total_cmp);
                Ok(
                    serde_json::json!({"requests": concurrent, "bytes": concurrent * MIB, "elapsed_s": started.elapsed().as_secs_f64(), "aggregate_mib_per_s": concurrent as f64 / started.elapsed().as_secs_f64(), "p50_s": latencies[(concurrent - 1) / 2], "p95_s": latencies[(concurrent * 95).div_ceil(100) - 1], "p99_s": latencies[(concurrent * 99).div_ceil(100) - 1]}),
                )
            };
            record(
                &mut results,
                &format!("concurrent_{kind}_{concurrent}"),
                bounded(batch).await,
                started,
            );
        }
    }
    let started = Instant::now();
    let reverse = async {
        let forward_uri = format!("https://receiver.dhttp.net/std/file/{}", files[8].0);
        let (forward, reverse) = tokio::try_join!(
            download(client, &forward_uri, files[8].1, &files[8].2),
            download(
                &receiver.endpoint,
                "https://alice.dhttp.net/std/file/reverse.bin",
                100 * MIB,
                &reverse_digest
            )
        )?;
        Ok(serde_json::json!({"a_to_b": forward, "b_to_a": reverse}))
    };
    record(
        &mut results,
        "simultaneous_reverse_requests_100_mib",
        bounded(reverse).await,
        started,
    );
    let started = Instant::now();
    let named_clients = async {
        let clients = [&alice.endpoint, &bob];
        let values = try_join_all((0..16).map(|index| {
            echo(
                clients[index % 2],
                proxy_uri,
                MIB,
                index + 300,
                Duration::ZERO,
                Duration::ZERO,
            )
        }))
        .await?;
        Ok(
            serde_json::json!({"identities": 2, "requests": values.len(), "elapsed_s": started.elapsed().as_secs_f64()}),
        )
    };
    record(
        &mut results,
        "two_named_clients_16_streams",
        bounded(named_clients).await,
        started,
    );
    let started = Instant::now();
    let mixed = async {
        let large_uri = format!("https://receiver.dhttp.net/std/file/{}", files[9].0);
        let large = download(client, &large_uri, files[9].1, &files[9].2);
        let probes = async {
            let mut values = Vec::new();
            for _ in 0..32 {
                values.push(download(client, &small_uri, small.1, &small.2).await?);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, dhttp::BoxError>(values)
        };
        let (large, probes) = tokio::try_join!(large, probes)?;
        let mut latency = probes
            .iter()
            .map(|p| p["elapsed_s"].as_f64().unwrap())
            .collect::<Vec<_>>();
        latency.sort_by(f64::total_cmp);
        Ok(
            serde_json::json!({"large": large, "small_requests": probes.len(), "small_p95_s": latency[30], "small_p99_s": latency[31]}),
        )
    };
    record(
        &mut results,
        "mixed_1_gib_and_small_requests",
        bounded(mixed).await,
        started,
    );
    for (kind, uri) in [("wasm", wasm_uri), ("proxy", proxy_uri)] {
        let started = Instant::now();
        let cancel = async {
            for index in 0..16 {
                let (mut writer, response) = client.post(uri.parse()?).await?;
                let mut response = response.await?;
                check_response(&response)?;
                writer.write_all(&payload(0, 4096, index)).await?;
                response
                    .body_mut()
                    .frame()
                    .await
                    .ok_or_else(|| io::Error::other("cancel stream ended early"))??;
                drop(response);
                drop(writer);
            }
            let survivor = download(client, &small_uri, small.1, &small.2).await?;
            timeout(Duration::from_secs(10), async {
                while !receiver.sandbox.tasks.is_empty() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .map_err(|_| io::Error::other("WASM tasks remain after cancellation"))?;
            Ok(
                serde_json::json!({"cancelled_streams": 16, "survivor": survivor, "remaining_wasm_tasks": receiver.sandbox.tasks.len()}),
            )
        };
        record(
            &mut results,
            &format!("{kind}_cancel_and_reuse"),
            bounded(cancel).await,
            started,
        );
    }
    let started = Instant::now();
    let dropped = async {
        for _ in 0..16 {
            let response = client
                .get(format!("https://receiver.dhttp.net/std/file/{}", files[9].0).parse()?)
                .await?;
            check_response(&response)?;
            drop(response);
        }
        download(client, &small_uri, small.1, &small.2).await
    };
    record(
        &mut results,
        "cancel_large_download_and_reuse",
        bounded(dropped).await,
        started,
    );
    let started = Instant::now();
    let cancellation_isolation = async {
        let survivors = try_join_all((0..8).map(|index| {
            echo(
                client,
                proxy_uri,
                4 * MIB,
                index + 500,
                Duration::from_millis(5),
                Duration::ZERO,
            )
        }));
        let cancel = async {
            let response = client
                .get(format!("https://receiver.dhttp.net/std/file/{}", files[9].0).parse()?)
                .await?;
            check_response(&response)?;
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(response);
            Ok::<_, dhttp::BoxError>(())
        };
        let (survivors, ()) = tokio::try_join!(survivors, cancel)?;
        Ok(
            serde_json::json!({"unaffected_streams": survivors.len(), "bytes": survivors.len() * 4 * MIB}),
        )
    };
    record(
        &mut results,
        "cancel_one_stream_preserves_eight_active_streams",
        bounded(cancellation_isolation).await,
        started,
    );
    let started = Instant::now();
    let authorization = async {
        let request = Request::builder()
            .uri(&small_uri)
            .body(http_body_util::Empty::<Bytes>::new())?;
        let response = dhttp::Request::new(request).await?;
        if response.status() != StatusCode::FORBIDDEN {
            return Err(io::Error::other("anonymous test file access allowed").into());
        }
        response.into_body().collect().await?;
        Ok(serde_json::json!({"anonymous_status": 403}))
    };
    record(
        &mut results,
        "anonymous_access_denied",
        bounded(authorization).await,
        started,
    );

    let seconds: u64 = std::env::var("PISHOO_TRANSPORT_SOAK_SECONDS")
        .unwrap_or_else(|_| "60".into())
        .parse()
        .unwrap();
    let started = Instant::now();
    let soak = async {
        let mut requests = 0usize;
        let mut maximum = 0.0_f64;
        let until = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < until {
            let small_uri = small_uri.as_str();
            let batch = try_join_all((0..8).map(|index| async move {
                match index % 3 {
                    0 => download(client, small_uri, small.1, &small.2).await,
                    1 => {
                        echo(
                            client,
                            wasm_uri,
                            256 * 1024,
                            index + requests,
                            Duration::ZERO,
                            Duration::ZERO,
                        )
                        .await
                    }
                    _ => {
                        echo(
                            client,
                            proxy_uri,
                            MIB,
                            index + requests,
                            Duration::ZERO,
                            Duration::ZERO,
                        )
                        .await
                    }
                }
            }));
            let values = timeout(Duration::from_secs(30), batch)
                .await
                .map_err(|_| io::Error::other("soak batch deadline"))??;
            for value in values {
                maximum = maximum.max(value["elapsed_s"].as_f64().unwrap());
            }
            requests += 8;
            if requests % 80 == 0 {
                println!(
                    "TRANSPORT_PROGRESS requests={requests} elapsed_s={:.1}",
                    started.elapsed().as_secs_f64()
                );
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Ok(
            serde_json::json!({"requests": requests, "elapsed_s": started.elapsed().as_secs_f64(), "max_request_s": maximum, "remaining_wasm_tasks": receiver.sandbox.tasks.len()}),
        )
    };
    record(&mut results, "mixed_soak", soak.await, started);
    let started = Instant::now();
    let close = async {
        receiver.close().await?;
        alice.close().await?;
        Ok(serde_json::json!({"remaining_wasm_tasks": receiver.sandbox.tasks.len()}))
    };
    record(&mut results, "application_shutdown", close.await, started);
    let failures = results
        .iter()
        .filter(|value| value["status"] == "failed")
        .collect::<Vec<_>>();
    assert!(failures.is_empty(), "transport failures: {failures:#?}");
}

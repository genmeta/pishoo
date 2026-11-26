//! Browser acceptance fixture. Each loopback port authenticates as its own disposable owner.
//! Set PISHOO_ZERO_ARTIFACTS and PISHOO_USOPP_ARTIFACTS to the builds under test.
//! Each directory contains profile/, message/ and group/ with component.wasm, openapi.json
//! and manifest.json (package metadata and dependencies). Point each Workspace development
//! server at its printed backend port and set VITE_PISHOO_IDENTITY to the matching full name.
use super::*;
use crate::package::tests::{package_fixture, publisher};
use serde_json::{Value, json};
use std::path::Path;

fn install_services(artifacts: &Path, profile: &IdentityProfile, runtime: Arc<WasmRuntime>) {
    let author = "dev.genmeta.tech.dhttp.net";
    let signer = publisher(author, 'a');
    let mut packages = Vec::new();
    for package in ["profile", "message", "group"] {
        let dir = artifacts.join(package);
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        let mut api: Value =
            serde_json::from_slice(&std::fs::read(dir.join("openapi.json")).unwrap()).unwrap();
        let name = manifest["package"]["name"].as_str().unwrap();
        api["info"]["description"] = manifest["package"]["description"].clone();
        api["info"]["license"] = json!({"name":"Apache-2.0"});
        api["x-pishoo"] = json!({"kind":"lib","publisher":author,"name":name,"dependencies":manifest["dependencies"]});
        let component = std::fs::read(dir.join("component.wasm")).unwrap();
        let package = package_fixture(&api, &component, BTreeMap::new(), &signer);
        crate::package::read_package(&package)
            .unwrap_or_else(|e| panic!("invalid {name} fixture: {e:?}"));
        packages.push((format!("{author}/{name}"), package));
    }
    crate::package::tests::install_fixture_packages(profile, &packages, &mut Sandbox::new(runtime))
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "manual browser acceptance; requires two component fixture directories and exclusive use of TLS, DNS, home and loopback ports"]
async fn communication_workspace_two_identities() {
    let root = tempfile::tempdir().unwrap();
    let names = [
        ("zero.ai", "zero.ai.dhttp.net"),
        ("usopp.li", "usopp.li.dhttp.net"),
    ];
    credentials::generate(root.path(), &names);
    let home = std::env::var_os("DHTTP_HOME");
    unsafe { std::env::set_var("DHTTP_HOME", root.path()) };
    let _home = scopeguard::guard(home, |value| unsafe {
        match value {
            Some(path) => std::env::set_var("DHTTP_HOME", path),
            None => std::env::remove_var("DHTTP_HOME"),
        }
    });
    dhttp::DhttpNetwork::init().await.unwrap();
    qtls::RootCerts::set([qtls::CertificateDer::from(
        std::fs::read(root.path().join("ca.der")).unwrap(),
    )])
    .unwrap();
    let socket = Arc::new(qprotocol::UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap());
    let address = EndpointAddr::direct(socket.local_addr().unwrap());
    qprotocol::Dock::global()
        .add(socket.clone())
        .unwrap()
        .unwrap();
    let _socket = scopeguard::guard(socket, |socket| {
        qprotocol::Dock::global().remove(&socket);
    });
    dhttp::resolve::Resolver::add(Arc::new(PeerResolver(address)));
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let mut servers = Vec::new();
    for ((short, _), (variable, port)) in names.into_iter().zip([
        ("PISHOO_ZERO_ARTIFACTS", 18751),
        ("PISHOO_USOPP_ARTIFACTS", 18752),
    ]) {
        let artifacts = std::env::var_os(variable).expect("set component fixture directories");
        let profile = IdentityProfile::try_from(root.path().join(short)).unwrap();
        initialize_directories(&profile).unwrap();
        crate::configure(&profile, Some("off"), true).unwrap();
        install_services(Path::new(&artifacts), &profile, runtime.clone());
        let mut server = Server::load(profile, runtime.clone()).await.unwrap();
        server.config.listen = "loopback".into();
        let _listener = tokio::spawn(server.listen().await.unwrap());
        let endpoint = server.endpoint.clone();
        let bridge = Router::new().fallback(any(move |mut request: Request<AxumBody>| {
            let endpoint = endpoint.clone();
            async move {
                let uri = request.uri().clone();
                if !(uri.path().starts_with("/std/") || uri.path().starts_with("/test-peer/std/"))
                    || uri
                        .path()
                        .strip_prefix("/std/forward/")
                        .is_some_and(|s| !names.iter().any(|(_, n)| s.split('/').next() == Some(n)))
                {
                    return StatusCode::NOT_FOUND.into_response();
                }
                let target = uri.path_and_query().unwrap().as_str();
                let (host, path) = if let Some(path) = target.strip_prefix("/test-peer") {
                    if path.starts_with("/std/forward/") {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    (
                        names
                            .iter()
                            .find(|(_, name)| *name != endpoint.name())
                            .unwrap()
                            .1,
                        path,
                    )
                } else {
                    (endpoint.name(), target)
                };
                *request.uri_mut() = format!("/std/forward/{host}{path}").parse().unwrap();
                request
                    .extensions_mut()
                    .insert(endpoint.local_authority().unwrap().clone());
                let mut response = forward_dhttp(endpoint, request).await;
                *response.version_mut() = http::Version::HTTP_11;
                response
            }
        }));
        let tcp = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = tcp.accept().await.unwrap();
                let bridge = bridge.clone();
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(
                            hyper_util::rt::TokioIo::new(stream),
                            hyper::service::service_fn(move |request| {
                                bridge.clone().oneshot(request)
                            }),
                        )
                        .await;
                });
            }
        });
        eprintln!("{short} browser backend ready on 127.0.0.1:{port}");
        servers.push(server);
    }
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    let finish = tokio::time::sleep(Duration::from_secs(1200));
    tokio::pin!(finish);
    loop {
        tokio::select! {
            _ = &mut finish => break,
            _ = &mut interrupt => break,
            _ = tick.tick() => for server in &servers {
                crate::contact::contact_jobs(&server.profile, &server.endpoint, &server.access, &server.contact_db).await.unwrap();
            }
        }
    }
    for mut server in servers {
        server.close().await.unwrap();
    }
}

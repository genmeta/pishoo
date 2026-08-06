#![cfg(unix)]

use std::{
    fmt,
    net::{SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use dhttp::{
    dquic::{
        binds::BindPattern,
        client::ServerCertVerifierChoice,
        qresolve::EndpointAddr,
        resolver::{Resolve, ResolveFuture, Source},
    },
    endpoint::Endpoint,
    home::identity::IdentityProfile,
    identity::RemoteAuthorityCertificateExt,
};
use futures::{FutureExt, StreamExt, stream};
use pishoo::{
    hypervisor::{in_process_plane::InProcessControlPlane, state::RootState},
    service::{runtime::RuntimeRegistry, source::TypedServerSource},
};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyIdMethod, KeyPair, KeyUsagePurpose, SerialNumber,
};

const SERVER_NAME: &str = "reload-test.dhttp.net";
const OWNER_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> TestResult<Self> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "pishoo-identity-hot-reload-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug)]
struct FixedResolver(SocketAddr);

impl fmt::Display for FixedResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("identity hot-reload fixed resolver")
    }
}

impl Resolve for FixedResolver {
    fn lookup<'a>(&'a self, _name: &'a str) -> ResolveFuture<'a> {
        let endpoint = EndpointAddr::direct(self.0);
        async move { Ok(stream::iter([(Source::System, endpoint)]).boxed()) }.boxed()
    }
}

struct Material {
    certificate: String,
    private_key: String,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn profile_identity_changes_reload_live_listener_in_place() -> TestResult {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let test_dir = TestDir::new()?;
    let profile = IdentityProfile::try_from(test_dir.path().join("reload-test"))?;
    let (identity_a, identity_b) = issue_identity_pair()?;
    profile
        .save_identity(
            identity_a.certificate.as_bytes(),
            identity_a.private_key.as_bytes(),
        )
        .await?;

    let port = UdpSocket::bind("127.0.0.1:0")?.local_addr()?.port();
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let server_config = parse_server_config(test_dir.path(), profile.clone(), port)?;

    let network = dhttp::h3x::dquic::Network::builder().build();
    let state = Arc::new(RootState::new(dhttp::network::DhttpNetwork::from(network)));
    let plane = Arc::new(InProcessControlPlane::new(state.clone()));
    let router_state = gateway::reverse::router::RouterState {
        #[cfg(feature = "sshd")]
        session_spawner: plane.clone(),
        #[cfg(feature = "sshd")]
        task_scope: Arc::new(state.local_task_scope()),
    };
    let (sources, context) =
        TypedServerSource::load_all([Arc::new(server_config)], router_state).await;
    assert_eq!(sources.len(), 1, "test server source should load");

    let mut runtime = RuntimeRegistry::new(plane);
    runtime.apply_sources(sources, &context).await;

    let long_lived = build_client(address).await?;
    assert_eq!(
        wait_for_sequence(address, 1, Duration::from_secs(5)).await?,
        1
    );
    assert_eq!(request_sequence(&long_lived).await?, 1);

    let result = {
        let exercise = exercise_rotation(&profile, &identity_a, &identity_b, address, &long_lived);
        let service_event = runtime.wait_service_completion();
        tokio::pin!(service_event);
        tokio::time::timeout(Duration::from_secs(12), async {
            tokio::select! {
                result = exercise => result,
                name = &mut service_event => Err(format!("server service exited unexpectedly: {name}").into()),
            }
        })
        .await
        .map_err(|_| "identity hot-reload test timed out")?
    };

    runtime.shutdown().await;
    state.cleanup_local_resources().await;
    result
}

async fn exercise_rotation(
    profile: &IdentityProfile,
    identity_a: &Material,
    identity_b: &Material,
    address: SocketAddr,
    long_lived: &Endpoint,
) -> TestResult {
    save_material(profile, identity_b).await?;
    assert_eq!(
        wait_for_sequence(address, 2, Duration::from_secs(2)).await?,
        2
    );
    assert_eq!(
        request_sequence(long_lived).await?,
        1,
        "connection established with A must remain usable with A after rotation"
    );

    save_material(profile, identity_a).await?;
    assert_eq!(
        wait_for_sequence(address, 1, Duration::from_secs(2)).await?,
        1
    );

    profile
        .save_identity(b"not a PEM certificate\n", b"not a PEM private key\n")
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let bad_window = Instant::now() + Duration::from_secs(1);
    let mut attempts = 0;
    while Instant::now() < bad_window {
        let client = build_client(address).await?;
        assert_eq!(
            request_sequence(&client).await?,
            1,
            "invalid PEM must retain the last good identity"
        );
        attempts += 1;
        tokio::time::sleep(Duration::from_millis(75)).await;
    }
    assert!(
        attempts >= 2,
        "bad PEM window should open multiple connections"
    );

    save_material(profile, identity_b).await?;
    assert_eq!(
        wait_for_sequence(address, 2, Duration::from_secs(2)).await?,
        2
    );
    Ok(())
}

fn issue_identity_pair() -> TestResult<(Material, Material)> {
    let ca_key = KeyPair::generate()?;
    let mut ca_params = CertificateParams::default();
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "pishoo identity reload test root");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca = CertifiedIssuer::self_signed(ca_params, ca_key)?;
    Ok((issue_identity(&ca, 1)?, issue_identity(&ca, 2)?))
}

fn issue_identity(issuer: &CertifiedIssuer<'_, KeyPair>, sequence: u32) -> TestResult<Material> {
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::new(vec![SERVER_NAME.to_owned()])?;
    params
        .distinguished_name
        .push(DnType::CommonName, SERVER_NAME);
    params.is_ca = IsCa::ExplicitNoCa;
    params.serial_number = Some(SerialNumber::from(sequence as u64));
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    params.key_identifier_method =
        KeyIdMethod::PreSpecified(format!("{sequence}:0:{OWNER_HASH}").into_bytes());
    let certificate = params.signed_by(&key, issuer)?;
    Ok(Material {
        certificate: format!("{}{}", certificate.pem(), issuer.pem()),
        private_key: key.serialize_pem(),
    })
}

fn parse_server_config(
    test_root: &Path,
    profile: IdentityProfile,
    port: u16,
) -> TestResult<gateway::parse::config::ServerConfig> {
    let root_config = gateway::parse::TypedConfigParser::new().parse_root(
        "pishoo {}",
        &test_root.join("pishoo.conf"),
        None,
    )?;
    let defaults = root_config.pishoo().worker_defaults();
    let text = format!(
        "server {{ listen internal v4only {port}; dns h3 https://127.0.0.1:4433; location / {{ return 204; }} }}"
    );
    let candidate = gateway::parse::TypedConfigParser::new().parse_identity(
        &text,
        &profile.join("server.conf"),
        profile,
        &defaults,
    )?;
    Ok(candidate.into_parts().1?)
}

async fn save_material(profile: &IdentityProfile, material: &Material) -> TestResult {
    profile
        .save_identity(
            material.certificate.as_bytes(),
            material.private_key.as_bytes(),
        )
        .await?;
    Ok(())
}

async fn build_client(address: SocketAddr) -> TestResult<Endpoint> {
    let bind = BindPattern::from_str("inet://127.0.0.1:0")?;
    let mut client_config = dhttp::trust::default_client_quic_config();
    client_config.verifier = ServerCertVerifierChoice::Dangerous;
    let network = dhttp::h3x::dquic::Network::builder().build();
    Ok(Endpoint::builder()
        .network(dhttp::network::DhttpNetwork::from(network))
        .client(client_config)
        .resolver(Arc::new(FixedResolver(address)))
        .bind(Arc::new(vec![bind]))
        .build()
        .await?)
}

async fn request_sequence(endpoint: &Endpoint) -> TestResult<u32> {
    let response = endpoint.get(format!("https://{SERVER_NAME}/")).await?;
    assert!(
        matches!(
            response.status(),
            http::StatusCode::NO_CONTENT | http::StatusCode::FORBIDDEN
        ),
        "unexpected response status: {}",
        response.status()
    );
    let identifier = response.authority().dhttp_subject_key_identifier()?;
    Ok(identifier.chain().sequence().get())
}

async fn wait_for_sequence(
    address: SocketAddr,
    expected: u32,
    timeout: Duration,
) -> TestResult<u32> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    while Instant::now() < deadline {
        let client = build_client(address).await?;
        match tokio::time::timeout(Duration::from_millis(500), request_sequence(&client)).await {
            Ok(Ok(sequence)) => {
                last = Some(sequence);
                if sequence == expected {
                    return Ok(sequence);
                }
            }
            Ok(Err(_)) | Err(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(
        format!("timed out waiting for certificate sequence {expected}; last observed {last:?}")
            .into(),
    )
}

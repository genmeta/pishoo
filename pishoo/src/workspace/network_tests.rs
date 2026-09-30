use std::{
    fmt,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use access_control::{
    AccessService, ContactPatch, ContactStatus, Effect, Grantee, Method as AccessMethod, SubjectId,
    Visitor,
};
use axum::{
    body::{Body, to_bytes},
    middleware::from_fn_with_state,
};
use bytes::Bytes;
use dhttp::{
    ddns::resolvers::DnsScheme,
    dquic::{
        client::ServerCertVerifierChoice,
        net::IO,
        qresolve::EndpointAddr,
        resolver::{Resolve, ResolveFuture, Source},
    },
    endpoint::{BuildEndpointError, Endpoint},
    h3x::{
        connection::ConnectionBuilder, dhttp::settings::Settings, endpoint::H3Endpoint,
        hyper::TowerService,
    },
    home::identity::IdentityProfile,
    identity::Identity,
};
use futures::{FutureExt, StreamExt, stream};
use gateway::{
    control_plane::{ConnectorRequest, ProvideConnector},
    reverse::body_adapter::BodyAdapterLayer,
};
use http::{Request, StatusCode};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyIdMethod, KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::CertificateDer;
use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use tower::{ServiceBuilder, ServiceExt};

use super::{
    WorkspaceState,
    outbound::{OutboundTransport, PlaneOutbound},
    router,
    store::WorkspaceStore,
};
use crate::{
    chat::{self, ChatState, store::ChatStore},
    service::daccess::{DaccessAuthState, authorize, profile_database_uri},
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct TestRoot(PathBuf);

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug)]
struct FixedResolver(SocketAddr);

impl fmt::Display for FixedResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("workspace test resolver")
    }
}

impl Resolve for FixedResolver {
    fn lookup<'a>(
        &'a self,
        _hostname: &'a str,
        _servname: &'a str,
        _family: Option<dhttp::dquic::qresolve::Family>,
    ) -> ResolveFuture<'a> {
        async move { Ok(stream::iter([(Source::System, EndpointAddr::direct(self.0))]).boxed()) }
            .boxed()
    }
}

struct LocalPlane(SocketAddr);

impl ProvideConnector for LocalPlane {
    type Connector = Arc<Endpoint>;
    type ConnectError = BuildEndpointError;

    async fn connector(
        &self,
        request: ConnectorRequest,
    ) -> Result<Self::Connector, Self::ConnectError> {
        let mut client = dhttp::trust::default_client_quic_config();
        // The listener uses an isolated test CA, never the production DHTTP root.
        client.verifier = ServerCertVerifierChoice::Dangerous;
        let endpoint = Endpoint::builder()
            .network(dhttp::network::DhttpNetwork::from(
                dhttp::h3x::dquic::Network::builder().build(),
            ))
            .maybe_identity(request.identity.map(Arc::new))
            .resolver(Arc::new(FixedResolver(self.0)))
            .client(client)
            .dns(DnsScheme::System)
            .build()
            .await?;
        Ok(Arc::new(endpoint))
    }
}

async fn profile_identity(
    root: &PathBuf,
    name: &str,
    issuer: &CertifiedIssuer<'_, KeyPair>,
    serial: u64,
) -> TestResult<(Identity, IdentityProfile)> {
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::new(vec![name.to_owned()])?;
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::ExplicitNoCa;
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    params.key_identifier_method =
        KeyIdMethod::PreSpecified(format!("{serial}:0:{serial:064x}").into_bytes());
    let certificate = params.signed_by(&key, issuer)?;
    let profile = IdentityProfile::try_from(root.join(name))?;
    profile
        .save_identity(
            format!("{}{}", certificate.pem(), issuer.pem()).as_bytes(),
            key.serialize_pem().as_bytes(),
        )
        .await?;
    Ok((profile.load_identity().await?, profile))
}

async fn local_app(
    identity: Identity,
    profile: &IdentityProfile,
    plane: Arc<LocalPlane>,
) -> TestResult<(axum::Router, Visitor)> {
    let subject_id = SubjectId::new(
        identity
            .dhttp_subject_key_identifier()?
            .owner_hash()
            .to_string()
            .into_bytes(),
    )
    .expect("valid certificate owner hash");
    let name = identity.name().as_full().to_owned();
    let store = WorkspaceStore::open(profile).await?;
    let db = Database::connect("sqlite::memory:").await?;
    let access = Arc::new(AccessService::new(db, &name));
    let state = Arc::new(WorkspaceState::new(
        name.clone(),
        name.clone(),
        subject_id.clone(),
        store,
        access.clone(),
    ));
    let chat_store = ChatStore::open(profile).await?;
    let chat_state = Arc::new(ChatState::new(
        name.clone(),
        subject_id.clone(),
        chat_store,
        access,
    ));
    state.configure_chat(chat_state.clone()).await;
    state
        .configure_outbound(Arc::new(PlaneOutbound::new(plane, identity)))
        .await;
    Ok((
        router(state).merge(chat::router(chat_state)),
        Visitor::new(name, subject_id),
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_profiles_send_over_real_dhttp_mtls_to_the_same_remote() -> TestResult {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let root = TestRoot(std::env::temp_dir().join(format!(
        "pishoo-workspace-mtls-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
    )));
    let ca_key = KeyPair::generate()?;
    let mut ca_params = CertificateParams::default();
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "workspace test CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let issuer = CertifiedIssuer::self_signed(ca_params, ca_key)?;
    let (server, server_profile) =
        profile_identity(&root.0, "receiver.dhttp.net", &issuer, 1).await?;
    let (alice, alice_profile) = profile_identity(&root.0, "alice.dhttp.net", &issuer, 2).await?;
    let (bob, bob_profile) = profile_identity(&root.0, "bob.dhttp.net", &issuer, 3).await?;

    let server_subject = SubjectId::new(
        server
            .dhttp_subject_key_identifier()?
            .owner_hash()
            .to_string()
            .into_bytes(),
    )
    .expect("valid certificate owner hash");
    let db_uri = profile_database_uri(&server_profile)?;
    let access = Arc::new(
        AccessService::load_from_db(&db_uri, "receiver.dhttp.net", &server_subject).await?,
    );
    access
        .set_policy(
            AccessMethod::Specified(http::Method::POST),
            "/contact",
            Effect::Allow,
            Grantee::All,
        )
        .await?;
    let server_store = WorkspaceStore::open(&server_profile).await?;
    server_store
        .db()
        .execute_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "UPDATE profile_preferences SET display_name = 'Receiver', updated_at = 1700000000 WHERE id = 1".to_owned(),
        ))
        .await?;
    let server_workspace = Arc::new(WorkspaceState::new(
        String::from("receiver.dhttp.net"),
        String::from("receiver.dhttp.net"),
        server_subject.clone(),
        server_store,
        access.clone(),
    ));
    let auth_state = DaccessAuthState::new(access.clone(), server_workspace.clone());
    let server_chat = Arc::new(ChatState::new(
        String::from("receiver.dhttp.net"),
        server_subject,
        ChatStore::open(&server_profile).await?,
        access.clone(),
    ));
    server_workspace.configure_chat(server_chat.clone()).await;
    server_chat.start_worker();
    let app = router(server_workspace.clone())
        .merge(chat::router(server_chat))
        .merge(access_control::management_router(access.clone()))
        .layer(from_fn_with_state(auth_state, authorize));
    let service = TowerService(ServiceBuilder::new().layer(BodyAdapterLayer).service(app));

    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(issuer.der().to_vec()))?;
    let mut config = dhttp::trust::default_server_quic_config();
    config.client_cert_verifier =
        rustls::server::WebPkiClientVerifier::builder(Arc::new(roots)).build()?;
    let network = dhttp::h3x::dquic::Network::builder().build();
    let listener = dhttp::h3x::dquic::QuicEndpoint::builder()
        .network(network.clone())
        .identity(Arc::new(server))
        .bind(Arc::new(vec!["127.0.0.1:0".parse()?]))
        .server(config)
        .build()
        .await;
    // Bind the test listener to the port advertised by the in-memory resolver.
    let actual = network
        .quic()
        .interfaces()
        .into_iter()
        .next()
        .ok_or("missing listener interface")?
        .borrow()
        .bound_addr()?
        .port();
    let plane = Arc::new(LocalPlane(SocketAddr::from(([127, 0, 0, 1], actual))));
    let mut endpoint = H3Endpoint::builder()
        .quic(listener)
        .builder(Arc::new(ConnectionBuilder::new(Arc::new(
            Settings::default(),
        ))))
        .build();
    let server_task = tokio::spawn(async move { endpoint.listen(service).await });
    let (alice_app, alice_visitor) =
        local_app(alice.clone(), &alice_profile, plane.clone()).await?;
    let (bob_app, bob_visitor) = local_app(bob.clone(), &bob_profile, plane.clone()).await?;

    for (app, visitor) in [
        (alice_app.clone(), alice_visitor.clone()),
        (bob_app.clone(), bob_visitor.clone()),
    ] {
        let mut request = Request::builder()
            .method(http::Method::POST)
            .uri("/workspace-api/contact-requests")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"target_name":"receiver.dhttp.net","description":"Hello","requested_capabilities":["chat"],"offered_capabilities":["chat"]}"#,
            ))?;
        request.extensions_mut().insert(visitor.clone());
        let response = app.clone().oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let created: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 16_384).await?)?;
        let id = created["id"]
            .as_i64()
            .ok_or("missing outbound request id")?;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let mut check = Request::builder()
                    .uri(format!("/workspace-api/contact-requests/{id}"))
                    .body(Body::empty()).expect("valid status request");
                check.extensions_mut().insert(visitor.clone());
                let response = app.clone().oneshot(check).await.expect("status response");
                let checked: serde_json::Value = serde_json::from_slice(
                    &to_bytes(response.into_body(), 16_384).await.expect("status body")
                ).expect("valid status JSON");
                if checked["status"] == "pending" { break; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }).await?;

        let mut profile = Request::builder()
            .uri("/workspace-api/profiles/receiver.dhttp.net")
            .body(Body::empty())?;
        profile.extensions_mut().insert(visitor.clone());
        let response = app.clone().oneshot(profile).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let public: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 16_384).await?)?;
        assert_eq!(public["display_name"], "Receiver");
    }
    // Daccess on the real listener accepted each distinct verified certificate.
    for visitor in [&alice_visitor, &bob_visitor] {
        let saved = access.find_contact_by_name(visitor.name()).await?;
        assert_eq!(&saved.subject_id, visitor.subject_id());
    }

    access
        .patch_contact(
            alice.name().as_full(),
            ContactPatch::LocalUpdate {
                status: Some(ContactStatus::Active),
                alias: None,
            },
        )
        .await?;
    // A name-based daccess rule alone is not a current Chat capability grant.
    access
        .set_policy(
            AccessMethod::Specified(http::Method::POST),
            "/std/message",
            Effect::Allow,
            Grantee::One(alice.name().as_full().to_owned()),
        )
        .await?;
    let alice_transport = PlaneOutbound::new(plane.clone(), alice);
    let body = Bytes::from_static(
        br#"{"client_message_id":"network-message-1","text":"hello over dhttp"}"#,
    );
    let denied = alice_transport
        .request(
            "receiver.dhttp.net",
            http::Method::POST,
            "/std/message",
            body.clone(),
        )
        .await
        .map_err(|error| error.to_owned())?;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    super::contacts::grant_capability(&server_workspace, alice_visitor.name(), "chat", None)
        .await
        .expect("grant Chat to verified contact");
    let sent = alice_transport
        .request(
            "receiver.dhttp.net",
            http::Method::POST,
            "/std/message",
            body,
        )
        .await
        .map_err(|error| error.to_owned())?;
    assert_eq!(sent.status, StatusCode::OK);
    let sent: serde_json::Value = serde_json::from_slice(&sent.body)?;
    assert_eq!(sent["sender"], alice_visitor.name());
    assert_eq!(sent["recipient"], "receiver.dhttp.net");
    assert!(
        !server_task.is_finished(),
        "remote listener exited unexpectedly"
    );
    server_task.abort();
    Ok(())
}

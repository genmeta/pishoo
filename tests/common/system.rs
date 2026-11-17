//! Fixtures shared by device administration regression tests.
use super::super::{open, storage};
use access_control::{SubjectId, Visitor};
use axum::body::Body;
use base64::Engine;
use dhttp_home::DhttpHome;
use http::{Method, Request};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use tokio::sync::Mutex;

pub(super) fn visitor(name: &str, owner: u8) -> Visitor {
    Visitor::new(
        storage::canonical(name).unwrap(),
        SubjectId::new(vec![owner; 64]).unwrap(),
    )
}

pub(super) fn database() -> (tempfile::TempDir, DhttpHome, Arc<Mutex<Connection>>) {
    let root = tempfile::tempdir().unwrap();
    let home = DhttpHome::new(root.path().into());
    let db = open(&home).unwrap();
    (root, home, db)
}

pub(super) fn request(
    endpoint: &dhttp::Endpoint,
    method: Method,
    path: &str,
    body: Body,
    anonymous: bool,
) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .body(body)
        .unwrap();
    if anonymous {
        request.extensions_mut().insert(dhttp::HandshakeSummary {
            alpn: None,
            local: Some(endpoint.local_authority().unwrap()),
            remote: None,
        });
    } else {
        request
            .extensions_mut()
            .insert(endpoint.local_authority().unwrap());
    }
    request
}

pub(super) fn imported_body(root: &Path, name: &str) -> Value {
    static ROOTS: std::sync::Once = std::sync::Once::new();
    ROOTS.call_once(|| {
        qtls::RootCerts::set([qtls::CertificateDer::from(dhttp::trust::DHTTP_ROOT_CA_DER)]).unwrap()
    });
    json!({
        "name":name,
        "certificate":format!("{}{}", std::fs::read_to_string(root.join(name).join("ssl/fullchain.crt")).unwrap(), std::fs::read_to_string(root.join("ca.crt")).unwrap()),
        "private_key":std::fs::read_to_string(root.join(name).join("ssl/privkey.pem")).unwrap(),
        "ocsp":base64::engine::general_purpose::STANDARD.encode(std::fs::read(root.join(name).join("ssl/stapling.ocsp")).unwrap()),
    })
}

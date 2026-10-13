//! Pinned online publishing proof and per-request trusted Lib installation.

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use dhttp_home::identity::IdentityProfile;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{WasmRuntime, install_lib, validate_lib};
use crate::{Error, Result};

const STORE_NAME: &str = "store.dhttp.net";
// Independent store-server/DEPLOYMENT.md, verified on 2026-10-13.
const STORE_OWNER: &str = "ffc3e50e1dfb7baca1cc07a7cbfcf8cb34a9bf5a9c25024fcf9a9786bb230746";

pub(crate) async fn install_store_lib(
    profile: &IdentityProfile,
    endpoint: &dhttp::Endpoint,
    publisher: &str,
    name: &str,
    version: &str,
    runtime: &WasmRuntime,
) -> Result<Value> {
    let id = format!("{publisher}/{name}");
    super::valid_id(&id)?;
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.parse::<u64>().is_err()
                || part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || part.len() > 1 && part.starts_with('0')
        })
    {
        return Err(Error::BadRequest(
            "version must be MAJOR.MINOR.PATCH".into(),
        ));
    }
    let path = format!("/store/v1/libs/{publisher}/{name}/versions/{version}");
    let metadata: Value =
        serde_json::from_slice(&store_get(endpoint, &path, 2 * 1024 * 1024).await?)
            .map_err(|_| Error::InvalidComponent("invalid store metadata".into()))?;
    let download = format!("{path}/lib.wasm");
    if metadata["id"] != id
        || metadata["publisher"] != publisher
        || metadata["name"] != name
        || metadata["version"] != version
        || metadata["status"] != "published"
        || metadata["download_path"] != download
    {
        return Err(Error::InvalidComponent(
            "store release identity, status or download path changed".into(),
        ));
    }
    let size = metadata["size"]
        .as_u64()
        .filter(|size| *size > 0 && *size <= 64 * 1024 * 1024)
        .ok_or_else(|| Error::InvalidComponent("invalid store package size".into()))?;
    let bytes = store_get(endpoint, &download, size as usize).await?;
    verify_release(&bytes, &metadata, publisher, name, version)?;
    // Save via the existing synchronous validator/compiler and atomic commit.
    // The operation stays in this request; disconnect does not imply rollback.
    tokio::task::block_in_place(|| install_lib(profile, &id, &bytes, runtime))
}

async fn store_get(endpoint: &dhttp::Endpoint, path: &str, limit: usize) -> Result<Vec<u8>> {
    tokio::time::timeout(Duration::from_secs(120), async {
        let uri = format!("https://{STORE_NAME}{path}")
            .parse()
            .map_err(|_| Error::BadRequest("invalid store URI".into()))?;
        let owner = dhttp_home::certificate::OwnerHash::try_from(STORE_OWNER)
            .map_err(|_| Error::InvalidIdentity("invalid pinned store identity".into()))?;
        let content_type = if path.ends_with("/lib.wasm") {
            "application/wasm"
        } else {
            "application/json"
        };
        let response = endpoint
            .get(uri)
            .expect_remote_owner_hash(owner)
            .header(
                http::header::ACCEPT,
                http::HeaderValue::from_static(content_type),
            )
            .await?;
        let remote = response
            .extensions()
            .get::<dhttp::RemoteAuthority>()
            .ok_or(Error::MissingHandshake)?;
        let ski =
            dhttp_home::certificate::extract_dhttp_subject_key_identifier(remote.certificates())
                .map_err(|_| Error::InvalidIdentity("invalid store identity".into()))?;
        if remote.name() != STORE_NAME || ski.owner_hash().as_str() != STORE_OWNER {
            return Err(Error::InvalidIdentity(
                "store identity does not match the pinned owner".into(),
            ));
        }
        match response.status() {
            http::StatusCode::OK => {}
            http::StatusCode::NOT_FOUND => return Err(Error::RouteNotFound),
            http::StatusCode::GONE => {
                return Err(Error::BadRequest("store version has been withdrawn".into()));
            }
            _ => {
                return Err(Error::BadRequest(format!(
                    "store request failed ({})",
                    response.status()
                )));
            }
        }
        if !response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case(content_type))
        {
            return Err(Error::InvalidComponent(
                "unexpected store response content type".into(),
            ));
        }
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        use http_body_util::BodyExt;
        while let Some(frame) = body.frame().await {
            let data = frame
                .map_err(|error| Error::Io(std::io::Error::other(error)))?
                .into_data()
                .map_err(|_| Error::BadRequest("store response trailers are unsupported".into()))?;
            if data.len() > limit.saturating_sub(bytes.len()) {
                return Err(Error::InvalidComponent(
                    "store response exceeds its size limit".into(),
                ));
            }
            bytes.extend_from_slice(&data);
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| Error::Deadline)?
}

fn verify_release(
    bytes: &[u8],
    metadata: &Value,
    publisher: &str,
    name: &str,
    version: &str,
) -> Result<()> {
    let invalid = |message: &str| Error::InvalidComponent(message.into());
    let digest = format!("{:x}", Sha256::digest(bytes));
    if metadata["package_sha256"] != digest || metadata["size"].as_u64() != Some(bytes.len() as u64)
    {
        return Err(invalid("store package digest or size mismatch"));
    }
    // validate_lib verifies the signature bytes, position and signer binding.
    validate_lib(bytes)?;
    let mut depth = 0;
    let mut envelope = None;
    let mut document = None;
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        match payload.map_err(|_| invalid("malformed store component"))? {
            wasmparser::Payload::ModuleSection { .. }
            | wasmparser::Payload::ComponentSection { .. } => depth += 1,
            wasmparser::Payload::End(_) if depth > 0 => depth -= 1,
            wasmparser::Payload::CustomSection(section) if depth == 0 => {
                if section.name() == "pishoo:signature" {
                    envelope = Some(
                        serde_json::from_slice::<Value>(section.data())
                            .map_err(|_| invalid("invalid signature envelope"))?,
                    );
                } else if section.name() == "pishoo:openapi" {
                    document = Some(
                        serde_json::from_slice::<Value>(section.data())
                            .map_err(|_| invalid("invalid store manifest"))?,
                    );
                }
            }
            _ => {}
        }
    }
    let envelope = envelope.ok_or_else(|| invalid("store component is unsigned"))?;
    let document = document.ok_or_else(|| invalid("missing store manifest"))?;
    if document != metadata["openapi"]
        || document["x-pishoo"]["publisher"] != publisher
        || document["x-pishoo"]["name"] != name
        || document["info"]["version"] != version
        || document["x-pishoo"].get("templates").is_some()
    {
        return Err(invalid("store manifest or release identity mismatch"));
    }
    if envelope["certificates"] != metadata["signer_certificates"] {
        return Err(invalid(
            "signer certificates differ from the authenticated store proof",
        ));
    }
    let certificates = envelope["certificates"]
        .as_array()
        .ok_or_else(|| invalid("missing signer certificates"))?
        .iter()
        .map(|certificate| {
            STANDARD
                .decode(
                    certificate
                        .as_str()
                        .ok_or_else(|| invalid("invalid signer certificate"))?,
                )
                .map(qtls::CertificateDer::from)
                .map_err(|_| invalid("invalid signer certificate encoding"))
        })
        .collect::<Result<Vec<_>>>()?;
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(&certificates)
        .map_err(|_| invalid("invalid publisher owner_hash"))?;
    if metadata["publisher_owner_hash"] != ski.owner_hash().as_str() {
        return Err(invalid(
            "publisher owner_hash differs from the authenticated store proof",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/sandbox/store.rs"]
mod tests;

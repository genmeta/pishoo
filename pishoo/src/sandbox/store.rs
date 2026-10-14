//! Pinned online publishing proof and per-request trusted Lib installation.

use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use dhttp_home::identity::IdentityProfile;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Lib, Sandbox, install_app, install_lib, package_file, package_metadata, read_package};
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
    sandbox: &Arc<RwLock<Sandbox>>,
) -> Result<Value> {
    install_store_artifact(profile, endpoint, "lib", publisher, name, version, sandbox).await
}

async fn download_release(
    endpoint: &dhttp::Endpoint,
    publisher: &str,
    name: &str,
    version: &str,
    kind: &str,
    cached: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
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
    let collection = if kind == "app" { "apps" } else { "libs" };
    let path = format!("/store/v1/{collection}/{publisher}/{name}/versions/{version}");
    let metadata: Value =
        serde_json::from_slice(&store_get(endpoint, &path, 2 * 1024 * 1024).await?)
            .map_err(|_| Error::InvalidComponent("invalid store metadata".into()))?;
    let download = format!("{path}/{kind}.webc");
    if metadata["id"] != id
        || metadata["publisher"] != publisher
        || metadata["name"] != name
        || metadata["version"] != version
        || metadata["status"] != "published"
        || metadata["download_path"] != download
        || metadata
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("lib")
            != kind
    {
        return Err(Error::InvalidComponent(
            "store release identity, status or download path changed".into(),
        ));
    }
    let size = metadata["size"]
        .as_u64()
        .filter(|size| *size > 0 && *size <= 64 * 1024 * 1024)
        .ok_or_else(|| Error::InvalidComponent("invalid store package size".into()))?;
    let bytes = match cached {
        Some(bytes) => bytes,
        None => store_get(endpoint, &download, size as usize).await?,
    };
    verify_release(&bytes, &metadata, publisher, name, version)?;
    Ok(bytes)
}

pub(crate) async fn install_store_app(
    profile: &IdentityProfile,
    endpoint: &dhttp::Endpoint,
    publisher: &str,
    name: &str,
    version: &str,
    sandbox: &Arc<RwLock<Sandbox>>,
) -> Result<Value> {
    install_store_artifact(profile, endpoint, "app", publisher, name, version, sandbox).await
}

// Fetch each release proof once, then prepare and install dependencies first.
async fn install_store_artifact(
    profile: &IdentityProfile,
    endpoint: &dhttp::Endpoint,
    kind: &str,
    publisher: &str,
    name: &str,
    version: &str,
    sandbox: &Arc<RwLock<Sandbox>>,
) -> Result<Value> {
    let root_id = format!("{publisher}/{name}");
    let mut pending = vec![(
        kind.to_owned(),
        publisher.to_owned(),
        name.to_owned(),
        version.to_owned(),
    )];
    let mut resources: Vec<(String, String, Vec<u8>, bool)> = Vec::new();
    while let Some((selected, publisher, name, version)) = pending.pop() {
        let id = format!("{publisher}/{name}");
        if let Some((_, _, bytes, _)) = resources
            .iter()
            .find(|(kind, key, _, _)| kind == &selected && key == &id)
        {
            let package = read_package(bytes)?;
            let document = package_metadata(&package)?;
            if document["version"] != version {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("conflicting dependency versions for {id}"),
                )));
            }
            continue;
        }
        if selected == "lib"
            && resources
                .iter()
                .filter(|(kind, _, _, _)| kind == "lib")
                .count()
                >= 100
        {
            return Err(Error::BadRequest(
                "installation requires more than 100 backend Libs".into(),
            ));
        }
        let current = if selected == "lib" && !(kind == "lib" && id == root_id) {
            sandbox
                .read()
                .unwrap()
                .libs
                .get(&id)
                .map(|lib| lib.openapi.info.version.clone())
        } else {
            None
        };
        if current
            .as_deref()
            .is_some_and(|installed| installed != version)
        {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("dependency {id} requires {version}; an incompatible version is installed"),
            )));
        }
        let cached = if current.is_some() {
            Some(super::disk_bytes(&super::lib_root(profile)?, &id)?)
        } else {
            None
        };
        let bytes =
            download_release(endpoint, &publisher, &name, &version, &selected, cached).await?;
        let package = read_package(&bytes)?;
        let document = package_metadata(&package)?;
        for dependency in document["dependencies"].as_array().into_iter().flatten() {
            pending.push((
                "lib".into(),
                dependency["publisher"].as_str().unwrap().to_owned(),
                dependency["name"].as_str().unwrap().to_owned(),
                dependency["version"].as_str().unwrap().to_owned(),
            ));
        }
        resources.push((selected, id, bytes, current.is_some()));
    }
    let resources = order_releases(resources)?;
    tokio::task::block_in_place(|| {
        let runtime = sandbox.read().unwrap().runtime.clone();
        let mut candidates = Vec::new();
        for (kind, id, bytes, reused) in &resources {
            if kind == "lib" && !reused {
                candidates.push((
                    id.clone(),
                    Arc::new(Lib::load(
                        runtime.clone(),
                        id.clone(),
                        bytes,
                        &profile.join("db").join(id),
                    )?),
                ));
            }
        }
        let mut sandbox = sandbox.write().unwrap();
        if sandbox.tasks.is_closed() {
            return Err(Error::Denied);
        }
        for (kind, id, bytes, reused) in &resources {
            if kind != "lib" || !reused {
                continue;
            }
            let expected = read_package(bytes)?;
            if sandbox.libs.get(id).is_none_or(|current| {
                current.package.get_atom("backend") != expected.get_atom("backend")
                    || package_file(&current.package, "/metadata/openapi.json").ok()
                        != package_file(&expected, "/metadata/openapi.json").ok()
                    || current
                        .package
                        .get_volume("pishoo-signature")
                        .unwrap()
                        .read_file("/signature.json")
                        .unwrap()
                        .0
                        != expected
                            .get_volume("pishoo-signature")
                            .unwrap()
                            .read_file("/signature.json")
                            .unwrap()
                            .0
                    || package_metadata(&current.package).ok() != package_metadata(&expected).ok()
            }) {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("dependency {id} changed during installation; retry"),
                )));
            }
        }
        let mut result = Value::Null;
        for (selected, id, bytes, reused) in resources {
            let value = if selected == "lib" {
                if reused {
                    super::installed_libs(profile, Some(&id))?
                } else {
                    let value = install_lib(profile, &id, &bytes, &runtime)?;
                    sandbox.libs.insert(
                        id.clone(),
                        candidates
                            .iter()
                            .find(|(key, _)| key == &id)
                            .unwrap()
                            .1
                            .clone(),
                    );
                    value
                }
            } else {
                let package = read_package(&bytes)?;
                let value = install_app(profile, &id, &bytes)?;
                sandbox.apps.insert(id.clone(), package);
                value
            };
            if selected == kind && id == root_id {
                result = value;
            }
        }
        Ok(result)
    })
}

fn order_releases(
    mut pending: Vec<(String, String, Vec<u8>, bool)>,
) -> Result<Vec<(String, String, Vec<u8>, bool)>> {
    let mut ordered: Vec<(String, String, Vec<u8>, bool)> = Vec::new();
    while !pending.is_empty() {
        let mut ready = None;
        for (index, (_, _, bytes, _)) in pending.iter().enumerate() {
            let package = read_package(bytes)?;
            let document = package_metadata(&package)?;
            if document["dependencies"]
                .as_array()
                .into_iter()
                .flatten()
                .all(|dependency| {
                    let id = format!(
                        "{}/{}",
                        dependency["publisher"].as_str().unwrap(),
                        dependency["name"].as_str().unwrap()
                    );
                    ordered
                        .iter()
                        .any(|(kind, key, _, _)| kind == "lib" && key == &id)
                })
            {
                ready = Some(index);
                break;
            }
        }
        let index = ready.ok_or_else(|| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cyclic or incomplete backend dependency graph",
            ))
        })?;
        ordered.push(pending.remove(index));
    }
    Ok(ordered)
}

async fn store_get(endpoint: &dhttp::Endpoint, path: &str, limit: usize) -> Result<Vec<u8>> {
    tokio::time::timeout(Duration::from_secs(120), async {
        let uri = format!("https://{STORE_NAME}{path}")
            .parse()
            .map_err(|_| Error::BadRequest("invalid store URI".into()))?;
        let owner = dhttp_home::certificate::OwnerHash::try_from(STORE_OWNER)
            .map_err(|_| Error::InvalidIdentity("invalid pinned store identity".into()))?;
        let content_type = if path.ends_with(".webc") {
            "application/webc"
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
    // Validate the entire signed container, not only the backend atom.
    let package = read_package(bytes)?;
    let document = package_metadata(&package)?;
    let (data, _) = package
        .get_volume("pishoo-signature")
        .unwrap()
        .read_file("/signature.json")
        .unwrap();
    let envelope: Value =
        serde_json::from_slice(&data).map_err(|_| invalid("invalid signature envelope"))?;
    if document["publisher"] != publisher
        || document["name"] != name
        || document["version"] != version
        || ["kind", "dependencies", "title", "description", "changelog"]
            .iter()
            .any(|key| metadata[*key] != document[*key])
        || metadata.get("host").filter(|v| !v.is_null()) != document.get("host")
        || metadata
            .get("entry")
            .is_some_and(|entry| Some(entry) != document.get("entry"))
        || metadata
            .get("license")
            .is_some_and(|license| Some(license) != document.get("license"))
    {
        return Err(invalid(
            "store metadata differs from signed package manifest",
        ));
    }
    if document["kind"] == "lib" {
        let api: Value = serde_json::from_slice(&package_file(&package, "/metadata/openapi.json")?)
            .map_err(|_| invalid("invalid package OpenAPI"))?;
        if metadata["openapi"] != api {
            return Err(invalid("store OpenAPI differs from signed package"));
        }
    } else if metadata.get("openapi").is_some() {
        return Err(invalid("App proof must not contain OpenAPI"));
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

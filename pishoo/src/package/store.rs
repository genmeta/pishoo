//! Pinned online publishing proof and per-request trusted Lib installation.

use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use dhttp_home::identity::IdentityProfile;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Sandbox, install_package, installed_packages, package_metadata, read_package};
#[cfg(test)]
use super::{install_lib, package_file};
use crate::{Error, Result, sandbox::Lib};

pub(crate) const STORE_NAME: &str = "store.dhttp.net";
// Independent store-server/DEPLOYMENT.md, verified on 2026-10-13.
pub(crate) const STORE_OWNER: &str =
    "ffc3e50e1dfb7baca1cc07a7cbfcf8cb34a9bf5a9c25024fcf9a9786bb230746";

async fn download_release(
    endpoint: &dhttp::Endpoint,
    publisher: &str,
    name: &str,
    version: &str,
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
    let path = format!("/store/v1/packages/{publisher}/{name}/versions/{version}");
    let metadata: Value =
        serde_json::from_slice(&store_get(endpoint, &path, 2 * 1024 * 1024).await?)
            .map_err(|_| Error::InvalidComponent("invalid store metadata".into()))?;
    let download = format!("{path}/package.waz");
    if metadata["id"] != id
        || metadata["author"] != publisher
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
    // Installed snapshots deliberately omit the original release proof. Fetch
    // signed source bytes so the authenticated store digest can still be checked.
    let bytes = store_get(endpoint, &download, size as usize).await?;
    verify_release(&bytes, &metadata, publisher, name, version)?;
    Ok(bytes)
}

pub(crate) async fn install_store_package(
    profile: &IdentityProfile,
    endpoint: &dhttp::Endpoint,
    publisher: &str,
    name: &str,
    version: &str,
    sandbox: &Arc<RwLock<Sandbox>>,
) -> Result<Value> {
    let root_id = format!("{publisher}/{name}");
    super::valid_id(&root_id)?;
    let installed = installed_packages(profile, None)?;
    let mut pending = vec![(publisher.to_owned(), name.to_owned(), version.to_owned())];
    let mut resources: Vec<(String, Vec<u8>)> = Vec::new();
    while let Some((publisher, name, version)) = pending.pop() {
        let id = format!("{publisher}/{name}");
        if let Some((_, bytes)) = resources.iter().find(|(key, _)| key == &id) {
            if package_metadata(&read_package(bytes)?)?["version"] != version {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("conflicting dependency versions for {id}"),
                )));
            }
            continue;
        }
        if resources.len() >= 100 {
            return Err(Error::BadRequest(
                "installation exceeds 100 packages".into(),
            ));
        }
        if resources
            .iter()
            .any(|(other, _)| other != &id && other.rsplit('/').next() == Some(name.as_str()))
        {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "conflicting package authors",
            )));
        }
        if installed
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == name && entry["id"] != id)
        {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("package {name} is already installed by another author"),
            )));
        }
        let bytes = download_release(endpoint, &publisher, &name, &version).await?;
        let document = package_metadata(&read_package(&bytes)?)?;
        for dep in document["dependencies"].as_array().unwrap() {
            pending.push((
                dep["publisher"].as_str().unwrap().to_owned(),
                dep["name"].as_str().unwrap().to_owned(),
                dep["version"].as_str().unwrap().to_owned(),
            ));
        }
        resources.push((id, bytes));
    }
    let ordered = order_packages(resources)?;
    tokio::task::block_in_place(|| {
        let mut sandbox = sandbox.write().unwrap();
        if sandbox.tasks.is_closed() {
            return Err(Error::Denied);
        }
        let runtime = sandbox.runtime.clone();
        let mut result = Value::Null;
        for (id, bytes) in ordered {
            let metadata = package_metadata(&read_package(&bytes)?)?;
            let current = match installed_packages(profile, Some(&id)) {
                Ok(value) => Some(value),
                Err(Error::RouteNotFound) => None,
                Err(e) => return Err(e),
            };
            let reused = id != root_id
                && current.as_ref().is_some_and(|value| {
                    value["version"] == metadata["version"]
                        && value["has_lib"] == metadata["has_lib"]
                        && value["has_app"] == metadata["has_app"]
                })
                && (metadata["has_lib"] != true
                    || sandbox.libs.get(&id).is_some_and(|lib| {
                        Some(lib.openapi.info.version.as_str()) == metadata["version"].as_str()
                    }))
                && (metadata["has_app"] != true
                    || sandbox.apps.get(&id).is_some_and(|app| {
                        package_metadata(app)
                            .is_ok_and(|value| value["version"] == metadata["version"])
                    }));
            if id != root_id
                && current
                    .as_ref()
                    .is_some_and(|v| v["version"] != metadata["version"])
            {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("incompatible installed dependency {id}"),
                )));
            }
            if !reused {
                let value = install_package(profile, &id, &bytes, &runtime)?;
                // Prepare both runtime projections, then replace them while the
                // existing Sandbox write lock excludes new request snapshots.
                let lib = if metadata["has_lib"] == true {
                    let package = super::disk_bytes(profile, &super::lib_root(profile)?, &id)?;
                    Some(Arc::new(Lib::load(
                        runtime.clone(),
                        id.clone(),
                        &package,
                        &profile
                            .join(".volume")
                            .join(if id.starts_with("std.dhttp.net/") {
                                format!("std/{}", id.rsplit('/').next().unwrap())
                            } else {
                                id.rsplit('/').next().unwrap().to_owned()
                            }),
                    )?))
                } else {
                    None
                };
                let app = if metadata["has_app"] == true {
                    super::installed_apps(profile, Some(&id))?.remove(&id)
                } else {
                    None
                };
                sandbox.libs.remove(&id);
                sandbox.apps.remove(&id);
                if let Some(lib) = lib {
                    sandbox.libs.insert(id.clone(), lib);
                }
                if let Some(app) = app {
                    sandbox.apps.insert(id.clone(), app);
                }
                if id == root_id {
                    result = value;
                }
            }
        }
        Ok(result)
    })
}

fn order_packages(mut resources: Vec<(String, Vec<u8>)>) -> Result<Vec<(String, Vec<u8>)>> {
    let mut ordered: Vec<(String, Vec<u8>)> = Vec::new();
    while !resources.is_empty() {
        let mut ready = None;
        for (index, (_, bytes)) in resources.iter().enumerate() {
            let doc = package_metadata(&read_package(bytes)?)?;
            if doc["dependencies"].as_array().unwrap().iter().all(|dep| {
                ordered.iter().any(|(id, _)| {
                    id == &format!(
                        "{}/{}",
                        dep["publisher"].as_str().unwrap(),
                        dep["name"].as_str().unwrap()
                    )
                })
            }) {
                ready = Some(index);
                break;
            }
        }
        let index = ready.ok_or_else(|| Error::BadRequest("cyclic package dependencies".into()))?;
        ordered.push(resources.remove(index));
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
        let content_type = if path.ends_with(".waz") {
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
    let dependencies = document["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|dependency| serde_json::json!({
            "author":dependency["publisher"],"name":dependency["name"],"version":dependency["version"]
        }))
        .collect::<Vec<_>>();
    if document["publisher"] != publisher
        || metadata["author"] != publisher
        || document["name"] != name
        || document["version"] != version
        || metadata["dependencies"] != serde_json::json!(dependencies)
        || ["title", "description", "changelog"]
            .iter()
            .any(|key| metadata[*key] != document[*key])
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
    if metadata["author_owner_hash"] != ski.owner_hash().as_str() {
        return Err(invalid(
            "publisher owner_hash differs from the authenticated store proof",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/package/store.rs"]
mod tests;

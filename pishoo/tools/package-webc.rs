//! Build a signed Pishoo WEBC release from independent backend/frontend inputs.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};
use webc::v3::{
    SignatureAlgorithm, Timestamps,
    write::{DirEntry, Directory, FileEntry, Writer},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.len() != 6 || args[4] != "--publisher" {
        return Err(
            "usage: package-webc BACKEND.wasm OPENAPI.json PUBLIC_DIR OUTPUT.webc --publisher NAME"
                .into(),
        );
    }
    let backend = std::fs::read(&args[0])?;
    if backend.len() > 64 * 1024 * 1024 {
        return Err("backend exceeds 64 MiB".into());
    }
    for payload in wasmparser::Parser::new(0).parse_all(&backend) {
        if let wasmparser::Payload::CustomSection(section) = payload?
            && matches!(section.name(), "pishoo:openapi" | "pishoo:signature")
        {
            return Err("use an unsigned backend without embedded OpenAPI".into());
        }
    }
    let runtime = pishoo::WasmRuntime::new()?;
    runtime.compile(&backend)?;
    let raw = std::fs::read(&args[1])?;
    pishoo::validate_openapi(&raw)?;
    let mut document: Value = serde_json::from_slice(&raw)?;
    let endpoint = dhttp::Endpoint::load(args[5].to_str().ok_or("invalid publisher name")?).await?;
    let authority = endpoint.local_authority()?;
    if document["x-pishoo"]["publisher"] != endpoint.name() {
        return Err("OpenAPI publisher differs from signing identity".into());
    }
    let name = document["x-pishoo"]["name"]
        .as_str()
        .ok_or("missing Lib name")?
        .to_owned();
    let version = document["info"]["version"]
        .as_str()
        .ok_or("missing version")?
        .to_owned();
    let project = Path::new(&args[1])
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    for (filename, expected_name) in [
        ("Cargo.toml", name.clone()),
        ("wasmer.toml", format!("{}/{}", endpoint.name(), name)),
    ] {
        let configuration: toml::Value =
            toml::from_str(&std::fs::read_to_string(project.join(filename))?)?;
        let package = &configuration["package"];
        if package["name"].as_str() != Some(&expected_name)
            || package["version"].as_str() != Some(&version)
        {
            return Err(format!("{filename} name/version differs from OpenAPI").into());
        }
    }
    let mut count = 0;
    let public = public_directory(Path::new(&args[2]), "", &mut document, &mut count, 0)?;
    let manifest: webc::metadata::Manifest = serde_json::from_value(json!({
        "package":{"wapm":{"name":format!("{}/{}",endpoint.name(),name),"version":version},
            "fs":[{"volume_name":"public","mount_path":"/public"},{"volume_name":"metadata","mount_path":"/metadata"}]},
        "atoms":{"backend":{"kind":"https://webc.org/kind/wasm","signature":format!("sha256:{}",STANDARD.encode(Sha256::digest(&backend)))}}
    }))?;
    let mut writer = Writer::default()
        .write_manifest(&manifest)?
        .write_atoms(BTreeMap::from([(
            "backend".parse()?,
            FileEntry::borrowed(&backend, Timestamps::default()),
        )]))?
        .with_volume("public", public)?
        .with_volume(
            "metadata",
            Directory::new(
                BTreeMap::from([(
                    "openapi.json".parse()?,
                    serde_json::to_vec(&document)?.into(),
                )]),
                Timestamps::default(),
            ),
        )?;
    let unsigned = writer.finish(SignatureAlgorithm::None)?;
    let container = webc::Container::from_bytes_and_version(unsigned.clone(), webc::Version::V3)?;
    let reader = webc::v3::read::OwnedReader::parse(unsigned.clone())?;
    let index = reader.index();
    let mut hash = Sha256::new();
    hash.update(b"\0webc003");
    for entry in std::iter::once(&index.manifest)
        .chain(std::iter::once(&index.atoms))
        .chain(index.volumes.values())
    {
        hash.update(&unsigned[entry.span.start..entry.span.end()]);
    }
    let mut input = b"pishoo-webc-signature-v1\0".to_vec();
    input.extend_from_slice(&hash.finalize());
    let mut signature = None;
    for scheme in [
        qtls::SignatureScheme::RSA_PSS_SHA512,
        qtls::SignatureScheme::ECDSA_NISTP256_SHA256,
        qtls::SignatureScheme::ECDSA_NISTP384_SHA384,
        qtls::SignatureScheme::ED25519,
    ] {
        match authority.sign(scheme, &input) {
            Ok(bytes) => {
                signature = Some(bytes);
                break;
            }
            Err(qtls::SignError::UnsupportedScheme { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let signature = signature.ok_or("unsupported signing key")?;
    let envelope = json!({"version":1,"certificates":authority.certificates().iter().map(|c|STANDARD.encode(c.as_ref())).collect::<Vec<_>>(),"signature":STANDARD.encode(signature)});
    writer = Writer::default()
        .write_raw_manifest(
            unsigned[index.manifest.span.start + 41..index.manifest.span.end()].to_vec(),
        )
        .write_atoms(BTreeMap::from([(
            "backend".parse()?,
            FileEntry::borrowed(&backend, Timestamps::default()),
        )]))?;
    for (name, volume) in container.volumes() {
        writer.write_volume(&name, copy_directory(&volume, "/")?)?;
    }
    let bytes = writer
        .with_volume(
            "pishoo-signature",
            Directory::new(
                BTreeMap::from([(
                    "signature.json".parse()?,
                    serde_json::to_vec(&envelope)?.into(),
                )]),
                Timestamps::default(),
            ),
        )?
        .finish(SignatureAlgorithm::None)?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err("package exceeds 64 MiB".into());
    }
    // The actual host validator checks signature, files and route completeness
    // before the requested output is published. Private credentials stay local.
    let parent = Path::new(&args[3])
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    pishoo::run_command(vec!["lib".into(), "check".into(), temporary.path().into()]).await?;
    let metadata = project.join("metadata");
    std::fs::create_dir_all(&metadata)?;
    let mut manifest_file = tempfile::NamedTempFile::new_in(&metadata)?;
    manifest_file.write_all(&serde_json::to_vec_pretty(&document)?)?;
    manifest_file.as_file().sync_all()?;
    manifest_file.persist(metadata.join("openapi.json"))?;
    temporary.persist(&args[3])?;
    eprintln!(
        "Saved {} ({} bytes)",
        Path::new(&args[3]).display(),
        bytes.len()
    );
    Ok(())
}

fn public_directory(
    path: &Path,
    relative: &str,
    document: &mut Value,
    count: &mut usize,
    depth: usize,
) -> Result<Directory<'static>> {
    if depth > 32 || std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err("public must contain real directories, depth <= 32".into());
    }
    let mut children = BTreeMap::new();
    let mut entries = std::fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        *count += 1;
        if *count > 4000 {
            return Err("too many public entries".into());
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "invalid public filename")?;
        let logical = format!("{relative}/{name}");
        let kind = entry.file_type()?;
        let item = if kind.is_dir() {
            DirEntry::Dir(public_directory(
                &entry.path(),
                &logical,
                document,
                count,
                depth + 1,
            )?)
        } else if kind.is_file() {
            if entry.metadata()?.len() > 16 * 1024 * 1024 {
                return Err("public file exceeds 16 MiB".into());
            }
            let route = if logical == "/index.html" {
                document["x-pishoo"]["entry"] = "/".into();
                "/"
            } else {
                &logical
            };
            let paths = document["paths"].as_object_mut().ok_or("missing paths")?;
            let ordinal = *count;
            let item = paths.entry(route.to_owned()).or_insert_with(|| json!({}));
            let operations = item
                .as_object_mut()
                .ok_or("static path must be an object")?;
            for method in ["post", "put", "patch", "delete", "options", "trace"] {
                if operations.contains_key(method) {
                    return Err(format!("public and backend route conflict: {route}").into());
                }
            }
            for method in ["get", "head"] {
                let target = format!("/public{logical}");
                if let Some(operation) = operations.get(method) {
                    if operation["x-pishoo"]["static-file"] != target {
                        return Err(format!("static mapping disagrees at {method} {route}").into());
                    }
                    // Preserve operationId, summary, groups and initial access.
                } else {
                    operations.insert(method.into(),json!({"operationId":format!("pishooFile{ordinal}{method}"),"summary":format!("Read {logical} ({method})"),"x-pishoo":{"static-file":target},"responses":{"200":{"description":"Static file"}}}));
                }
            }
            DirEntry::File(FileEntry::owned(
                std::fs::read(entry.path())?,
                Timestamps::default(),
            ))
        } else {
            return Err("public symlinks and special files are unsupported".into());
        };
        children.insert(name.parse()?, item);
    }
    Ok(Directory::new(children, Timestamps::default()))
}
fn copy_directory(volume: &webc::Volume, path: &str) -> Result<Directory<'static>> {
    let mut children = BTreeMap::new();
    for (name, _, metadata) in volume.read_dir(path).ok_or("missing directory")? {
        let child = format!("{}/{}", path.trim_end_matches('/'), name);
        let entry = match metadata {
            webc::Metadata::Dir { .. } => DirEntry::Dir(copy_directory(volume, &child)?),
            webc::Metadata::File { .. } => DirEntry::File(FileEntry::owned(
                volume
                    .read_file(child.as_str())
                    .ok_or("missing file")?
                    .0
                    .to_vec(),
                Timestamps::default(),
            )),
            _ => return Err("unexpected package symlink".into()),
        };
        children.insert(name, entry);
    }
    Ok(Directory::new(children, Timestamps::default()))
}

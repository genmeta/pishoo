//! Build a signed Pishoo WEBC release from independent backend/frontend inputs.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::json;
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
    if args.len() != 4 || args[2] != "--publisher" {
        return Err("usage: package-webc WASMER.toml OUTPUT.webc --publisher NAME".into());
    }
    let project = Path::new(&args[0])
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let config: toml::Value = toml::from_str(&std::fs::read_to_string(&args[0])?)?;
    if config.as_table().is_none_or(|table| {
        table
            .keys()
            .any(|key| !matches!(key.as_str(), "package" | "module" | "fs"))
    }) {
        return Err("unsupported package build input (commands, bindings and module dependencies are forbidden)".into());
    }
    let package = config.get("package").ok_or("missing package")?;
    let annotation = package.get("pishoo").ok_or("missing package.pishoo")?;
    let kind = annotation
        .get("kind")
        .and_then(toml::Value::as_str)
        .ok_or("missing kind")?;
    if !matches!(kind, "app" | "lib") {
        return Err("kind must be app or lib".into());
    }
    let endpoint = dhttp::Endpoint::load(args[3].to_str().ok_or("invalid publisher name")?).await?;
    let authority = endpoint.local_authority()?;
    let id = package
        .get("name")
        .and_then(toml::Value::as_str)
        .ok_or("missing full package name")?;
    let (publisher, _) = id
        .split_once('/')
        .ok_or("package name requires publisher/name")?;
    if publisher != endpoint.name() {
        return Err("package publisher differs from signing identity".into());
    }
    let modules = config.get("module").and_then(toml::Value::as_array);
    let backend = if kind == "app" {
        if modules.is_some_and(|v| !v.is_empty()) {
            return Err("app must have zero atoms".into());
        }
        Vec::new()
    } else {
        let module = modules
            .filter(|v| v.len() == 1)
            .ok_or("lib requires one backend module")?;
        if module[0].get("name").and_then(toml::Value::as_str) != Some("backend") {
            return Err("module must be backend".into());
        }
        let source = module[0]
            .get("source")
            .and_then(toml::Value::as_str)
            .ok_or("missing backend source")?;
        let bytes = std::fs::read(project.join(source))?;
        for payload in wasmparser::Parser::new(0).parse_all(&bytes) {
            if let wasmparser::Payload::CustomSection(section) = payload?
                && matches!(section.name(), "pishoo:openapi" | "pishoo:signature")
            {
                return Err("backend must not embed OpenAPI or signatures".into());
            }
        }
        pishoo::WasmRuntime::new()?.compile(&bytes)?;
        bytes
    };
    if backend.len() > 64 * 1024 * 1024 {
        return Err("backend exceeds 64 MiB".into());
    }
    let mount = if kind == "app" {
        "/public"
    } else {
        "/metadata"
    };
    let volume_name = if kind == "app" { "public" } else { "metadata" };
    let fs = config
        .get("fs")
        .and_then(toml::Value::as_table)
        .filter(|fs| fs.len() == 1)
        .ok_or("require one content mapping")?;
    let directory = fs
        .get(mount)
        .and_then(toml::Value::as_str)
        .ok_or("content mapping differs from kind")?;
    let mut count = 0;
    let content = public_directory(&project.join(directory), "", &mut count, 0)?;
    let mut wapm = serde_json::to_value(package)?;
    wapm.as_object_mut()
        .ok_or("package must be a table")?
        .remove("pishoo");
    let mut manifest: webc::metadata::Manifest = serde_json::from_value(json!({
        "package":{"wapm":wapm,"pishoo":annotation,"fs":[{"volume_name":volume_name,"mount_path":mount}]},
        "atoms":{"backend":{"kind":"https://webc.org/kind/wasm","signature":format!("sha256:{}",STANDARD.encode(Sha256::digest(&backend)))}}
    }))?;
    if kind == "app" {
        manifest.atoms.clear();
    }
    let atoms = || -> Result<_> {
        Ok(if kind == "app" {
            BTreeMap::new()
        } else {
            BTreeMap::from([(
                "backend".parse()?,
                FileEntry::borrowed(&backend, Timestamps::default()),
            )])
        })
    };
    let mut writer = Writer::default()
        .write_manifest(&manifest)?
        .write_atoms(atoms()?)?
        .with_volume(volume_name, content)?;
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
        .write_atoms(atoms()?)?;
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
    let parent = Path::new(&args[1])
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    pishoo::run_command(vec![kind.into(), "check".into(), temporary.path().into()]).await?;
    temporary.persist(&args[1])?;
    eprintln!(
        "Saved {} ({} bytes)",
        Path::new(&args[1]).display(),
        bytes.len()
    );
    Ok(())
}

fn public_directory(
    path: &Path,
    relative: &str,
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
        if *count > 4095 {
            return Err("too many public entries".into());
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "invalid public filename")?;
        let logical = format!("{relative}/{name}");
        let kind = entry.file_type()?;
        let item = if kind.is_dir() {
            DirEntry::Dir(public_directory(&entry.path(), &logical, count, depth + 1)?)
        } else if kind.is_file() {
            if entry.metadata()?.len() > 16 * 1024 * 1024 {
                return Err("public file exceeds 16 MiB".into());
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

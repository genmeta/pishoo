//! Build/sign local Pishoo packages; bare invocation signs existing artifacts.
use std::{
    collections::{BTreeMap, HashSet},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Stdio,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::{Arg, Command};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use webc::v3::{
    SignatureAlgorithm, Timestamps,
    write::{DirEntry, Directory, FileEntry, Writer},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().collect::<Vec<_>>();
    // Keep the original example's positional contract for build scripts.
    if args.len() == 5 && args[3] == "--publisher" {
        args = vec![
            args[0].clone(),
            args[1].clone(),
            "--output".into(),
            args[2].clone(),
            "--id".into(),
            args[4].clone(),
        ];
    }
    let options = Command::new("pishoo-pack")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Package local build artifacts as a signed Pishoo App or Lib")
        .arg(
            Arg::new("build")
                .long("build")
                .action(clap::ArgAction::SetTrue)
                .help("Run the manifest's local [build] command before signing its output"),
        )
        .arg(
            Arg::new("path")
                .help("Local project directory or TOML manifest")
                .default_value(".")
                .value_parser(clap::value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("output")
                .help(
                    "Output file (default: <manifest directory>/dist/<name>-<version>-<kind>.webc)",
                )
                .short('o')
                .long("output")
                .value_name("FILE.webc")
                .value_parser(clap::value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("id")
                .short('i')
                .long("id")
                .value_name("PUBLISHER")
                .help("Check the signing identity against the manifest publisher"),
        )
        .get_matches_from(args);
    let input = options.get_one::<PathBuf>("path").unwrap();
    reject_url(input.as_os_str())?;
    let input = std::fs::canonicalize(input)
        .map_err(|e| format!("missing local project or manifest {}: {e}", input.display()))?;
    let mut manifest_path = if input.is_dir() {
        input.join("wasmer.toml")
    } else {
        input
    };
    if options.get_flag("build") {
        let config: toml::Value = toml::from_str(&std::fs::read_to_string(&manifest_path)?)?;
        let build = config.get("build").and_then(toml::Value::as_table)
            .ok_or("source build requires [build] command=[\"program\", \"arg\", ...] in the local manifest")?;
        if build
            .keys()
            .any(|key| !matches!(key.as_str(), "command" | "manifest"))
        {
            return Err("build supports only command and optional output manifest".into());
        }
        let command = build
            .get("command")
            .and_then(toml::Value::as_array)
            .filter(|args| !args.is_empty())
            .ok_or("build.command must be a nonempty argument array")?;
        let command = command
            .iter()
            .map(|arg| {
                arg.as_str()
                    .ok_or("build.command arguments must be strings")
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if command[0].is_empty() {
            return Err("build.command program must not be empty".into());
        }
        let project = manifest_path
            .parent()
            .ok_or("manifest has no parent directory")?
            .to_path_buf();
        let output_manifest = match build.get("manifest") {
            Some(path) => {
                let path = Path::new(
                    path.as_str()
                        .ok_or("build.manifest must be a local relative path")?,
                );
                if path.as_os_str().is_empty()
                    || path.is_absolute()
                    || path
                        .components()
                        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
                {
                    return Err("build.manifest must stay within the project directory".into());
                }
                project.join(path)
            }
            None => manifest_path.clone(),
        };
        eprintln!("Building {}", manifest_path.display());
        let status = tokio::process::Command::new(command[0])
            .args(&command[1..])
            .current_dir(&project)
            .stdin(Stdio::null())
            .stdout(Stdio::from(std::io::stderr()))
            .stderr(Stdio::inherit())
            .status()
            .await?;
        if !status.success() {
            return Err(format!("source build failed ({status}); no package was signed").into());
        }
        manifest_path = std::fs::canonicalize(output_manifest)
            .map_err(|e| format!("build did not produce its package manifest: {e}"))?;
        if !manifest_path.starts_with(&project) {
            return Err("built manifest must stay within the project directory".into());
        }
    }
    let project = manifest_path
        .parent()
        .ok_or("manifest has no parent directory")?;
    let config: toml::Value = toml::from_str(&std::fs::read_to_string(&manifest_path)?)?;
    if config.as_table().is_none_or(|table| {
        table
            .keys()
            .any(|key| !matches!(key.as_str(), "package" | "module" | "fs" | "build"))
    }) {
        return Err(
            "unsupported package input: commands, bindings and module dependencies are forbidden"
                .into(),
        );
    }
    let package = config.get("package").ok_or("missing package")?;
    let annotation = package.get("pishoo").ok_or("missing package.pishoo")?;
    let wapm = {
        let mut value = serde_json::to_value(package)?;
        value
            .as_object_mut()
            .ok_or("package must be a table")?
            .remove("pishoo");
        value
    };
    let annotation = serde_json::to_value(annotation)?;
    validate_metadata(&wapm, &annotation)?;
    let id = text(&wapm, "name")?;
    let (publisher, name) = id.split_once('/').unwrap();
    let version = text(&wapm, "version")?;
    let kind = text(&annotation, "kind")?;
    if let Some(selected) = options.get_one::<String>("id")
        && dhttp_home::normalize_name(selected).as_deref() != Some(publisher)
    {
        return Err("--id differs from manifest publisher".into());
    }
    let modules = config.get("module");
    let backend = if kind == "app" {
        if modules.is_some_and(|m| m.as_array().is_none_or(|v| !v.is_empty())) {
            return Err("app must have zero backend modules".into());
        }
        Vec::new()
    } else {
        let module = modules
            .and_then(toml::Value::as_array)
            .filter(|v| v.len() == 1)
            .ok_or("lib requires exactly one backend module")?;
        if module[0].get("name").and_then(toml::Value::as_str) != Some("backend")
            || module[0]
                .as_table()
                .is_none_or(|t| t.keys().any(|k| !matches!(k.as_str(), "name" | "source")))
        {
            return Err("lib module must declare only name=backend and a local source".into());
        }
        let source = module[0]
            .get("source")
            .and_then(toml::Value::as_str)
            .ok_or("missing backend source")?;
        let source = local_content_path(project, source)?;
        let bytes = read_bounded(&source, 64 * 1024 * 1024)?;
        if bytes.get(..8) != Some(b"\0asm\x0d\0\x01\0") {
            return Err("backend must be a built WASM component".into());
        }
        bytes
    };
    let (mount, volume_name, required) = if kind == "app" {
        ("/public", "public", "index.html")
    } else {
        ("/metadata", "metadata", "openapi.json")
    };
    let fs = config
        .get("fs")
        .and_then(toml::Value::as_table)
        .filter(|fs| fs.len() == 1)
        .ok_or("require exactly one content mapping")?;
    let directory = fs
        .get(mount)
        .and_then(toml::Value::as_str)
        .ok_or("content mapping differs from package kind")?;
    let directory = local_content_path(project, directory)?;
    if !directory.is_dir() || !directory.join(required).is_file() {
        return Err(format!("missing build artifact: {mount}/{required}").into());
    }
    let output = options
        .get_one::<PathBuf>("output")
        .cloned()
        .unwrap_or_else(|| {
            project
                .join("dist")
                .join(format!("{name}-{version}-{kind}.webc"))
        });
    reject_url(output.as_os_str())?;
    if std::fs::symlink_metadata(&output).is_ok() {
        return Err(format!("output already exists: {}", output.display()).into());
    }
    let output = resolved_output(&output)?;
    let parent = output.parent().ok_or("output has no parent")?;
    ensure_outside_mapping(&output, parent, &directory)?;
    // Count the signature.json entry as part of the existing 4096-entry limit.
    let mut count = 1;
    let mut remaining = 64 * 1024 * 1024 - backend.len();
    let content = public_directory(&directory, "", &mut count, &mut remaining, 0)?;
    // Local-only loading; do not refresh OCSP or initialize a network.
    let home = dhttp_home::DhttpHome::for_user_home_dir(
        dirs::home_dir().ok_or("cannot locate user home directory")?,
    );
    let profile = home.identity_profile(publisher)?;
    let certificates = profile
        .load_certs()
        .await
        .map_err(|e| format!("missing or invalid local credentials for {publisher}: {e}"))?;
    let private_key = profile
        .load_key()
        .await
        .map_err(|e| format!("missing or invalid local credentials for {publisher}: {e}"))?;
    let signing_key = qtls::default_provider()
        .key_provider
        .load_private_key(private_key)?;
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
    let unsigned = Writer::default()
        .write_manifest(&manifest)?
        .write_atoms(atoms()?)?
        .with_volume(volume_name, content)?
        .finish(SignatureAlgorithm::None)?;
    let container = webc::Container::from_bytes_and_version(unsigned.clone(), webc::Version::V3)?;
    let reader = webc::v3::read::OwnedReader::parse(unsigned.clone())?;
    let index = reader.index();
    let digest = payload_digest(&unsigned)?;
    let mut input = b"pishoo-webc-signature-v1\0".to_vec();
    input.extend_from_slice(&digest);
    let mut signature = None;
    for scheme in [
        qtls::SignatureScheme::RSA_PSS_SHA512,
        qtls::SignatureScheme::ECDSA_NISTP256_SHA256,
        qtls::SignatureScheme::ECDSA_NISTP384_SHA384,
        qtls::SignatureScheme::ED25519,
    ] {
        if let Some(signer) = signing_key.choose_scheme(&[scheme]) {
            signature = Some(signer.sign(&input)?);
            break;
        }
    }
    let envelope = json!({"version":1,
        "certificates":certificates.iter().map(|c|STANDARD.encode(c.as_ref())).collect::<Vec<_>>(),
        "signature":STANDARD.encode(signature.ok_or("unsupported signing key")?)});
    let mut writer = Writer::default()
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
    if payload_digest(&bytes)? != digest {
        return Err("final package content differs from signed payload".into());
    }
    std::fs::create_dir_all(parent)?;
    // Recheck after directory creation, before any temporary file is created.
    let parent = std::fs::canonicalize(parent)?;
    let output = parent.join(output.file_name().ok_or("missing output filename")?);
    ensure_outside_mapping(&output, &parent, &std::fs::canonicalize(&directory)?)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&parent)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    let checked = check_package(kind, temporary.path()).await?;
    if checked["id"] != id || checked["kind"] != kind || checked["version"] != version {
        return Err("host validation returned different package metadata".into());
    }
    let result = json!({"id":id,"kind":kind,"version":version,
        "file":output.to_string_lossy(),"size":bytes.len(),
        "package_sha256":format!("{:x}", Sha256::digest(&bytes))});
    // persist_noclobber uses exclusive publication, including concurrent creators.
    temporary.persist_noclobber(&output).map_err(|e| {
        if e.error.kind() == std::io::ErrorKind::AlreadyExists {
            format!("output already exists: {}", output.display())
        } else {
            format!("cannot publish {}: {}", output.display(), e.error)
        }
    })?;
    #[cfg(unix)]
    std::fs::File::open(&parent)?.sync_all()?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

fn reject_url(value: &std::ffi::OsStr) -> Result<()> {
    if value.to_str().is_some_and(|s| {
        s.contains("://")
            || s.split_once(':').is_some_and(|(scheme, _)| {
                scheme
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphabetic)
                    && scheme
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
                    && !(cfg!(windows) && scheme.len() == 1)
            })
    }) {
        return Err("only local filesystem paths are accepted; URLs are unsupported".into());
    }
    Ok(())
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("missing or empty {key}").into())
}

fn valid_version(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<u64>().is_ok()
                && (p.len() == 1 || !p.starts_with('0'))
        })
}

fn valid_id(id: &str) -> bool {
    id.split_once('/').is_some_and(|(publisher, name)| {
        dhttp_home::normalize_name(publisher).as_deref() == Some(publisher)
            && (1..=63).contains(&name.len())
            && name.as_bytes()[0].is_ascii_lowercase()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}

fn validate_metadata(wapm: &Value, annotation: &Value) -> Result<()> {
    let id = text(wapm, "name")?;
    if !valid_id(id) {
        return Err("invalid normalized publisher/name".into());
    }
    if !valid_version(text(wapm, "version")?) {
        return Err("version must be MAJOR.MINOR.PATCH".into());
    }
    text(wapm, "description")?;
    text(wapm, "license")?;
    if let Some(repository) = wapm.get("repository") {
        let raw = repository
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or("repository must be a nonempty HTTPS URL")?;
        let url = url::Url::parse(raw).map_err(|e| format!("invalid repository URL: {e}"))?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !raw
                .get(..8)
                .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
            || raw[8..]
                .split(['/', '?', '#'])
                .next()
                .is_none_or(str::is_empty)
            || !url.username().is_empty()
            || url.password().is_some()
            || raw[8..]
                .split(['/', '?', '#'])
                .next()
                .is_some_and(|s| s.contains('@'))
            || raw.chars().any(char::is_whitespace)
        {
            return Err(
                "repository must be an HTTPS URL with a host and without credentials".into(),
            );
        }
    }
    let kind = text(annotation, "kind")?;
    if !matches!(kind, "app" | "lib") {
        return Err("kind must be app or lib".into());
    }
    text(annotation, "title")?;
    text(annotation, "changelog")?;
    if annotation.as_object().is_none_or(|o| {
        o.keys().any(|k| {
            !matches!(
                k.as_str(),
                "kind" | "title" | "dependencies" | "changelog" | "entry" | "host"
            )
        })
    }) {
        return Err("unknown package.pishoo field".into());
    }
    let deps = annotation["dependencies"]
        .as_array()
        .filter(|v| v.len() <= 100)
        .ok_or("dependencies requires an explicit array with at most 100 Libs")?;
    let mut ids = HashSet::new();
    for dep in deps {
        let dep_id = format!("{}/{}", text(dep, "publisher")?, text(dep, "name")?);
        if dep.as_object().is_none_or(|o| o.len() != 3)
            || !valid_id(&dep_id)
            || !valid_version(text(dep, "version")?)
            || !ids.insert(dep_id.clone())
            || (kind == "lib" && dep_id == id)
        {
            return Err("invalid, duplicate or self Lib dependency".into());
        }
    }
    if kind == "app" {
        if annotation["entry"] != "/" || annotation.get("host").is_some() {
            return Err("app requires entry / and forbids host".into());
        }
    } else {
        if annotation.get("entry").is_some() {
            return Err("lib forbids entry".into());
        }
        let host = annotation["host"]
            .as_object()
            .ok_or("lib requires explicit host purposes")?;
        for (key, purpose) in host {
            if !matches!(
                key.as_str(),
                "private-storage" | "request-identity" | "signatures"
            ) || purpose.as_str().is_none_or(|s| s.trim().is_empty())
            {
                return Err("unknown or unexplained host capability".into());
            }
        }
    }
    Ok(())
}

fn local_content_path(project: &Path, source: &str) -> Result<PathBuf> {
    reject_url(std::ffi::OsStr::new(source))?;
    let source = project.join(source);
    let mut path = PathBuf::new();
    for component in source.components() {
        path.push(component);
        if std::fs::symlink_metadata(&path)
            .map_err(|e| format!("missing build artifact {}: {e}", path.display()))?
            .file_type()
            .is_symlink()
        {
            return Err(format!(
                "build artifact paths cannot contain symlinks: {}",
                path.display()
            )
            .into());
        }
    }
    Ok(std::fs::canonicalize(source)?)
}

fn resolved_output(path: &Path) -> Result<PathBuf> {
    let absolute = std::env::current_dir()?.join(path);
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            _ => {
                resolved.push(component);
            }
        }
        match std::fs::symlink_metadata(&resolved) {
            Ok(_) => {
                resolved = std::fs::canonicalize(&resolved)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    if resolved.file_name().is_none() {
        return Err("output requires a filename".into());
    }
    Ok(resolved)
}

fn ensure_outside_mapping(output: &Path, parent: &Path, directory: &Path) -> Result<()> {
    if output.starts_with(directory) || parent.starts_with(directory) {
        return Err("output and staging directory are inside a content mapping; use an external -o FILE.webc".into());
    }
    Ok(())
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| format!("missing build artifact {}: {e}", path.display()))?;
    if !metadata.is_file() {
        return Err("build artifacts must be regular files without symlinks".into());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Do not follow a replaced symlink or block if a source became a FIFO.
        options.custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
        );
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err("build artifacts must be regular files".into());
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(format!("build artifact exceeds size limit: {}", path.display()).into());
    }
    Ok(bytes)
}

fn payload_digest(bytes: &[u8]) -> Result<[u8; 32]> {
    let reader = webc::v3::read::OwnedReader::parse(bytes.to_vec())?;
    let index = reader.index();
    let mut hash = Sha256::new();
    hash.update(b"\0webc003");
    for entry in std::iter::once(&index.manifest)
        .chain(std::iter::once(&index.atoms))
        .chain(
            index
                .volumes
                .iter()
                .filter(|(name, _)| name.as_str() != "pishoo-signature")
                .map(|(_, entry)| entry),
        )
    {
        hash.update(&bytes[entry.span.start..entry.span.end()]);
    }
    Ok(hash.finalize().into())
}

async fn check_package(kind: &str, path: &Path) -> Result<Value> {
    let executable = std::env::current_exe()?;
    let name = format!("pishoo{}", std::env::consts::EXE_SUFFIX);
    let mut tool = executable.with_file_name(&name);
    // cargo run --example package-webc keeps its executable under examples/.
    if !tool.is_file()
        && executable.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new("examples"))
    {
        tool = executable
            .parent()
            .unwrap()
            .parent()
            .ok_or("missing example parent")?
            .join(&name);
    }
    if !tool.is_file() {
        tool = PathBuf::from(&name);
    }
    let output = tokio::process::Command::new(tool)
        .arg(kind)
        .arg("check")
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| {
            format!(
                "cannot run Pishoo package validator; install Pishoo alongside pishoo-pack: {e}"
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "signature/component validation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn public_directory(
    path: &Path,
    relative: &str,
    count: &mut usize,
    remaining: &mut usize,
    depth: usize,
) -> Result<Directory<'static>> {
    if depth > 32 || std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err("content must contain real directories, depth <= 32".into());
    }
    let mut children = BTreeMap::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        *count += 1;
        if *count > 4096 {
            return Err("too many content entries".into());
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "invalid UTF-8 content filename")?;
        let logical = format!("{relative}/{name}");
        if logical.len() > 1024
            || name.is_empty()
            || name == "."
            || name == ".."
            || name.contains(['?', '#', '%', '\\'])
            || name.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err("invalid content file path".into());
        }
        let kind = entry.file_type()?;
        let item = if kind.is_dir() {
            DirEntry::Dir(public_directory(
                &entry.path(),
                &logical,
                count,
                remaining,
                depth + 1,
            )?)
        } else if kind.is_file() {
            let bytes = read_bounded(&entry.path(), (16 * 1024 * 1024).min(*remaining))?;
            *remaining -= bytes.len();
            DirEntry::File(FileEntry::owned(bytes, Timestamps::default()))
        } else {
            return Err("content symlinks and special files are unsupported".into());
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

//! Standard ZIP release packages, signature verification and runtime projection.
// Protocol matches the independent store's ZIP package validation.
use std::collections::{BTreeMap, HashSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Error, Result};
const PACKAGE_LIMIT: usize = 64 * 1024 * 1024;
const METHODS: [&str; 8] = [
    "get", "head", "post", "put", "patch", "delete", "options", "trace",
];
fn invalid(message: &str) -> Error {
    Error::InvalidComponent(message.into())
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid(&format!("missing or empty {key}")))
}
fn webc_u64(bytes: &[u8], offset: usize) -> Result<usize> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| invalid("WEBC length overflow"))?;
    let raw: [u8; 8] = bytes
        .get(offset..end)
        .ok_or_else(|| invalid("truncated WEBC length"))?
        .try_into()
        .unwrap();
    usize::try_from(u64::from_le_bytes(raw)).map_err(|_| invalid("WEBC length overflow"))
}

pub(crate) fn read_package(bytes: &[u8]) -> Result<webc::Container> {
    let original = bytes;
    let projection = waz_convert(bytes).map_err(|error| invalid(&error.to_string()))?;
    let bytes = projection.as_slice();
    // The dependency has lazy readers. Bound and authenticate every top-level span
    // before using them, including rejecting unindexed sections and trailing bytes.
    if bytes.len() > PACKAGE_LIMIT
        || bytes.get(..8) != Some(b"\0webc003")
        || bytes.get(8) != Some(&2)
    {
        return Err(invalid("expected a bounded WEBC v3 package"));
    }
    let end = 17usize
        .checked_add(webc_u64(bytes, 9)?)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| invalid("truncated WEBC index"))?;
    if end > 1024 * 1024 {
        return Err(invalid("WEBC index exceeds 1 MiB"));
    }
    let section = webc::v3::read::Section::parse(2, None, bytes[17..end].to_vec().into())
        .map_err(|_| invalid("invalid WEBC index"))?;
    let index = section
        .as_index()
        .unwrap()
        .index()
        .map_err(|_| invalid("invalid WEBC index"))?;
    if !index.signature.is_none() || !index.signature.value.is_empty() || index.volumes.len() > 16 {
        return Err(invalid("unsupported WEBC index signature or volume count"));
    }
    let mut spans = BTreeMap::new();
    for (tag, name, entry) in std::iter::once((1, "", &index.manifest))
        .chain(std::iter::once((3, "", &index.atoms)))
        .chain(
            index
                .volumes
                .iter()
                .map(|(name, entry)| (4, name.as_str(), entry)),
        )
    {
        let stop = entry
            .span
            .start
            .checked_add(entry.span.len)
            .filter(|stop| *stop <= bytes.len())
            .ok_or_else(|| invalid("WEBC span exceeds input"))?;
        if entry.span.start < end
            || entry.span.len < 41
            || spans.insert(entry.span.start, stop).is_some()
        {
            return Err(invalid("overlapping WEBC sections"));
        }
        let raw = &bytes[entry.span.start..stop];
        if raw[0] != tag || webc_u64(raw, 33)? != raw.len() - 41 {
            return Err(invalid("WEBC section disagrees with index"));
        }
        let hash = Sha256::digest(&raw[41..]);
        if raw[1..33] != hash[..] || entry.checksum != webc::v3::Checksum::sha256(hash.into()) {
            return Err(invalid("WEBC section checksum mismatch"));
        }
        if tag == 1 && raw.len() > 1024 * 1024 {
            return Err(invalid("WEBC manifest exceeds 1 MiB"));
        }
        if tag == 4 {
            let name_end = 8usize
                .checked_add(webc_u64(&raw[41..], 0)?)
                .filter(|n| *n <= raw.len() - 41)
                .ok_or_else(|| invalid("invalid WEBC volume name"))?;
            if &raw[49..41 + name_end] != name.as_bytes() {
                return Err(invalid("WEBC volume name disagrees with index"));
            }
        }
    }
    let mut position = end;
    for (start, stop) in spans {
        if start != position {
            return Err(invalid("unindexed or overlapping WEBC bytes"));
        }
        position = stop;
    }
    if position != bytes.len() {
        return Err(invalid("unindexed trailing WEBC bytes"));
    }
    let container =
        webc::Container::from_bytes_and_version(bytes.to_vec().into(), webc::Version::V3)
            .map_err(|_| invalid("malformed WEBC package"))?;
    if container.get_volume("pishoo-signature").is_none() {
        return Err(invalid("incoming package requires a publisher signature"));
    }
    // Use an iterative bounded walk before any recursive dependency validator.
    let mut count = 0;
    for (name, volume) in container.volumes() {
        let before = count;
        let files = volume_files(&volume, "/", &mut count)?;
        if name == "metadata"
            && (files.is_empty()
                || count - before != files.len()
                || files
                    .keys()
                    .any(|path| !matches!(path.as_str(), "/README.md" | "/CHANGELOG.md")))
        {
            return Err(invalid(
                "metadata volume may only contain README.md and CHANGELOG.md",
            ));
        }
    }
    container
        .validate()
        .map_err(|_| invalid("invalid WEBC atom, mapping or file digest"))?;
    let manifest = container.manifest();
    if !manifest.use_map.is_empty()
        || !manifest.commands.is_empty()
        || manifest.entrypoint.is_some()
        || !manifest.bindings.is_empty()
        || manifest.atoms.len() > 1
        || manifest.atoms.keys().any(|name| name != "backend")
        || manifest.atoms.len() != container.atoms().len()
    {
        return Err(invalid(
            "package must contain one backend atom without commands, bindings or dependencies",
        ));
    }
    // Only installed runtime snapshots may use an old short-name alias. Every
    // incoming release still has a canonical, signed publisher/name identity.
    let metadata = package_metadata(&container)?;
    if metadata.get("publisher").and_then(Value::as_str).is_none() {
        return Err(invalid("package requires publisher/name"));
    }
    if metadata["name"] == "std" {
        return Err(invalid(
            "package name conflicts with the native std namespace",
        ));
    }
    if metadata["name"] == "app"
        && metadata["has_lib"] == true
        && metadata["publisher"] != "std.dhttp.net"
    {
        return Err(invalid(
            "ordinary Lib name conflicts with the frontend app namespace",
        ));
    }
    let mappings = manifest
        .filesystem()
        .map_err(|_| invalid("invalid filesystem mappings"))?
        .unwrap_or_default();
    if mappings.len() > 1
        || mappings.iter().any(|m| {
            m.mount_path != "/public"
                || m.from.is_some()
                || matches!(m.volume_name.as_str(), "pishoo-signature" | "metadata")
        })
        || container.volumes().keys().any(|name| {
            !matches!(name.as_str(), "pishoo-signature" | "metadata")
                && !mappings.iter().any(|m| &m.volume_name == name)
        })
    {
        return Err(invalid("only an optional local /public mapping is allowed"));
    }
    for mapping in mappings.iter() {
        let root = mapping.host_path.as_deref().unwrap_or("/");
        if !file_path(root)
            || container
                .get_volume(&mapping.volume_name)
                .and_then(|v| v.read_dir(root))
                .is_none()
        {
            return Err(invalid("invalid mapped WEBC directory"));
        }
    }
    if let Some(backend) = container.get_atom("backend") {
        if backend.get(..8) != Some(b"\0asm\x0d\0\x01\0") {
            return Err(invalid("backend must be a WASM component"));
        }
        for item in wasmparser::Parser::new(0).parse_all(&backend) {
            if let wasmparser::Payload::CustomSection(section) =
                item.map_err(|_| invalid("malformed backend"))?
                && section.name() == "pishoo:signature"
            {
                return Err(invalid(
                    "backend must not embed a second publisher signature",
                ));
            }
        }
    }
    validate_content(&container, &metadata)?;
    if container.get_atom("backend").is_some() {
        let raw = package_file(&container, "/metadata/openapi.json")?;
        let api = super::validate_openapi(&raw)?;
        if metadata["publisher"] == "std.dhttp.net"
            && api.paths.as_ref().unwrap().keys().any(|path| {
                crate::routes::reserved(path)
                    || [
                        "/acl",
                        "/contact",
                        "/contacts",
                        "/sys",
                        "/pishoo",
                        "/file",
                        "/dhttp",
                    ]
                    .iter()
                    .any(|prefix| {
                        path == prefix
                            || path
                                .strip_prefix(prefix)
                                .is_some_and(|tail| tail.starts_with('/'))
                    })
            })
        {
            return Err(invalid("standard Lib conflicts with a reserved API path"));
        }
        if metadata["publisher"] == "std.dhttp.net" {
            let prefix = format!("/{}", metadata["name"].as_str().unwrap());
            if api.paths.as_ref().unwrap().keys().any(|path| {
                path != &prefix
                    && !path
                        .strip_prefix(&prefix)
                        .is_some_and(|tail| tail.starts_with('/'))
            }) {
                return Err(invalid(
                    "standard Lib paths must stay under its package name",
                ));
            }
        }
        let document: Value =
            serde_json::from_slice(&raw).map_err(|_| invalid("invalid OpenAPI JSON"))?;
        if document["info"]["version"] != metadata["version"] {
            return Err(invalid("OpenAPI version differs from package version"));
        }
        validate_declarations(&document)?;
    }
    verify_signature(original, &container, &metadata)?;
    Ok(container)
}

fn file_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains("//")
        && !path.contains(['?', '#', '%', '\\'])
        && !path.chars().any(|c| c.is_control() || c.is_whitespace())
        && !path.split('/').any(|s| s == "." || s == "..")
        && path.len() <= 1024
        && (path == "/" || !path.ends_with('/'))
}

fn volume_files(
    volume: &webc::Volume,
    root: &str,
    count: &mut usize,
) -> Result<BTreeMap<String, usize>> {
    let mut files = BTreeMap::new();
    let mut directories = vec![(root.to_owned(), 0)];
    while let Some((path, depth)) = directories.pop() {
        if depth > 32 {
            return Err(invalid("WEBC directory depth exceeds 32"));
        }
        let entries = volume
            .read_dir(path.as_str())
            .ok_or_else(|| invalid("unreadable WEBC directory"))?;
        let mut names = HashSet::new();
        for (name, _, metadata) in entries {
            *count += 1;
            if *count > 4096 || !names.insert(name.to_string()) {
                return Err(invalid("too many or duplicate WEBC entries"));
            }
            let child = format!("{}/{}", path.trim_end_matches('/'), name);
            if !file_path(&child) {
                return Err(invalid("invalid WEBC file path"));
            }
            match metadata {
                webc::Metadata::Dir { .. } => directories.push((child, depth + 1)),
                webc::Metadata::File { length, .. } => {
                    if length > 16 * 1024 * 1024 {
                        return Err(invalid("WEBC file exceeds 16 MiB"));
                    }
                    let (data, hash) = volume
                        .read_file(child.as_str())
                        .ok_or_else(|| invalid("unreadable WEBC file"))?;
                    if data.len() != length || hash != Some(Sha256::digest(&data).into()) {
                        return Err(invalid("WEBC file checksum mismatch"));
                    }
                    files.insert(child, length);
                }
                _ => return Err(invalid("WEBC symlinks are unsupported")),
            }
        }
    }
    Ok(files)
}

pub(crate) fn package_file(container: &webc::Container, path: &str) -> Result<bytes::Bytes> {
    if !file_path(path) {
        return Err(invalid("invalid logical package path"));
    }
    // OpenAPI is signed as part of the backend atom. Keep the historical logical
    // path so policy, capability and runtime consumers all read the same bytes.
    if path == "/metadata/openapi.json"
        && let Some(backend) = container.get_atom("backend")
    {
        let mut document = None;
        let mut depth = 0;
        for item in wasmparser::Parser::new(0).parse_all(&backend) {
            match item.map_err(|_| invalid("malformed backend"))? {
                wasmparser::Payload::ModuleSection { .. }
                | wasmparser::Payload::ComponentSection { .. } => depth += 1,
                wasmparser::Payload::End(_) if depth > 0 => depth -= 1,
                wasmparser::Payload::CustomSection(section)
                    if depth > 0 && section.name() == "pishoo:openapi" =>
                {
                    return Err(invalid(
                        "OpenAPI must be embedded in the top-level backend component",
                    ));
                }
                wasmparser::Payload::CustomSection(section)
                    if depth == 0 && section.name() == "pishoo:openapi" =>
                {
                    if document.is_some() || section.data().len() > 1024 * 1024 {
                        return Err(invalid("duplicate or oversized embedded OpenAPI"));
                    }
                    document = Some(bytes::Bytes::copy_from_slice(section.data()));
                }
                _ => {}
            }
        }
        if let Some(document) = document {
            let mappings = container
                .manifest()
                .filesystem()
                .map_err(|_| invalid("invalid mappings"))?;
            if let Some(mapping) = mappings.as_ref().and_then(|items| {
                items
                    .iter()
                    .find(|mapping| mapping.mount_path == "/metadata")
            }) {
                let target = format!(
                    "{}/openapi.json",
                    mapping
                        .host_path
                        .as_deref()
                        .unwrap_or("/")
                        .trim_end_matches('/')
                );
                if container
                    .get_volume(&mapping.volume_name)
                    .and_then(|volume| volume.read_file(target.as_str()))
                    .is_some()
                {
                    return Err(invalid("OpenAPI must have a single signed source"));
                }
            }
            return Ok(document);
        }
    }
    let mappings = container
        .manifest()
        .filesystem()
        .map_err(|_| invalid("invalid mappings"))?
        .ok_or_else(|| invalid("missing mappings"))?;
    let mapping = mappings
        .iter()
        .find(|m| {
            path.strip_prefix(&m.mount_path)
                .is_some_and(|tail| tail.starts_with('/'))
        })
        .ok_or_else(|| invalid("path has no package mapping"))?;
    let tail = path.strip_prefix(&mapping.mount_path).unwrap();
    let target = format!(
        "{}{}",
        mapping
            .host_path
            .as_deref()
            .unwrap_or("/")
            .trim_end_matches('/'),
        tail
    );
    let volume = container
        .get_volume(&mapping.volume_name)
        .ok_or_else(|| invalid("missing mapped volume"))?;
    volume
        .read_file(target.as_str())
        .map(|(data, _)| bytes::Bytes::copy_from_slice(&data))
        .ok_or_else(|| invalid("missing mapped file"))
}

// Hash sorted uncompressed ZIP files. Signature bytes and ZIP compression or
// timestamp metadata are outside the payload digest.
fn package_payload(bytes: &[u8]) -> Result<String> {
    let files = waz_read(bytes).map_err(|error| invalid(&error.to_string()))?;
    Ok(waz_payload(&files))
}

fn verify_signature(bytes: &[u8], container: &webc::Container, document: &Value) -> Result<()> {
    let volume = container
        .get_volume("pishoo-signature")
        .ok_or_else(|| invalid("missing publisher signature volume"))?;
    let mut count = 0;
    let files = volume_files(&volume, "/", &mut count)?;
    if files.len() != 1
        || count != 1
        || files
            .get("/signature.json")
            .is_none_or(|size| *size > 64 * 1024)
    {
        return Err(invalid(
            "signature volume must contain only a bounded signature.json",
        ));
    }
    let (data, _) = volume.read_file("/signature.json").unwrap();
    let envelope: Value =
        serde_json::from_slice(&data).map_err(|_| invalid("invalid signature envelope"))?;
    if envelope.as_object().is_none_or(|o| o.len() != 3)
        || envelope["version"].as_u64() != Some(1)
        || envelope["certificates"]
            .as_array()
            .is_none_or(Vec::is_empty)
    {
        return Err(invalid("unsupported signature envelope"));
    }
    let signature = STANDARD
        .decode(text(&envelope, "signature")?)
        .map_err(|_| invalid("invalid signature encoding"))?;
    if signature.is_empty() || signature.len() > 8192 {
        return Err(invalid("invalid signature size"));
    }
    for cert in envelope["certificates"].as_array().unwrap() {
        if STANDARD
            .decode(
                cert.as_str()
                    .ok_or_else(|| invalid("invalid certificate encoding"))?,
            )
            .map_err(|_| invalid("invalid certificate encoding"))?
            .is_empty()
        {
            return Err(invalid("empty signer certificate"));
        }
    }
    let certificates = envelope["certificates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| {
            STANDARD
                .decode(value.as_str().unwrap())
                .map(qtls::CertificateDer::from)
                .map_err(|_| invalid("invalid signer certificate"))
        })
        .collect::<Result<Vec<_>>>()?;
    let (remaining, certificate) = x509_parser::parse_x509_certificate(certificates[0].as_ref())
        .map_err(|_| invalid("invalid signer certificate"))?;
    if !remaining.is_empty() {
        return Err(invalid("trailing signer certificate data"));
    }
    let publisher = document["publisher"].as_str().unwrap();
    dhttp_home::certificate::extract_dhttp_subject_key_identifier(&certificates)
        .map_err(|_| invalid("invalid signer owner_hash"))?;
    if certificate.subject_alternative_name().map_err(|_| invalid("invalid signer names"))?
        .is_none_or(|names| !names.value.general_names.iter().any(|name|
            matches!(name, x509_parser::extensions::GeneralName::DNSName(name) if *name == publisher))) {
        return Err(invalid("signer certificate and publisher disagree"));
    }
    let payload = package_payload(bytes)?;
    let mut input = b"pishoo-waz-signature-v1\0".to_vec();
    for i in (0..payload.len()).step_by(2) {
        input.push(u8::from_str_radix(&payload[i..i + 2], 16).unwrap());
    }
    if !dhttp_home::certificate::verify_signature(certificate.public_key().raw, &input, &signature)
        .map_err(|_| invalid("unsupported publisher signing key"))?
    {
        return Err(invalid("publisher signature verification failed"));
    }
    Ok(())
}

/// Derive package metadata from the manifest; never fall back to OpenAPI.
pub(crate) fn package_metadata(container: &webc::Container) -> Result<Value> {
    let manifest = serde_json::to_value(container.manifest())
        .map_err(|_| invalid("invalid package manifest"))?;
    let wapm = &manifest["package"]["wapm"];
    let annotation = &manifest["package"]["pishoo"];
    let id = text(wapm, "name")?;
    super::valid_id(id).map_err(|_| invalid("invalid full package identity"))?;
    let (publisher, name) = match id.split_once('/') {
        Some((publisher, name)) => {
            if dhttp_home::normalize_name(publisher).as_deref() != Some(publisher) {
                return Err(invalid("publisher must be normalized"));
            }
            (Some(publisher), name)
        }
        None if container.get_volume("pishoo-signature").is_none() => (None, id),
        None => return Err(invalid("package requires publisher/name")),
    };
    let version = text(wapm, "version")?;
    if !release_version(version) {
        return Err(invalid("version must be MAJOR.MINOR.PATCH"));
    }
    // Older signed packages may contain host purposes; ignore them entirely.
    let allowed = ["title", "dependencies", "changelog", "entry", "host"];
    if annotation
        .as_object()
        .is_none_or(|o| o.keys().any(|k| !allowed.contains(&k.as_str())))
    {
        return Err(invalid("unknown package.pishoo field"));
    }
    let dependencies = annotation["dependencies"]
        .as_array()
        .filter(|v| v.len() <= 100)
        .ok_or_else(|| invalid("dependencies requires an explicit array with at most 100 Libs"))?;
    let mut ids = HashSet::new();
    for dep in dependencies {
        let publisher = text(dep, "publisher")?;
        let name = text(dep, "name")?;
        let id_dep = format!("{publisher}/{name}");
        if dep.as_object().is_none_or(|o| o.len() != 3)
            || super::valid_id(&id_dep).is_err()
            || dhttp_home::normalize_name(publisher).as_deref() != Some(publisher)
            || !release_version(text(dep, "version")?)
            || !ids.insert(id_dep.clone())
            || id_dep == id
        {
            return Err(invalid("invalid, duplicate or self Lib dependency"));
        }
    }
    let mut value = serde_json::json!({"id":id,"name":name,"version":version,
        "title":text(annotation,"title")?,"description":text(wapm,"description")?,
        "license":text(wapm,"license")?,"dependencies":dependencies});
    if container.get_volume("pishoo-signature").is_some() || annotation.get("changelog").is_some() {
        value["changelog"] = text(annotation, "changelog")?.into();
    }
    if let Some(publisher) = publisher {
        value["publisher"] = publisher.into();
    }
    if let Some(repository) = wapm.get("repository") {
        let raw = repository
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| invalid("repository must be a nonempty HTTPS URL"))?;
        let url = url::Url::parse(raw).map_err(|_| invalid("invalid repository URL"))?;
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
            return Err(invalid(
                "repository must be an HTTPS URL with a host and without credentials",
            ));
        }
        value["repository"] = repository.clone();
    }
    let has_lib = container.get_atom("backend").is_some();
    let has_app = container
        .manifest()
        .filesystem()
        .map_err(|_| invalid("invalid mappings"))?
        .is_some_and(|m| m.iter().any(|m| m.mount_path == "/public"));
    if !has_lib && !has_app {
        return Err(invalid("package has no runtime content"));
    }
    if has_app {
        if annotation["entry"] != "/" {
            return Err(invalid("frontend requires entry /"));
        }
        value["entry"] = "/".into();
    } else if annotation.get("entry").is_some() {
        return Err(invalid("entry requires public resources"));
    }
    value["has_lib"] = has_lib.into();
    value["has_app"] = has_app.into();
    Ok(value)
}

fn release_version(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_digit())
                && part.parse::<u64>().is_ok()
                && (part.len() == 1 || !part.starts_with('0'))
        })
}

fn validate_content(container: &webc::Container, metadata: &Value) -> Result<()> {
    if metadata["has_app"] == true {
        package_file(container, "/public/index.html")?;
    }
    Ok(())
}

fn valid_name(value: &str) -> bool {
    value.len() <= 63
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn validate_declarations(document: &Value) -> Result<()> {
    let info = &document["info"];
    text(info, "title")?;
    let extension = &document["x-pishoo"];
    if extension.get("templates").is_some() {
        return Err(invalid("external templates are not part of store v1"));
    }
    if extension
        .as_object()
        .is_some_and(|o| o.keys().any(|k| k != "access-groups"))
    {
        return Err(invalid("OpenAPI cannot declare package metadata"));
    }
    let groups = match (
        document.get("x-access-groups"),
        extension.get("access-groups"),
    ) {
        (Some(_), Some(_)) => return Err(invalid("duplicate access-groups declarations")),
        (groups, legacy) => groups.or(legacy),
    };
    let paths = document["paths"]
        .as_object()
        .ok_or_else(|| invalid("paths must be an object"))?;
    let mut operations = BTreeMap::new();
    let mut annotated = BTreeMap::<String, HashSet<String>>::new();
    for (path, item) in paths {
        if ["/api", "/app"].iter().any(|mount| {
            path == mount
                || path
                    .strip_prefix(mount)
                    .is_some_and(|tail| tail.starts_with('/'))
        }) {
            return Err(invalid(
                "business API cannot contain host mounting prefixes",
            ));
        }
        for method in METHODS {
            let Some(operation) = item.get(method) else {
                continue;
            };
            let id = text(operation, "operationId")?;
            text(operation, "summary")?;
            if operations.insert(id.to_owned(), (path, method)).is_some() {
                return Err(invalid("duplicate operationId"));
            }
            let op_extension = &operation["x-pishoo"];
            if op_extension.as_object().is_some_and(|o| {
                o.keys()
                    .any(|key| !matches!(key.as_str(), "access-group" | "initial-access"))
            }) {
                return Err(invalid("template operations are unsupported"));
            }
            let group = match (
                operation.get("x-access-group"),
                op_extension.get("access-group"),
            ) {
                (Some(_), Some(_)) => return Err(invalid("duplicate access-group declarations")),
                (group, legacy) => group.or(legacy),
            };
            if let Some(group) = group {
                let group = group
                    .as_str()
                    .filter(|s| valid_name(s))
                    .ok_or_else(|| invalid("invalid access-group"))?;
                annotated
                    .entry(group.to_owned())
                    .or_default()
                    .insert(id.to_owned());
            }
            if let Some(initial) = op_extension.get("initial-access") {
                let initial = initial
                    .as_array()
                    .ok_or_else(|| invalid("initial-access must be an array"))?;
                let mut grantees = HashSet::new();
                for rule in initial {
                    if rule.as_object().is_none_or(|o| o.len() != 2) {
                        return Err(invalid("initial-access requires only grantee and effect"));
                    }
                    let grantee = text(rule, "grantee")?;
                    grantee
                        .parse::<access_control::Grantee>()
                        .map_err(|_| invalid("invalid grantee"))?;
                    text(rule, "effect")?
                        .parse::<access_control::Effect>()
                        .map_err(|_| invalid("invalid effect"))?;
                    if !grantees.insert(grantee) {
                        return Err(invalid("duplicate initial-access grantee"));
                    }
                }
                if grantees.contains("*?") && (grantees.contains("**") || grantees.contains("?")) {
                    return Err(invalid("conflicting wildcard grantees"));
                }
            }
        }
    }
    let mut declared = BTreeMap::<String, HashSet<String>>::new();
    let mut members = HashSet::new();
    if let Some(groups) = groups {
        for (name, group) in groups
            .as_object()
            .ok_or_else(|| invalid("access-groups must be an object"))?
        {
            if !valid_name(name) {
                return Err(invalid("invalid group name"));
            }
            if let Some(titles) = group["title"].as_object() {
                if titles.len() != 2 || !titles.contains_key("en") || !titles.contains_key("zh-CN")
                {
                    return Err(invalid("localized group title requires en and zh-CN"));
                }
                text(&group["title"], "en")?;
                text(&group["title"], "zh-CN")?;
            } else {
                text(group, "title")?;
            }
            let ids = group["operations"]
                .as_array()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| invalid("group must contain operations"))?;
            for id in ids {
                let id = id
                    .as_str()
                    .filter(|id| operations.contains_key(*id))
                    .ok_or_else(|| invalid("unknown group operation"))?;
                if !members.insert(id.to_owned()) {
                    return Err(invalid("overlapping or duplicate group operation"));
                }
                declared
                    .entry(name.to_owned())
                    .or_default()
                    .insert(id.to_owned());
            }
        }
    }
    if declared != annotated {
        return Err(invalid("access-groups disagree with operation annotations"));
    }
    Ok(())
}

// WAZ v1 boundary codec, kept in sync with Jade archive.rs.

type WazResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const WAZ_LIMIT: usize = 64 * 1024 * 1024;

fn waz_is_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
}

fn waz_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.contains(['\\', ':', '?', '#', '%'])
        && !path.chars().any(|c| c.is_control() || c.is_whitespace())
        && path.split('/').count() <= 33
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
        && (matches!(
            path,
            "manifest.json" | "signature.json" | "backend.wasm" | "README.md" | "CHANGELOG.md"
        ) || path.starts_with("public/"))
}

fn waz_read(bytes: &[u8]) -> WazResult<std::collections::BTreeMap<String, Vec<u8>>> {
    use std::io::Read;

    if bytes.len() > WAZ_LIMIT || !waz_is_zip(bytes) || bytes.len() < 22 {
        return Err("expected a bounded standard ZIP package".into());
    }
    // This profile uses ordinary single-disk ZIP, without ZIP64, comments,
    // self-extracting prefixes or trailing data. Every byte belongs to the archive.
    let end = bytes.len() - 22;
    let eocd = &bytes[end..];
    let u16_at = |offset: usize| u16::from_le_bytes(eocd[offset..offset + 2].try_into().unwrap());
    let u32_at =
        |offset: usize| u32::from_le_bytes(eocd[offset..offset + 4].try_into().unwrap()) as usize;
    let count = u16_at(10) as usize;
    let directory_size = u32_at(12);
    let directory_start = u32_at(16);
    if eocd[..4] != *b"PK\x05\x06"
        || u16_at(4) != 0
        || u16_at(6) != 0
        || u16_at(20) != 0
        || u16_at(8) as usize != count
        || count == 0
        || count > 4096
        || directory_start.checked_add(directory_size) != Some(end)
    {
        return Err("invalid ZIP end record or entry budget".into());
    }
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    if archive.len() != count || archive.offset() != 0 {
        return Err("ZIP central directory disagrees with archive".into());
    }
    let mut files = std::collections::BTreeMap::new();
    let mut spans = std::collections::BTreeMap::new();
    let mut central_spans = std::collections::BTreeMap::new();
    let mut remaining = WAZ_LIMIT;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = std::str::from_utf8(entry.name_raw())?.to_owned();
        if !waz_path(&name)
            || entry.is_dir()
            || entry.encrypted()
            || !matches!(
                entry.compression(),
                zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
            )
            || entry
                .unix_mode()
                .is_some_and(|mode| !matches!(mode & 0o170000, 0 | 0o100000))
            || files.contains_key(&name)
        {
            return Err(format!("invalid, duplicate or unsupported ZIP entry: {name}").into());
        }
        let limit = match name.as_str() {
            "manifest.json" => 1024 * 1024,
            "signature.json" => 64 * 1024,
            "backend.wasm" => WAZ_LIMIT,
            _ => 16 * 1024 * 1024,
        }
        .min(remaining);
        let size = usize::try_from(entry.size())?;
        if size > limit {
            return Err(format!("ZIP entry exceeds its decompressed size limit: {name}").into());
        }
        let start = usize::try_from(entry.header_start())?;
        let data = usize::try_from(entry.data_start())?;
        let stop = data
            .checked_add(usize::try_from(entry.compressed_size())?)
            .ok_or("ZIP span overflow")?;
        let header = bytes
            .get(start..start.checked_add(30).ok_or("ZIP header overflow")?)
            .ok_or("truncated ZIP local header")?;
        let name_length = u16::from_le_bytes(header[26..28].try_into().unwrap()) as usize;
        let extra_length = u16::from_le_bytes(header[28..30].try_into().unwrap()) as usize;
        let flags = u16::from_le_bytes(header[6..8].try_into().unwrap());
        let central_start = usize::try_from(entry.central_header_start())?;
        let central = bytes
            .get(
                central_start
                    ..central_start
                        .checked_add(46)
                        .ok_or("ZIP directory overflow")?,
            )
            .ok_or("truncated ZIP central header")?;
        let central_length = 46
            + u16::from_le_bytes(central[28..30].try_into().unwrap()) as usize
            + u16::from_le_bytes(central[30..32].try_into().unwrap()) as usize
            + u16::from_le_bytes(central[32..34].try_into().unwrap()) as usize;
        let central_end = central_start
            .checked_add(central_length)
            .ok_or("ZIP directory span overflow")?;
        if central[..4] != *b"PK\x01\x02"
            || central_start < directory_start
            || central_end > end
            || u16::from_le_bytes(central[8..10].try_into().unwrap()) != flags
            || central_spans.insert(central_start, central_end).is_some()
        {
            return Err("invalid ZIP central header".into());
        }
        if header[..4] != *b"PK\x03\x04"
            || flags & !0x0806 != 0 // UTF-8 and Deflate level hints only; no encryption/descriptors.
            || bytes.get(start + 30..start + 30 + name_length) != Some(name.as_bytes())
            || start + 30 + name_length + extra_length != data
            || u16::from_le_bytes(header[8..10].try_into().unwrap()) != if entry.compression() == zip::CompressionMethod::Stored { 0 } else { 8 }
            || u32::from_le_bytes(header[14..18].try_into().unwrap()) != entry.crc32()
            || u32::from_le_bytes(header[18..22].try_into().unwrap()) as u64 != entry.compressed_size()
            || u32::from_le_bytes(header[22..26].try_into().unwrap()) as u64 != entry.size()
            || stop > directory_start || spans.insert(start, stop).is_some()
        {
            return Err("ZIP local header disagrees with the central directory".into());
        }
        let mut content = Vec::new();
        entry
            .by_ref()
            .take(size as u64 + 1)
            .read_to_end(&mut content)?;
        if content.len() != size {
            return Err("ZIP decompressed size disagrees with header".into());
        }
        remaining -= content.len();
        files.insert(name, content);
    }
    let mut cursor = 0;
    for (start, stop) in spans {
        if start != cursor {
            return Err("overlapping or unindexed ZIP data".into());
        }
        cursor = stop;
    }
    if cursor != directory_start {
        return Err("unindexed ZIP data before directory".into());
    }
    for (start, stop) in central_spans {
        if start != cursor {
            return Err("overlapping or unindexed ZIP directory".into());
        }
        cursor = stop;
    }
    if cursor != end {
        return Err("unindexed ZIP directory data".into());
    }
    for name in files.keys() {
        let mut ancestor = name.as_str();
        while let Some((parent, _)) = ancestor.rsplit_once('/') {
            if files.contains_key(parent) {
                return Err("ZIP file conflicts with a directory".into());
            }
            ancestor = parent;
        }
    }
    if !files.contains_key("manifest.json") {
        return Err("ZIP package requires manifest.json".into());
    }
    Ok(files)
}

fn waz_payload(files: &std::collections::BTreeMap<String, Vec<u8>>) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"pishoo-waz-payload-v1\0");
    for (name, bytes) in files
        .iter()
        .filter(|(name, _)| name.as_str() != "signature.json")
    {
        digest.update((name.len() as u32).to_le_bytes());
        digest.update(name.as_bytes());
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(Sha256::digest(bytes));
    }
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
pub(super) fn waz_write(files: &std::collections::BTreeMap<String, Vec<u8>>) -> WazResult<Vec<u8>> {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(6))
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    for (name, bytes) in files {
        writer.start_file(name, options)?;
        writer.write_all(bytes)?;
    }
    let bytes = writer.finish()?.into_inner();
    waz_read(&bytes)?;
    Ok(bytes)
}

fn waz_directory(
    files: &std::collections::BTreeMap<String, Vec<u8>>,
    prefix: &str,
) -> WazResult<webc::v3::write::Directory<'static>> {
    use webc::v3::{
        Timestamps,
        write::{DirEntry, Directory, FileEntry},
    };
    let mut children = std::collections::BTreeMap::new();
    for (name, bytes) in files {
        let Some(relative) = name.strip_prefix(prefix) else {
            continue;
        };
        if let Some((directory, _)) = relative.split_once('/') {
            let key = directory.parse()?;
            if let std::collections::btree_map::Entry::Vacant(entry) = children.entry(key) {
                entry.insert(DirEntry::Dir(waz_directory(
                    files,
                    &format!("{prefix}{directory}/"),
                )?));
            }
        } else {
            children.insert(
                relative.parse()?,
                DirEntry::File(FileEntry::owned(bytes.clone(), Timestamps::default())),
            );
        }
    }
    Ok(Directory::new(children, Timestamps::default()))
}

// Runtime tests load the same internal resource projection as installed packages.
#[cfg(test)]
pub(crate) fn runtime_fixture(bytes: &[u8]) -> Vec<u8> {
    waz_convert(bytes).unwrap()
}

fn waz_convert(bytes: &[u8]) -> WazResult<Vec<u8>> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use webc::v3::{
        SignatureAlgorithm, Timestamps,
        write::{Directory, FileEntry, Writer},
    };

    let files = waz_read(bytes)?;
    let document: Value = serde_json::from_slice(&files["manifest.json"])?;
    if document["format"] != "waz/1"
        || document.as_object().is_none_or(|v| {
            v.keys().any(|k| {
                !matches!(
                    k.as_str(),
                    "format" | "package" | "dependencies" | "frontend" | "backend"
                )
            })
        })
    {
        return Err("unsupported WAZ manifest format or field".into());
    }
    let package = document["package"]
        .as_object()
        .ok_or("manifest package requires an object")?;
    if package.keys().any(|k| {
        !matches!(
            k.as_str(),
            "author"
                | "name"
                | "version"
                | "title"
                | "description"
                | "license"
                | "release-notes"
                | "repository"
        )
    }) {
        return Err("unknown WAZ package field".into());
    }
    let field = |name: &str| {
        package
            .get(name)
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| format!("missing WAZ package {name}"))
    };
    let mut wapm = json!({"name":format!("{}/{}", field("author")?, field("name")?),
        "version":field("version")?,"description":field("description")?,"license":field("license")?});
    if let Some(repository) = package.get("repository") {
        wapm["repository"] = repository.clone();
    }
    if files.contains_key("README.md") {
        wapm["readme"] = json!({"volume":"metadata","path":"/README.md"});
    }
    let dependencies = document["dependencies"]
        .as_array()
        .filter(|v| v.len() <= 100)
        .ok_or("WAZ dependencies requires an array with at most 100 packages")?;
    let mut dependencies_out = Vec::new();
    for dependency in dependencies {
        if dependency.as_object().is_none_or(|v| {
            v.len() != 3
                || !v.contains_key("author")
                || !v.contains_key("name")
                || !v.contains_key("version")
        }) {
            return Err("WAZ dependency requires author, name and version".into());
        }
        dependencies_out.push(json!({"publisher":dependency["author"],"name":dependency["name"],"version":dependency["version"]}));
    }
    let has_frontend = document.get("frontend").is_some();
    let has_backend = document.get("backend").is_some();
    if (has_frontend && document["frontend"] != "public")
        || (has_backend && document["backend"] != "backend.wasm")
        || has_backend != files.contains_key("backend.wasm")
        || has_frontend != files.keys().any(|name| name.starts_with("public/"))
        || (has_frontend && !files.contains_key("public/index.html"))
        || (!has_frontend && !has_backend)
    {
        return Err("WAZ manifest runtime paths disagree with package files".into());
    }
    let mut annotation = json!({"title":field("title")?,"changelog":field("release-notes")?,"dependencies":dependencies_out});
    if has_frontend {
        annotation["entry"] = "/".into();
    }
    let mut manifest = json!({"package":{"wapm":wapm,"pishoo":annotation,
        "fs":if has_frontend {json!([{"volume_name":"public","mount_path":"/public"}])} else {json!([])}}});
    let mut atoms = std::collections::BTreeMap::new();
    if let Some(backend) = files.get("backend.wasm") {
        manifest["atoms"] = json!({"backend":{"kind":"https://webc.org/kind/wasm","signature":format!("sha256:{}", STANDARD.encode(Sha256::digest(backend)))}});
        atoms.insert(
            "backend".parse()?,
            FileEntry::owned(backend.clone(), Timestamps::default()),
        );
    }
    let manifest: webc::metadata::Manifest = serde_json::from_value(manifest)?;
    let mut writer = Writer::default()
        .write_manifest(&manifest)?
        .write_atoms(atoms)?;
    if has_frontend {
        writer.write_volume("public", waz_directory(&files, "public/")?)?;
    }
    let documents = files
        .iter()
        .filter(|(name, _)| matches!(name.as_str(), "README.md" | "CHANGELOG.md"))
        .map(|(name, data)| Ok((name.parse()?, data.clone().into())))
        .collect::<WazResult<std::collections::BTreeMap<_, _>>>()?;
    if !documents.is_empty() {
        writer.write_volume("metadata", Directory::new(documents, Timestamps::default()))?;
    }
    if let Some(signature) = files.get("signature.json") {
        writer.write_volume(
            "pishoo-signature",
            Directory::new(
                std::collections::BTreeMap::from([(
                    "signature.json".parse()?,
                    signature.clone().into(),
                )]),
                Timestamps::default(),
            ),
        )?;
    }
    // Keep the existing in-memory resource API and installation snapshots. These
    // bytes are an internal projection, never a signed archive or published file.
    Ok(writer.finish(SignatureAlgorithm::None)?.to_vec())
}

#[cfg(test)]
pub(super) fn waz_export(
    container: &webc::Container,
) -> WazResult<std::collections::BTreeMap<String, Vec<u8>>> {
    use serde_json::json;
    let old = serde_json::to_value(container.manifest())?;
    let wapm = &old["package"]["wapm"];
    let annotation = &old["package"]["pishoo"];
    let (author, name) = wapm["name"]
        .as_str()
        .and_then(|v| v.split_once('/'))
        .ok_or("package requires author/name")?;
    let mut package = json!({"author":author,"name":name,"version":wapm["version"],"description":wapm["description"],
        "license":wapm["license"],"title":annotation["title"],"release-notes":annotation["changelog"]});
    if let Some(repository) = wapm.get("repository") {
        package["repository"] = repository.clone();
    }
    let dependencies = annotation["dependencies"]
        .as_array()
        .ok_or("missing dependencies")?
        .iter()
        .map(|v| json!({"author":v["publisher"],"name":v["name"],"version":v["version"]}))
        .collect::<Vec<_>>();
    let mut manifest = json!({"format":"waz/1","package":package,"dependencies":dependencies});
    let mut files = std::collections::BTreeMap::new();
    if let Some(backend) = container.get_atom("backend") {
        manifest["backend"] = "backend.wasm".into();
        files.insert("backend.wasm".into(), backend.to_vec());
    }
    if let Some(volume) = container.get_volume("public") {
        manifest["frontend"] = "public".into();
        let mut pending = vec!["/".to_owned()];
        while let Some(directory) = pending.pop() {
            for (name, _, metadata) in volume
                .read_dir(directory.as_str())
                .ok_or("unreadable resource directory")?
            {
                let path = format!("{directory}{name}");
                match metadata {
                    webc::Metadata::Dir { .. } => pending.push(format!("{path}/")),
                    webc::Metadata::File { .. } => {
                        files.insert(
                            format!("public{path}"),
                            volume
                                .read_file(path.as_str())
                                .ok_or("unreadable resource")?
                                .0
                                .to_vec(),
                        );
                    }
                    _ => return Err("unsupported resource type".into()),
                }
            }
        }
    }
    if let Some(volume) = container.get_volume("metadata") {
        for name in ["README.md", "CHANGELOG.md"] {
            if let Some((data, _)) = volume.read_file(format!("/{name}")) {
                files.insert(name.into(), data.to_vec());
            }
        }
    }
    files.insert(
        "manifest.json".into(),
        serde_json::to_vec_pretty(&manifest)?,
    );
    Ok(files)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use webc::v3::{
        SignatureAlgorithm, Timestamps,
        write::{FileEntry, Writer},
    };

    use super::*;

    #[test]
    fn signed_lib_reads_embedded_openapi_without_host_purposes_or_a_sidecar() {
        for host in [
            None,
            Some(json!({"unknown-capability":""})),
            Some(json!(false)),
        ] {
            let document = serde_json::to_vec(&json!({
                "openapi":"3.1.0", "info":{"title":"Embedded API","version":"1.0.0"},
                "paths":{}
            }))
            .unwrap();
            let mut backend = b"\0asm\x0d\0\x01\0".to_vec();
            let mut payload = vec![b"pishoo:openapi".len() as u8];
            payload.extend_from_slice(b"pishoo:openapi");
            payload.extend_from_slice(&document);
            backend.push(0);
            let mut length = payload.len();
            while length >= 0x80 {
                backend.push((length as u8 & 0x7f) | 0x80);
                length >>= 7;
            }
            backend.push(length as u8);
            backend.extend_from_slice(&payload);
            let authority = crate::package::tests::publisher("alice.dhttp.net", 'a');
            let mut annotation =
                json!({"title":"Embedded","changelog":"First release","dependencies":[]});
            if let Some(host) = host {
                annotation["host"] = host;
            }
            let manifest: webc::metadata::Manifest = serde_json::from_value(json!({
            "package":{
                "wapm":{"name":"alice.dhttp.net/embedded","version":"1.0.0","description":"Embedded API","license":"MIT"},
                "pishoo":annotation,
                "fs":[]
            },
            "atoms":{"backend":{"kind":"https://webc.org/kind/wasm","signature":format!("sha256:{}",STANDARD.encode(Sha256::digest(&backend)))}}
        })).unwrap();
            let atoms = || {
                BTreeMap::from([(
                    "backend".parse().unwrap(),
                    FileEntry::borrowed(&backend, Timestamps::default()),
                )])
            };
            let unsigned = Writer::default()
                .write_manifest(&manifest)
                .unwrap()
                .write_atoms(atoms())
                .unwrap()
                .finish(SignatureAlgorithm::None)
                .unwrap();
            let projection =
                webc::Container::from_bytes_and_version(unsigned, webc::Version::V3).unwrap();
            let mut files = waz_export(&projection).unwrap();
            let unsigned = waz_write(&files).unwrap();
            let mut input = b"pishoo-waz-signature-v1\0".to_vec();
            let digest = package_payload(&unsigned).unwrap();
            for index in (0..digest.len()).step_by(2) {
                input.push(u8::from_str_radix(&digest[index..index + 2], 16).unwrap());
            }
            let signature = authority
                .sign(qtls::SignatureScheme::ECDSA_NISTP256_SHA256, &input)
                .unwrap();
            let envelope = json!({"version":1,
            "certificates":authority.certificates().iter().map(|cert|STANDARD.encode(cert.as_ref())).collect::<Vec<_>>(),
            "signature":STANDARD.encode(signature)});
            files.insert(
                "signature.json".into(),
                serde_json::to_vec(&envelope).unwrap(),
            );
            let bytes = waz_write(&files).unwrap();
            let package = read_package(&bytes).unwrap();
            assert_eq!(
                package_file(&package, "/metadata/openapi.json").unwrap(),
                document
            );
            assert!(package.get_volume("metadata").is_none());
            assert!(package_metadata(&package).unwrap().get("host").is_none());
        }
    }

    #[test]
    fn signed_packages_accept_only_optional_readme_and_changelog_documents() {
        let authority = crate::package::tests::publisher("alice.dhttp.net", 'a');
        for names in [
            vec![],
            vec!["README.md"],
            vec!["CHANGELOG.md"],
            vec!["README.md", "CHANGELOG.md"],
        ] {
            let manifest = json!({"format":"waz/1","package":{
                "author":"alice.dhttp.net","name":"docs","version":"1.0.0","description":"Documentation","license":"MIT",
                "title":"Docs","release-notes":"First release"},"dependencies":[],"frontend":"public"});
            let mut files = BTreeMap::from([
                (
                    "manifest.json".to_owned(),
                    serde_json::to_vec(&manifest).unwrap(),
                ),
                ("public/index.html".to_owned(), b"<h1>Docs</h1>".to_vec()),
            ]);
            for name in &names {
                files.insert((*name).to_owned(), b"# Docs".to_vec());
            }
            let digest = waz_payload(&files);
            let mut input = b"pishoo-waz-signature-v1\0".to_vec();
            for index in (0..digest.len()).step_by(2) {
                input.push(u8::from_str_radix(&digest[index..index + 2], 16).unwrap());
            }
            let signature = authority
                .sign(qtls::SignatureScheme::ECDSA_NISTP256_SHA256, &input)
                .unwrap();
            files.insert("signature.json".into(),serde_json::to_vec(&json!({"version":1,
                "certificates":authority.certificates().iter().map(|c|STANDARD.encode(c.as_ref())).collect::<Vec<_>>(),
                "signature":STANDARD.encode(signature)})).unwrap());
            let bytes = waz_write(&files).unwrap();
            let package = read_package(&bytes).unwrap();
            assert_eq!(package.get_volume("metadata").is_some(), !names.is_empty());
            for name in &names {
                assert_eq!(
                    package
                        .get_volume("metadata")
                        .unwrap()
                        .read_file(format!("/{name}"))
                        .unwrap()
                        .0
                        .as_ref(),
                    b"# Docs"
                );
            }
            if let Some(name) = names.first() {
                files.insert((*name).into(), b"# Changed docs".to_vec());
                assert!(read_package(&waz_write(&files).unwrap()).is_err());
            }
            assert!(
                read_package(&waz_convert(&bytes).unwrap()).is_err(),
                "raw WEBC packages are unsupported"
            );
        }
    }
}

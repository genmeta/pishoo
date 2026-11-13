//! Standard ZIP release packages, signature verification and direct file access.
// Protocol matches the independent store's ZIP package validation.
use std::collections::{BTreeMap, HashSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;

use crate::{Error, Result};
pub(crate) type PackageFiles = BTreeMap<String, Vec<u8>>;
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
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Frozen current-time inspection entry; online installation uses trusted historical time."
    )
)]
pub(crate) fn read_package(bytes: &[u8]) -> Result<PackageFiles> {
    read_package_at(bytes, chrono::Utc::now().timestamp())
}
pub(super) fn read_package_at(bytes: &[u8], at: i64) -> Result<PackageFiles> {
    let files = waz_read(bytes).map_err(|error| invalid(&error.to_string()))?;
    let metadata = validate_contents(&files)?;
    let chain = super::signing::certificates(&files)?;
    super::signing::verify_chain(&chain, metadata["publisher"].as_str().unwrap(), at)?;
    verify_signature(&files, &metadata)?;
    Ok(files)
}

fn validate_contents(files: &PackageFiles) -> Result<Value> {
    let metadata = package_metadata(files)?;
    if metadata["name"] == "std" {
        return Err(invalid(
            "package name conflicts with the native std namespace",
        ));
    }
    if metadata["name"] == "app"
        && metadata["has_lib"] == true
        && !crate::package::is_standard_author(metadata["publisher"].as_str().unwrap_or(""))
    {
        return Err(invalid(
            "ordinary Lib name conflicts with the frontend app namespace",
        ));
    }
    if let Some(backend) = waz_backend(files) {
        if backend.get(..8) != Some(b"\0asm\x0d\0\x01\0") {
            return Err(invalid("backend must be a WASM component"));
        }
        for item in wasmparser::Parser::new(0).parse_all(backend) {
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

    if waz_backend(files).is_some() {
        let raw = package_openapi(files)?;
        let api = super::validate_openapi(raw)?;
        if crate::package::is_standard_author(metadata["publisher"].as_str().unwrap_or(""))
            && api.paths.as_ref().unwrap().keys().any(|path| {
                crate::routes::reserved(path)
                    || [
                        "/acl",
                        "/contact",
                        "/contacts",
                        "/sys",
                        "/pishoo",
                        "/file",
                        "/forward",
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
        let document: Value =
            serde_json::from_slice(raw).map_err(|_| invalid("invalid OpenAPI JSON"))?;
        if document["info"]["version"] != metadata["version"] {
            return Err(invalid("OpenAPI version differs from package version"));
        }
        validate_declarations(&document)?;
        let groups = document
            .get("x-access-groups")
            .or_else(|| document["x-pishoo"].get("access-groups"));
        if metadata["has_app"] != true
            && groups
                .and_then(Value::as_object)
                .is_some_and(|groups| groups.values().any(|group| group["app"] == true))
        {
            return Err(invalid(
                "app capability requires this package's public resources",
            ));
        }
    }
    Ok(metadata)
}

pub(crate) fn package_openapi(files: &PackageFiles) -> Result<&[u8]> {
    let backend = waz_backend(files).ok_or_else(|| invalid("package has no backend"))?;
    let mut document = None;
    let mut depth = 0;
    for item in wasmparser::Parser::new(0).parse_all(backend) {
        match item.map_err(|_| invalid("malformed backend"))? {
            wasmparser::Payload::ModuleSection { .. }
            | wasmparser::Payload::ComponentSection { .. } => depth += 1,
            wasmparser::Payload::End(_) if depth > 0 => depth -= 1,
            wasmparser::Payload::CustomSection(section) if section.name() == "pishoo:openapi" => {
                if depth != 0 || document.is_some() || section.data().len() > 1024 * 1024 {
                    return Err(invalid(
                        "OpenAPI must be a unique bounded top-level section",
                    ));
                }
                document = Some(section.data());
            }
            _ => {}
        }
    }
    document.ok_or_else(|| invalid("missing embedded OpenAPI"))
}

fn verify_signature(files: &PackageFiles, document: &Value) -> Result<()> {
    let envelope = waz_signature(files).map_err(|error| invalid(&error.to_string()))?;
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
    let payload = waz_payload(files).map_err(|error| invalid(&error.to_string()))?;
    let mut input = if files.contains_key("manifest.toml") {
        b"pishoo-waz-signature-v2\0"
    } else {
        b"pishoo-waz-signature-v1\0"
    }
    .to_vec();
    for i in (0..payload.len()).step_by(2) {
        input.push(u8::from_str_radix(&payload[i..i + 2], 16).unwrap());
    }
    if !dhttp_home::certificate::verify_signature(certificate.public_key().raw, &input, &signature)
        .map_err(|_| invalid("unsupported publisher signing key"))?
    {
        return Err(super::signing::policy(
            "package_signature_invalid",
            http::StatusCode::UNPROCESSABLE_ENTITY,
        ));
    }
    Ok(())
}

/// Derive package metadata from the authenticated release manifest.
pub(crate) fn package_metadata(files: &PackageFiles) -> Result<Value> {
    let manifest = waz_manifest(files).map_err(|error| invalid(&error.to_string()))?;
    waz_layout(files, &manifest).map_err(|error| invalid(&error.to_string()))?;
    let package = &manifest["package"];
    if package.as_object().is_none_or(|v| {
        v.keys().any(|key| {
            !matches!(
                key.as_str(),
                "author"
                    | "name"
                    | "version"
                    | "title"
                    | "description"
                    | "license"
                    | "release-notes"
                    | "repository"
            )
        })
    }) {
        return Err(invalid("unknown WAZ package field"));
    }
    let publisher = text(package, "author")?;
    let name = text(package, "name")?;
    let id = format!("{publisher}/{name}");
    super::valid_id(&id).map_err(|_| invalid("invalid full package identity"))?;
    if dhttp_home::normalize_name(publisher).as_deref() != Some(publisher) {
        return Err(invalid("author must be normalized"));
    }
    let version = text(package, "version")?;
    if !release_version(version) {
        return Err(invalid("version must be MAJOR.MINOR.PATCH"));
    }
    let dependencies = manifest["dependencies"]
        .as_array()
        .filter(|v| v.len() <= 100)
        .ok_or_else(|| {
            invalid("dependencies requires an explicit array with at most 100 packages")
        })?;
    let mut ids = HashSet::new();
    let mut dependencies_out = Vec::new();
    for dependency in dependencies {
        let author = dependency
            .get("author")
            .map(|_| text(dependency, "author"))
            .transpose()?;
        let dep_name = text(dependency, "name")?;
        let dep_version = text(dependency, "version")?;
        let dep_id = format!("{}/{dep_name}", author.unwrap_or(publisher));
        if dependency
            .as_object()
            .is_none_or(|o| o.len() != 2 + usize::from(author.is_some()))
            || super::valid_id(&dep_id).is_err()
            || author
                .is_some_and(|author| dhttp_home::normalize_name(author).as_deref() != Some(author))
            || !release_version(dep_version)
            || !ids.insert(dep_name)
            || dep_name == name
        {
            return Err(invalid("invalid, duplicate or self package dependency"));
        }
        let mut item = serde_json::json!({"name":dep_name,"version":dep_version});
        if let Some(author) = author {
            item["publisher"] = author.into();
        }
        dependencies_out.push(item);
    }
    let has_lib = manifest.get("backend").is_some();
    let has_app = manifest.get("frontend").is_some();
    let mut value = serde_json::json!({"id":id,"publisher":publisher,"name":name,"version":version,
        "title":text(package,"title")?,"description":text(package,"description")?,"license":text(package,"license")?,
        "changelog":text(package,"release-notes")?,"dependencies":dependencies_out,"has_lib":has_lib,"has_app":has_app});
    if let Some(docs) = manifest.get("docs") {
        value["docs"] = docs.clone();
    }
    if has_app {
        value["entry"] = "/".into();
    }
    if let Some(repository) = package.get("repository") {
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
    let mut app_group = false;
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
            if group.get("app").is_some_and(|app| !app.is_boolean()) {
                return Err(invalid("group app must be a boolean"));
            }
            if group["app"] == true {
                if app_group {
                    return Err(invalid("app entry belongs to multiple groups"));
                }
                app_group = true;
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

// WAZ v1/v2 boundary codec, mirrored in Jade and the store.
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
        ) || path == "manifest.toml"
            || path.starts_with("public/")
            || path.starts_with("wasm/")
            || path.starts_with("web/")
            || path.starts_with("docs/"))
}

pub(super) fn waz_read(bytes: &[u8]) -> WazResult<std::collections::BTreeMap<String, Vec<u8>>> {
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
            "manifest.json" | "manifest.toml" => 1024 * 1024,
            "signature.json" => 64 * 1024,
            "backend.wasm" => WAZ_LIMIT,
            _ if name.starts_with("wasm/") => WAZ_LIMIT,
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
    if files.contains_key("manifest.json") == files.contains_key("manifest.toml") {
        return Err("ZIP package requires exactly one manifest.json or manifest.toml".into());
    }
    if let Some(manifest) = files.get("manifest.toml") {
        waz_toml(manifest)?;
    }
    Ok(files)
}

fn waz_payload(files: &std::collections::BTreeMap<String, Vec<u8>>) -> WazResult<String> {
    use sha2::{Digest, Sha256};
    let modern = files.contains_key("manifest.toml");
    let mut digest = Sha256::new();
    digest.update(if modern {
        b"pishoo-waz-payload-v2\0"
    } else {
        b"pishoo-waz-payload-v1\0"
    });
    for (name, bytes) in files
        .iter()
        .filter(|(name, _)| modern || name.as_str() != "signature.json")
    {
        let content = if name == "manifest.toml" {
            &bytes[..waz_toml(bytes)?.1]
        } else {
            bytes.as_slice()
        };
        digest.update((name.len() as u32).to_le_bytes());
        digest.update(name.as_bytes());
        digest.update((content.len() as u64).to_le_bytes());
        digest.update(Sha256::digest(content));
    }
    Ok(format!("{:x}", digest.finalize()))
}

// These stateless boundary helpers are mirrored in Jade, Pishoo and the store.
fn waz_toml(bytes: &[u8]) -> WazResult<(serde_json::Value, usize)> {
    use serde_json::Value;
    let source = std::str::from_utf8(bytes)?;
    let document: Value = toml::from_str(source)?;
    if document.get("signature").is_none() {
        return Ok((document, bytes.len()));
    }
    let positions: std::collections::BTreeMap<String, toml::Spanned<Value>> =
        toml::from_str(source)?;
    let span = positions
        .get("signature")
        .ok_or("missing signature table")?
        .span();
    let boundary = span.start;
    let after_header = &source[span.end..];
    if source.get(span) != Some("[signature]")
        || (boundary != 0 && bytes[boundary - 1] != b'\n')
        || !(after_header.is_empty()
            || after_header.starts_with('\n')
            || after_header.starts_with("\r\n"))
    {
        return Err("signature must use a final [signature] table on its own line".into());
    }
    if bytes.len() - boundary > 64 * 1024 {
        return Err("publisher signature exceeds 64 KiB".into());
    }
    let unsigned: Value = toml::from_str(&source[..boundary])?;
    let suffix: Value = toml::from_str(&source[boundary..])?;
    let mut expected = document.clone();
    expected
        .as_object_mut()
        .ok_or("manifest must be a table")?
        .remove("signature");
    if unsigned != expected
        || suffix.as_object().is_none_or(|fields| fields.len() != 1)
        || suffix["signature"] != document["signature"]
    {
        return Err("signature must be last and contain only its own fields".into());
    }
    Ok((document, boundary))
}

pub(super) fn waz_manifest(
    files: &std::collections::BTreeMap<String, Vec<u8>>,
) -> WazResult<serde_json::Value> {
    if let Some(bytes) = files.get("manifest.toml") {
        Ok(waz_toml(bytes)?.0)
    } else {
        Ok(serde_json::from_slice(
            files
                .get("manifest.json")
                .ok_or("missing package manifest")?,
        )?)
    }
}

pub(super) fn waz_signature(
    files: &std::collections::BTreeMap<String, Vec<u8>>,
) -> WazResult<serde_json::Value> {
    if files.contains_key("manifest.toml") {
        let mut signature = waz_manifest(files)?["signature"].clone();
        let fields = signature
            .as_object_mut()
            .ok_or("missing publisher signature")?;
        if fields.len() != 3
            || !fields.contains_key("version")
            || !fields.contains_key("certificates")
        {
            return Err("unsupported signature envelope".into());
        }
        let value = fields.remove("value").ok_or("missing signature value")?;
        fields.insert("signature".into(), value);
        Ok(signature)
    } else {
        Ok(serde_json::from_slice(
            files
                .get("signature.json")
                .ok_or("missing publisher signature")?,
        )?)
    }
}

pub(super) fn waz_backend(files: &std::collections::BTreeMap<String, Vec<u8>>) -> Option<&Vec<u8>> {
    files.get("backend.wasm").or_else(|| {
        files
            .iter()
            .find(|(name, _)| name.starts_with("wasm/"))
            .map(|(_, bytes)| bytes)
    })
}

fn waz_layout(
    files: &std::collections::BTreeMap<String, Vec<u8>>,
    document: &serde_json::Value,
) -> WazResult<()> {
    let modern = files.contains_key("manifest.toml");
    let format = if modern { "waz/2" } else { "waz/1" };
    if document["format"] != format
        || document.as_object().is_none_or(|fields| {
            fields.keys().any(|key| {
                !(matches!(
                    key.as_str(),
                    "format" | "package" | "dependencies" | "backend" | "frontend"
                ) || modern && matches!(key.as_str(), "docs" | "signature"))
            })
        })
    {
        return Err("unsupported WAZ manifest format or field".into());
    }
    if modern
        && document["package"]["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("std/"))
        && !matches!(
            document["package"]["author"].as_str(),
            Some("std.dhttp.net" | "dev.genmeta.tech.dhttp.net")
        )
    {
        return Err("std/ package names require a standard publisher".into());
    }
    let backend = if modern {
        format!(
            "wasm/{}.wasm",
            document["package"]["name"]
                .as_str()
                .ok_or("missing package name")?
        )
    } else {
        "backend.wasm".to_owned()
    };
    let frontend = if modern { "web" } else { "public" };
    let prefix = format!("{frontend}/");
    let has_frontend = document.get("frontend").is_some();
    let has_backend = document.get("backend").is_some();
    if (has_backend && document["backend"] != backend)
        || (has_frontend && document["frontend"] != frontend)
        || has_backend != files.contains_key(&backend)
        || has_frontend != files.keys().any(|name| name.starts_with(&prefix))
        || (has_frontend && !files.contains_key(&format!("{prefix}index.html")))
        || (!has_backend && !has_frontend)
        || files
            .keys()
            .any(|name| name.starts_with("wasm/") && name != &backend)
    {
        return Err("WAZ manifest runtime paths disagree with package files".into());
    }
    if modern {
        if files.keys().any(|name| {
            matches!(
                name.as_str(),
                "manifest.json" | "signature.json" | "backend.wasm" | "README.md" | "CHANGELOG.md"
            ) || name.starts_with("public/")
        }) {
            return Err("WAZ v2 contains legacy package entries".into());
        }
        if let Some(docs) = document.get("docs") {
            let fields = docs.as_object().ok_or("docs must be a table")?;
            for (key, value) in fields {
                let path = value.as_str().ok_or("document locations must be strings")?;
                match key.as_str() {
                    "manual" if path == "docs/manual.md" && files.contains_key(path) => {}
                    "readme" | "changelog" => {
                        let url = url::Url::parse(path)?;
                        if url.scheme() != "https"
                            || url.host_str().is_none()
                            || !url.username().is_empty()
                            || url.password().is_some()
                            || path.chars().any(char::is_whitespace)
                        {
                            return Err(
                                "external document links require HTTPS URLs without credentials"
                                    .into(),
                            );
                        }
                    }
                    _ => return Err("unknown document field or missing docs/manual.md".into()),
                }
            }
        }
        for (name, bytes) in files.iter().filter(|(name, _)| name.starts_with("docs/")) {
            if !name.ends_with(".md") {
                return Err("package manuals must be Markdown files".into());
            }
            std::str::from_utf8(bytes)?;
        }
    } else if files.keys().any(|name| {
        name == "manifest.toml"
            || name.starts_with("wasm/")
            || name.starts_with("web/")
            || name.starts_with("docs/")
    }) {
        return Err("WAZ v1 contains unsupported package entries".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/package/format/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/common/package_format.rs"]
mod test_support;
#[cfg(test)]
pub(crate) use test_support::{runtime_fixture, test_payload_digest, waz_write};

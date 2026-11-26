//! Canonical wire paths shared by routing and per-request authorization.
use super::*;

const CONTACT: &str = "/std/contact";
const MESSAGE: &str = "/std/comm/message";
const GROUP: &str = "/std/comm/group";

fn invalid() -> Error {
    Error::BadRequest("invalid communication resource path".into())
}

pub(crate) fn identity_segment(raw: &str, reserved: &[&str]) -> Result<String> {
    let raw = raw.to_ascii_lowercase();
    let short = raw
        .strip_suffix(".dhttp.net")
        .or_else(|| raw.strip_suffix('~'))
        .unwrap_or(&raw);
    if short.ends_with(".dhttp.net")
        || short.len() + 10 > 253
        || !short.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
    {
        return Err(invalid());
    }
    Ok(if reserved.contains(&short) {
        format!("{short}.dhttp.net")
    } else {
        short.to_owned()
    })
}

fn segment(raw: &str) -> Result<String> {
    let mut bytes = raw.bytes();
    let mut output = String::with_capacity(raw.len());
    while let Some(byte) = bytes.next() {
        let decoded = if byte == b'%' {
            let hex = |c: Option<u8>| c.and_then(|c| (c as char).to_digit(16)).ok_or_else(invalid);
            (hex(bytes.next())? * 16 + hex(bytes.next())?) as u8
        } else {
            byte
        };
        if decoded == b'#' && byte == b'%' {
            output.push_str("%23");
        } else if decoded.is_ascii_alphanumeric() || b"-._~".contains(&decoded) {
            output.push(decoded as char);
        } else {
            return Err(invalid());
        }
    }
    if output.is_empty() || matches!(output.as_str(), "." | "..") {
        return Err(invalid());
    }
    Ok(output)
}

fn conversation(value: &str, reserved: &[&str]) -> Result<String> {
    if let Some((host, topic)) = value.split_once("~%23") {
        if topic.is_empty()
            || topic.len() > 80
            || matches!(topic, "." | "..")
            || !topic
                .bytes()
                .all(|v| v.is_ascii_lowercase() || v.is_ascii_digit() || b"._-".contains(&v))
        {
            return Err(invalid());
        }
        return Ok(format!("{}~%23{topic}", identity_segment(host, &[])?));
    }
    identity_segment(value, reserved)
}

pub(crate) fn canonical(path: &str) -> Result<String> {
    let Some((base, tail)) = [CONTACT, MESSAGE, GROUP].into_iter().find_map(|base| {
        path.strip_prefix(base)
            .filter(|s| s.is_empty() || s.starts_with('/'))
            .map(|s| (base, s))
    }) else {
        return Ok(path.into());
    };
    if tail.contains("//") {
        return Err(invalid());
    }
    let tail = tail.trim_matches('/');
    if tail.is_empty() {
        return Ok(base.into());
    }
    let mut parts: Vec<_> = tail.split('/').map(segment).collect::<Result<_>>()?;
    match base {
        MESSAGE if !matches!(parts[0].as_str(), "list" | "history") => {
            parts[0] = conversation(&parts[0], &["list", "history"])?
        }
        CONTACT if !matches!(parts[0].as_str(), "list" | "requests") => {
            parts[0] = conversation(&parts[0], &["list", "requests"])?
        }
        GROUP => {
            if parts[0].contains("%23") {
                parts[0] = conversation(&parts[0], &[])?;
            }
            if parts.len() >= 3
                && matches!(parts[1].as_str(), "profile" | "member")
                && (parts[2] != "list" || parts[1] == "profile")
            {
                parts[2] = identity_segment(
                    &parts[2],
                    if parts[0].contains("~%23") {
                        &["list"]
                    } else {
                        &[]
                    },
                )?;
            }
        }
        _ => (),
    }
    Ok(format!("{base}/{}", parts.join("/")))
}

pub(super) fn normalize_uri(uri: &http::Uri) -> Result<http::Uri> {
    let path = canonical(uri.path())?;
    if uri.path() == path {
        return Ok(uri.clone());
    }
    let mut parts = uri.clone().into_parts();
    let target = match uri.query() {
        Some(q) => format!("{path}?{q}"),
        None => path,
    };
    parts.path_and_query = Some(target.parse().map_err(|_| invalid())?);
    http::Uri::from_parts(parts).map_err(|_| invalid())
}

fn query(uri: &http::Uri, key: &str) -> Result<Option<String>> {
    let mut matches =
        form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()).filter(|(k, _)| k == key);
    let first = matches.next().map(|(_, v)| v.into_owned());
    if matches.next().is_some() {
        return Err(invalid());
    }
    Ok(first)
}

async fn json_request(request: &mut Request<AxumBody>, limit: usize) -> Result<serde_json::Value> {
    let body = std::mem::replace(request.body_mut(), AxumBody::empty());
    let bytes = axum::body::to_bytes(body, limit)
        .await
        .map_err(|_| Error::BadRequest("invalid bounded request body".into()))?;
    let doc =
        serde_json::from_slice(&bytes).map_err(|_| Error::BadRequest("invalid JSON".into()))?;
    *request.body_mut() = AxumBody::from(bytes);
    Ok(doc)
}

fn wire_name(value: &str, reserved: &[&str]) -> Result<String> {
    conversation(&value.replace("~#", "~%23"), reserved)
}

// Retain the original URI and body for the legacy handler and its receipt.
// Authorization (including a one-shot review) runs once for the business resource.
pub(super) async fn operation(
    request: &mut Request<AxumBody>,
    visitor: Option<&access_control::Visitor>,
    resolves_files: bool,
) -> Result<Option<(Method, String)>> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let local = request
        .extensions()
        .get::<dhttp::Endpoint>()
        .map(|e| e.name().to_owned());
    let actor = visitor.map(|v| v.name());
    let owner = actor.is_some() && actor == local.as_deref();
    let actor_or_local = actor.unwrap_or("");
    match (path.as_str(), &method) {
        (CONTACT, &Method::GET | &Method::PATCH | &Method::DELETE) => {
            if let Some(id) = query(request.uri(), "application_id")? {
                return Ok(Some((method, format!("{CONTACT}/requests/{id}"))));
            }
            if let Some(name) = query(request.uri(), "name")? {
                return Ok(Some((
                    method,
                    format!("{CONTACT}/{}", wire_name(&name, &["list", "requests"])?),
                )));
            }
        }
        (CONTACT, &Method::POST) => {
            let doc = json_request(request, 65536).await?;
            let name = if owner {
                doc["to"].as_str().ok_or_else(invalid)?
            } else {
                actor.ok_or(Error::Denied)?
            };
            return Ok(Some((
                method,
                format!(
                    "{CONTACT}/{}",
                    identity_segment(name, &["list", "requests"])?
                ),
            )));
        }
        (GROUP, &Method::GET) => {
            if let (Some(group), Some(person)) =
                (query(request.uri(), "membership")?, local.as_deref())
            {
                return Ok(Some((
                    method,
                    format!(
                        "{GROUP}/{}/member/{}",
                        wire_name(&group, &[])?,
                        identity_segment(person, &["list"])?
                    ),
                )));
            }
        }
        (GROUP, &Method::POST) => {
            let doc = json_request(request, 3 * 1024 * 1024).await?;
            if doc["type"] == "request_join" {
                let group = wire_name(doc["topic"].as_str().ok_or_else(invalid)?, &[])?;
                let member =
                    identity_segment(local.as_deref().ok_or(Error::MissingHandshake)?, &["list"])?;
                return Ok(Some((
                    Method::PUT,
                    format!("{GROUP}/{group}/member/{member}"),
                )));
            }
        }
        ("/std/comm/message/list", &Method::GET) => {
            if request.headers().get_all(header::ACCEPT).iter().any(|v| {
                v.to_str().is_ok_and(|s| {
                    s.split(',')
                        .any(|m| m.trim().split(';').next() == Some("text/event-stream"))
                })
            }) {
                return Ok(Some((method, format!("{MESSAGE}/history"))));
            }
            if query(request.uri(), "view")?.as_deref() == Some("conversations") {
                return Ok(Some((method, path)));
            }
            if let Some(name) = query(request.uri(), "name")? {
                return Ok(Some((
                    method,
                    format!("{MESSAGE}/{}", wire_name(&name, &["list", "history"])?),
                )));
            }
        }
        (MESSAGE, &Method::POST)
            if !request
                .headers()
                .get(header::CONTENT_TYPE)
                .is_some_and(|v| v.as_bytes().starts_with(b"multipart/")) =>
        {
            let doc = json_request(request, 3 * 1024 * 1024).await?;
            let kind = doc["type"].as_str().unwrap_or("");
            if resolves_files
                && matches!(
                    kind,
                    "file_read"
                        | "file_info"
                        | "download"
                        | "retry"
                        | "upload_status"
                        | "upload_cancel"
                        | "file_save"
                        | "group_attachment_offer"
                )
            {
                return Ok(None);
            }
            let peer = match kind {
                "text" | "image" | "file" => {
                    if let Some(group) = ["from", "to"]
                        .into_iter()
                        .filter_map(|field| doc[field].as_str())
                        .find(|v| v.contains("~#"))
                    {
                        Some(group)
                    } else if owner {
                        doc["to"].as_str()
                    } else {
                        actor
                    }
                }
                "group_history" => doc["topic"].as_str(),
                "group_push" | "broadcast" => doc["from"].as_str(),
                "retry" => doc["to"].as_str(),
                "file_import" | "upload" => doc["peer"].as_str(),
                "file_read" | "file_info" if !owner => {
                    if let (Some(peer), Some(file)) = (actor, doc["id"].as_str()) {
                        let name = identity_segment(peer, &["list", "history"])?;
                        return Ok(Some((
                            if kind == "file_read" {
                                Method::GET
                            } else {
                                Method::HEAD
                            },
                            format!("{MESSAGE}/{name}/file/{}", segment(file)?),
                        )));
                    }
                    None
                }
                _ => None,
            };
            if let Some(peer) = peer.filter(|v| *v != "~") {
                let name = wire_name(peer, &["list", "history"])?;
                let suffix = if matches!(kind, "upload" | "file_import") {
                    "/file"
                } else {
                    ""
                };
                return Ok(Some((
                    if kind == "group_history" {
                        Method::GET
                    } else {
                        method
                    },
                    format!("{MESSAGE}/{name}{suffix}"),
                )));
            }
        }
        // The verified Message resource adapter checks the concrete upload or
        // group-file grant after decoding the bounded multipart metadata.
        (MESSAGE, &Method::POST) if resolves_files => return Ok(None),
        _ => (),
    }
    if let (Some(slug), Some(local)) = (path.strip_prefix("/std/comm/group/"), local.as_deref()) {
        let parts: Vec<_> = slug.split('/').collect();
        if !parts[0].contains("%23") {
            let local = identity_segment(local, &[])?;
            let mut group = wire_name(&format!("{local}~#{}", parts[0]), &[])?;
            let root = format!("{GROUP}/{group}");
            if method == Method::GET || method == Method::DELETE {
                match parts.as_slice() {
                    [_, "member", "list"] => return Ok(Some((method, format!("{root}/member")))),
                    [_, "member", person, field] => {
                        return Ok(Some((
                            method,
                            format!(
                                "{root}/profile/{}/{}",
                                identity_segment(person, &["list"])?,
                                field
                            ),
                        )));
                    }
                    [_, "member"] if actor.is_some() => {
                        return Ok(Some((
                            method,
                            format!(
                                "{root}/member/{}",
                                identity_segment(actor_or_local, &["list"])?
                            ),
                        )));
                    }
                    [_, "avatar"] => return Ok(Some((method, format!("{root}/avatar")))),
                    [_] if method == Method::GET && actor.is_some() => {
                        return Ok(Some((
                            method,
                            format!(
                                "{root}/member/{}",
                                identity_segment(actor_or_local, &["list"])?
                            ),
                        )));
                    }
                    [_] => return Ok(Some((method, root))),
                    _ => (),
                }
            }
            if method == Method::POST {
                let doc = json_request(request, 3 * 1024 * 1024).await?;
                if parts.as_slice() == [parts[0], "broadcast"] {
                    return Ok(Some((Method::POST, format!("{MESSAGE}/{group}"))));
                }
                let action = doc["type"].as_str().unwrap_or("");
                if parts.len() == 1 && action == "create" {
                    return Ok(Some((Method::PUT, root)));
                }
                if parts.len() == 2 && parts[1] == "member" {
                    let (verb, member) = match action {
                        "apply" => (Method::PUT, actor),
                        "invite" => {
                            group = wire_name(doc["group"].as_str().ok_or_else(invalid)?, &[])?;
                            (Method::PUT, doc["name"].as_str())
                        }
                        "approve" | "add" | "resend" => (Method::PUT, doc["name"].as_str()),
                        "dismiss" | "remove" | "cancel" => (Method::DELETE, doc["name"].as_str()),
                        "leave" => (Method::DELETE, actor),
                        "nickname" => (Method::PATCH, actor),
                        _ => return Ok(Some((method, path))),
                    };
                    if let Some(member) = member {
                        return Ok(Some((
                            verb,
                            format!(
                                "{GROUP}/{group}/member/{}",
                                identity_segment(member, &["list"])?
                            ),
                        )));
                    }
                }
            }
        }
    }
    Ok(Some((method, path)))
}

// The old flat RPC grants have a defined meaning only for authenticated inbound
// group operations. Keep the newer concrete operation whenever its method has
// a configured rule on the resource or any ancestor (including deny/review).
pub(super) async fn compatibility_operation(
    access: &access_control::AccessService,
    method: Method,
    path: String,
    visitor: Option<&access_control::Visitor>,
    local: Option<&str>,
) -> Result<(Method, String)> {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    let (Some(visitor), Some(local)) = (visitor, local) else {
        return Ok((method, path));
    };
    let actor = visitor.name();
    let subject = visitor.subject_id().as_bytes().to_vec();
    if actor == local
        || !(path.starts_with("/std/comm/message/") || path.starts_with("/std/comm/group/"))
    {
        return Ok((method, path));
    }
    // A legacy private Message RPC grant included authenticated reads of shared
    // files. Never derive those reads from a new conversation-only POST grant.
    let peer = identity_segment(actor, &["list", "history"])?;
    let peer_file = format!("{MESSAGE}/{peer}/file/");
    if matches!(method, Method::GET | Method::HEAD)
        && path
            .strip_prefix(&peer_file)
            .is_some_and(|id| !id.is_empty() && !id.contains('/'))
    {
        const LEGACY_READ: &str = "
            SELECT EXISTS(SELECT 1 FROM contacts WHERE name=?1 AND subject_id=?4 AND status=2)
            AND EXISTS(SELECT 1 FROM access_rules WHERE api='/std/comm/message' AND method='POST' AND grantee IN (?1,'**','*?'))
            AND NOT EXISTS(SELECT 1 FROM access_rules WHERE method IN (?3,'*') AND grantee IN (?1,'**','*?')
                           AND (api='/' OR api=?2 OR instr(?2,api||'/')=1))
            AND NOT EXISTS(SELECT 1 FROM access_rules WHERE method='POST' AND grantee IN (?1,'**','*?')
                           AND length(api)>length('/std/comm/message') AND (api=?2 OR instr(?2,api||'/')=1)) AS legacy";
        let statement = Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            LEGACY_READ,
            [
                actor.into(),
                path.as_str().into(),
                method.as_str().into(),
                subject.into(),
            ],
        );
        let row = access
            .database()
            .query_one_raw(statement)
            .await
            .map_err(|e| Error::InvalidConfig(e.to_string()))?;
        let legacy = row
            .and_then(|r| r.try_get::<bool>("", "legacy").ok())
            .unwrap_or(false);
        return Ok((if legacy { Method::POST } else { method }, path));
    }
    let actor_short = identity_segment(actor, &[])?;
    let local_short = identity_segment(local, &[])?;
    let (legacy_path, needs_message_grant) = if let Some((group, member)) = path
        .strip_prefix("/std/comm/group/")
        .and_then(|tail| tail.split_once("/member/"))
    {
        let Some((host, topic)) = group.split_once("~%23") else {
            return Ok((method, path));
        };
        let self_member = host == local_short && member == identity_segment(actor, &["list"])?;
        let invitation = host == actor_short && member == identity_segment(local, &["list"])?;
        if !(method == Method::PUT && (self_member || invitation)
            || matches!(method, Method::DELETE | Method::PATCH) && self_member)
        {
            return Ok((method, path));
        }
        (format!("{GROUP}/{topic}/member"), false)
    } else if let Some(tail) = path.strip_prefix("/std/comm/message/") {
        let mut segments = tail.split('/');
        let group = segments.next().unwrap_or_default();
        let Some((host, topic)) = group.split_once("~%23") else {
            return Ok((method, path));
        };
        if host != local_short && host != actor_short {
            return Ok((method, path));
        }
        let compatible = match (segments.next(), segments.next(), segments.next()) {
            (None, None, None) => matches!(method, Method::GET | Method::POST),
            (Some("file"), Some(id), None) if !id.is_empty() => {
                matches!(method, Method::GET | Method::HEAD | Method::PATCH)
            }
            _ => false,
        };
        if !compatible {
            return Ok((method, path));
        }
        (format!("{GROUP}/{topic}/broadcast"), true)
    } else {
        return Ok((method, path));
    };
    const LEGACY_GROUP: &str = "
        SELECT EXISTS(SELECT 1 FROM contacts WHERE name=?1 AND subject_id=?6 AND status=2)
        AND EXISTS(SELECT 1 FROM access_rules WHERE api='/std/comm/group' AND method='POST' AND grantee=?1 AND effect='allow')
        AND (?4=0 OR EXISTS(SELECT 1 FROM access_rules WHERE api IN ('/std/comm/message',?5) AND method='POST' AND grantee=?1 AND effect='allow'))
        AND NOT EXISTS(SELECT 1 FROM access_rules WHERE method IN (?3,'*') AND grantee IN (?1,'**','*?')
                       AND (api='/' OR api=?2 OR instr(?2,api||'/')=1)) AS legacy";
    let statement = Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        LEGACY_GROUP,
        [
            actor.into(),
            path.as_str().into(),
            method.as_str().into(),
            (needs_message_grant as i32).into(),
            format!("{MESSAGE}/{peer}").into(),
            subject.into(),
        ],
    );
    let row = access
        .database()
        .query_one_raw(statement)
        .await
        .map_err(|e| Error::InvalidConfig(e.to_string()))?;
    let legacy = row
        .and_then(|r| r.try_get::<bool>("", "legacy").ok())
        .unwrap_or(false);
    Ok(if legacy {
        (Method::POST, legacy_path)
    } else {
        (method, path)
    })
}

// Canonicalize concrete paths at rule creation and deletion as well as at use.
pub(super) async fn rule_target(request: &mut Request<AxumBody>) -> Result<()> {
    if request.uri().path() == "/std/acl/access"
        && let Some(query) = request.uri().query()
    {
        let mut params = form_urlencoded::Serializer::new(String::new());
        for (k, v) in form_urlencoded::parse(query.as_bytes()) {
            if k == "api" {
                params.append_pair(&k, &canonical(&v)?);
            } else {
                params.append_pair(&k, &v);
            }
        }
        let mut parts = request.uri().clone().into_parts();
        parts.path_and_query = Some(
            format!("/std/acl/access?{}", params.finish())
                .parse()
                .map_err(|_| invalid())?,
        );
        *request.uri_mut() = http::Uri::from_parts(parts).map_err(|_| invalid())?;
    } else if request.uri().path() == "/std/acl/allow"
        && matches!(*request.method(), Method::PATCH | Method::DELETE)
    {
        let mut doc = json_request(request, 65536).await?;
        if let Some(raw) = doc["api"].as_str() {
            let path = canonical(raw)?;
            if path != raw {
                doc["api"] = path.into();
                let bytes = serde_json::to_vec(&doc).map_err(|_| invalid())?;
                request.headers_mut().remove(header::CONTENT_LENGTH);
                *request.body_mut() = AxumBody::from(bytes);
            }
        }
    }
    Ok(())
}

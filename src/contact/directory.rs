//! Persistent local directory projections. DHTTP authentication remains in daccess.
use rusqlite::Connection;

use super::*;

fn group_name(raw: &str) -> Result<String, Failure> {
    let (host, suffix) = raw
        .split_once("~#")
        .ok_or_else(|| fail(400, "invalid group name"))?;
    let host = name(host)?;
    if suffix.is_empty()
        || matches!(suffix, "." | "..")
        || suffix.len() > 80
        || !suffix.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_' || b == b'.'
        })
    {
        return Err(fail(400, "invalid group name"));
    }
    let canonical = format!("{}~#{suffix}", host.trim_end_matches(".dhttp.net"));
    if canonical != raw {
        return Err(fail(400, "invalid group name"));
    }
    Ok(canonical)
}
fn hash(raw: &str) -> Result<(), Failure> {
    if raw.len() != 64
        || !raw
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(fail(400, "invalid owner hash"));
    }
    Ok(())
}
pub(super) fn entry(db: &Connection, name: &str) -> Result<Value, Failure> {
    db.query_row("SELECT bound_subject,alias,avatar,profile_updated_at,created_at,updated_at FROM contact_entries WHERE name=?1",
        [name], |row| Ok(json!({"name":name,"kind":"group","class":"Group",
            "bound_subject":row.get::<_,String>(0)?, "alias":row.get::<_,Option<String>>(1)?,
            "avatar":row.get::<_,Option<String>>(2)?, "profile_updated_at":row.get::<_,i64>(3)?,
            "created_at":row.get::<_,i64>(4)?, "updated_at":row.get::<_,i64>(5)?})))
        .optional().map_err(storage)?.ok_or_else(||fail(404,"contact not found"))
}

pub(super) fn entries(db: &Connection) -> Result<Vec<Value>, Failure> {
    let mut q = db
        .prepare("SELECT name FROM contact_entries ORDER BY name")
        .map_err(storage)?;
    q.query_map([], |row| row.get::<_, String>(0))
        .map_err(storage)?
        .map(|name| entry(db, &name.map_err(storage)?))
        .collect()
}

pub(super) fn put(
    db: &mut Connection,
    doc: &Value,
    service: Option<&(String, String)>,
    _owner: bool,
    time: i64,
) -> Result<Value, Failure> {
    if string(doc, "kind")? != "group" {
        return Err(fail(400, "invalid directory kind"));
    }
    let service = service.ok_or_else(|| fail(403, "local_group_required"))?;
    let provider = service.0.split_once('/').is_some_and(|(author, package)| {
        crate::package::is_standard_author(author) && package == "std/comm/group"
    });
    if !provider {
        return Err(fail(403, "local_group_required"));
    }
    let allowed = [
        "kind",
        "name",
        "avatar",
        "bound_subject",
        "profile_updated_at",
    ];
    if doc.as_object().is_none_or(|v| {
        v.len() != allowed.len() || v.keys().any(|k| !allowed.contains(&k.as_str()))
    }) {
        return Err(fail(400, "invalid group profile fields"));
    }
    let target = group_name(string(doc, "name")?)?;
    let subject = string(doc, "bound_subject")?;
    hash(subject)?;
    let stamp = doc["profile_updated_at"]
        .as_i64()
        .filter(|v| *v > 0)
        .ok_or_else(|| fail(400, "invalid profile_updated_at"))?;
    let avatar = if doc["avatar"].is_null() {
        None
    } else {
        let value = string(doc, "avatar")?;
        if value.len() > 22000
            || !(value.starts_with("data:image/png;base64,")
                || value.starts_with("data:image/webp;base64,"))
        {
            return Err(fail(400, "invalid avatar"));
        }
        Some(value)
    };
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage)?;
    authorize(&tx, service, "group")?;
    let old = tx
        .query_row(
            "SELECT bound_subject,avatar,profile_updated_at FROM contact_entries WHERE name=?1",
            [&target],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(storage)?;
    if let Some((bound, previous, at)) = old {
        if bound != subject {
            return Err(fail(409, "group_binding_conflict"));
        }
        if at == stamp && previous.as_deref() != avatar {
            return Err(fail(409, "group_profile_conflict"));
        }
    }
    tx.execute("INSERT INTO contact_entries(name,bound_subject,avatar,profile_updated_at,created_at,updated_at)
        VALUES(?1,?2,?3,?4,?5,?5) ON CONFLICT(name) DO UPDATE SET avatar=excluded.avatar,
        profile_updated_at=excluded.profile_updated_at,updated_at=excluded.updated_at
        WHERE excluded.profile_updated_at>contact_entries.profile_updated_at",params![target,subject,avatar,stamp,time]).map_err(storage)?;
    let saved = entry(&tx, &target)?;
    tx.commit().map_err(storage)?;
    Ok(saved)
}

pub(super) fn patch(
    db: &Connection,
    target: &str,
    doc: &Value,
    time: i64,
) -> Result<Value, Failure> {
    if doc
        .as_object()
        .is_none_or(|v| v.len() != 1 || !v.contains_key("alias"))
    {
        return Err(fail(400, "expected alias"));
    }
    let alias = match &doc["alias"] {
        Value::Null => None,
        Value::String(s) if s.chars().count() <= 80 && !s.chars().any(char::is_control) => {
            Some(s.as_str()).filter(|s| !s.is_empty())
        }
        _ => return Err(fail(400, "invalid alias")),
    };
    db.execute(
        "UPDATE contact_entries SET alias=?2,updated_at=?3 WHERE name=?1 AND alias IS NOT ?2",
        params![target, alias, time],
    )
    .map_err(storage)?;
    entry(db, target)
}

pub(super) fn authorize(
    db: &Connection,
    service: &(String, String),
    kind: &str,
) -> Result<(), Failure> {
    let allowed:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM directory_service_grants WHERE service_id=?1 AND author_owner_hash=?2 AND kind=?3)",params![service.0,service.1,kind],|row|row.get(0)).map_err(storage)?;
    if !allowed {
        return Err(fail(403, "directory service permission required"));
    }
    Ok(())
}
pub(super) fn grants(db: &mut Connection, doc: &Value) -> Result<Value, Failure> {
    if doc["type"] == "service_grant" {
        let service = string(doc, "service")?;
        let (author, _package) = service
            .split_once('/')
            .ok_or_else(|| fail(400, "invalid service identity"))?;
        if name(author)? != author || crate::package::valid_id(service).is_err() {
            return Err(fail(400, "invalid service identity"));
        }
        let subject = string(doc, "author_owner_hash")?;
        hash(subject)?;
        let kind = string(doc, "kind")?;
        if !["name", "group"].contains(&kind) {
            return Err(fail(400, "invalid directory kind"));
        }
        let allow = doc["allow"]
            .as_bool()
            .ok_or_else(|| fail(400, "invalid grant"))?;
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        if allow {
            tx.execute(
                "INSERT OR IGNORE INTO directory_service_grants VALUES(?1,?2,?3)",
                params![service, subject, kind],
            )
            .map_err(storage)?;
        } else {
            tx.execute("DELETE FROM directory_service_grants WHERE service_id=?1 AND author_owner_hash=?2 AND kind=?3",params![service,subject,kind]).map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
    }
    let mut statement=db.prepare("SELECT service_id,author_owner_hash,kind FROM directory_service_grants ORDER BY service_id,kind").map_err(storage)?;
    let items=statement.query_map([],|row|Ok(json!({"service":row.get::<_,String>(0)?,"author_owner_hash":row.get::<_,String>(1)?,"kind":row.get::<_,String>(2)?}))).map_err(storage)?.collect::<rusqlite::Result<Vec<_>>>().map_err(storage)?;
    Ok(json!({"items":items}))
}

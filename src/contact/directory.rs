//! Persistent local directory projections. DHTTP authentication remains in daccess.
use rusqlite::Connection;

use super::*;

fn group_name(raw: &str) -> Result<String, Failure> {
    let (host, suffix) = raw
        .split_once("~#")
        .ok_or_else(|| fail(400, "invalid group name"))?;
    let host = name(host)?;
    if suffix.is_empty()
        || suffix.len() > 80
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(fail(400, "invalid group name"));
    }
    let canonical = format!("{}~#{suffix}", host.trim_end_matches(".dhttp.net"));
    if canonical != raw {
        return Err(fail(400, "invalid group name"));
    }
    Ok(canonical)
}
fn number(doc: &Value, key: &str) -> Result<i64, Failure> {
    let raw = string(doc, key)?;
    let value = raw
        .parse::<i64>()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| fail(400, format!("invalid {key}")))?;
    if value.to_string() != raw {
        return Err(fail(400, format!("invalid {key}")));
    }
    Ok(value)
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
fn title(raw: &str) -> Result<(), Failure> {
    let (user, domain) = raw
        .split_once('@')
        .ok_or_else(|| fail(400, "invalid title name"))?;
    if raw.len() > 254
        || user.is_empty()
        || domain.is_empty()
        || domain.contains('@')
        || raw.chars().any(|c| c.is_control() || c.is_whitespace())
        || !domain.is_ascii()
    {
        return Err(fail(400, "invalid title name"));
    }
    Ok(())
}

pub(super) fn entry(db: &Connection, name: &str) -> Result<Value, Failure> {
    db.query_row(
            "SELECT name,kind,alias,hidden,nickname,avatar,host,host_owner_hash,member_joined_seq,local_state,bound_name,bound_subject,source_revision,created_at,source_updated_at,updated_at FROM contact_entries WHERE name=?1",
            [name],|r| {
        let kind:String=r.get(1)?;
        Ok(json!({"name":r.get::<_,String>(0)?,"kind":kind,"subject_id":null,
            "requested_access":{},"granted_access":{},"offers":{},"description":"","valid_before":0,
            "class":if kind=="group" {"Group"} else {"Human"},"alias":r.get::<_,Option<String>>(2)?,"hidden":r.get::<_,bool>(3)?,
            "nickname":r.get::<_,Option<String>>(4)?,"avatar":r.get::<_,Option<String>>(5)?,"host":r.get::<_,Option<String>>(6)?,
            "host_owner_hash":r.get::<_,Option<String>>(7)?,"member_joined_seq":r.get::<_,Option<i64>>(8)?.map(|n|n.to_string()),
            "status":r.get::<_,Option<String>>(9)?.unwrap_or_else(||"active".into()),"bound_name":r.get::<_,Option<String>>(10)?,
            "bound_subject":r.get::<_,Option<String>>(11)?,"source_revision":r.get::<_,i64>(12)?.to_string(),
            "created_at":r.get::<_,i64>(13)?,"source_updated_at":r.get::<_,i64>(14)?,"updated_at":r.get::<_,i64>(15)?}))
    }).optional().map_err(storage)?.ok_or_else(||fail(404,"contact not found"))
}

pub(super) fn entries(db: &Connection) -> Result<Vec<Value>, Failure> {
    let names = db
        .prepare("SELECT name FROM contact_entries WHERE hidden=0 ORDER BY name")
        .map_err(storage)?
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(storage)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage)?;
    names.iter().map(|name| entry(db, name)).collect()
}

pub(super) fn put(
    db: &mut Connection,
    doc: &Value,
    service: Option<&(String, String)>,
    owner: bool,
    time: i64,
) -> Result<Value, Failure> {
    let kind = string(doc, "kind")?;
    if kind == "group" {
        if service.is_none() {
            return Err(fail(403, "local_group_required"));
        }
    } else if kind == "title" {
        if !owner {
            return Err(fail(403, "verified owner required"));
        }
    } else {
        return Err(fail(400, "invalid directory kind"));
    }
    let allowed = if kind == "group" {
        vec![
            "kind",
            "name",
            "host",
            "host_owner_hash",
            "nickname",
            "avatar",
            "member_joined_seq",
            "status",
            "source_revision",
            "source_updated_at",
            "restore",
        ]
    } else {
        vec![
            "kind",
            "name",
            "bound_name",
            "bound_subject",
            "source_revision",
            "source_updated_at",
            "restore",
        ]
    };
    if doc
        .as_object()
        .is_none_or(|obj| obj.keys().any(|key| !allowed.contains(&key.as_str())))
    {
        return Err(fail(400, "invalid directory fields"));
    }
    let target = string(doc, "name")?;
    let revision = number(doc, "source_revision")?;
    let restore = doc
        .get("restore")
        .map(|v| v.as_bool().ok_or_else(|| fail(400, "invalid restore")))
        .transpose()?
        .unwrap_or(false);
    let (host, host_hash, joined, state, nickname, avatar, bound_name, bound_subject) = if kind
        == "group"
    {
        group_name(target)?;
        let host = string(doc, "host")?;
        let host_hash = string(doc, "host_owner_hash")?;
        if name(host)? != host
            || target.split_once("~#").unwrap().0 != host.trim_end_matches(".dhttp.net")
        {
            return Err(fail(400, "invalid group host"));
        }
        hash(host_hash)?;
        let joined = number(doc, "member_joined_seq")?;
        let state = string(doc, "status")?;
        if !["active", "leaving", "left", "removed", "dissolved"].contains(&state) {
            return Err(fail(400, "invalid group status"));
        }
        let nickname = string(doc, "nickname")?;
        if !(1..=80).contains(&nickname.chars().count()) || nickname.chars().any(char::is_control) {
            return Err(fail(400, "invalid nickname"));
        }
        let avatar = doc
            .get("avatar")
            .filter(|v| !v.is_null())
            .map(|v| v.as_str().ok_or_else(|| fail(400, "invalid avatar")))
            .transpose()?;
        if avatar.is_some_and(|value| {
            value.len() > 22_000
                || !(value.starts_with("data:image/png;base64,")
                    || value.starts_with("data:image/webp;base64,"))
        }) {
            return Err(fail(400, "invalid avatar"));
        }
        (
            Some(host),
            Some(host_hash),
            Some(joined),
            Some(state),
            Some(nickname),
            avatar,
            None,
            None,
        )
    } else {
        title(target)?;
        let bound = string(doc, "bound_name")?;
        let subject = string(doc, "bound_subject")?;
        if name(bound)? != bound {
            return Err(fail(400, "invalid bound_name"));
        }
        hash(subject)?;
        (
            None,
            None,
            None,
            None,
            None,
            None,
            Some(bound),
            Some(subject),
        )
    };
    let source_time = doc
        .get("source_updated_at")
        .map(|v| {
            v.as_i64()
                .ok_or_else(|| fail(400, "invalid source_updated_at"))
        })
        .transpose()?
        .unwrap_or(time);
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage)?;
    if let Some(service) = service {
        let old: Option<(Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT source_service,source_subject FROM contact_entries WHERE name=?1",
                [target],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(storage)?;
        if old.is_some_and(|(name, subject)| {
            name.as_deref() != Some(&service.0) || subject.as_deref() != Some(&service.1)
        }) {
            return Err(fail(409, "directory source changed"));
        }
    }

    let old = match entry(&tx, target) {
        Ok(value) => Some(value),
        Err((404, _)) => None,
        Err(error) => return Err(error),
    };
    let mut new_period = false;
    if let Some(old) = old {
        if old["kind"] != kind
            || kind == "group"
                && (old["host"] != doc["host"] || old["host_owner_hash"] != doc["host_owner_hash"])
        {
            return Err(fail(409, "group_binding_conflict"));
        }
        let previous = number(&old, "source_revision")?;
        if revision == previous {
            let equal = if kind == "group" {
                old["nickname"] == doc["nickname"]
                    && old["avatar"] == doc["avatar"]
                    && old["member_joined_seq"] == doc["member_joined_seq"]
                    && old["status"] == doc["status"]
            } else {
                old["bound_name"] == doc["bound_name"]
                    && old["bound_subject"] == doc["bound_subject"]
            };
            if !equal
                || doc
                    .get("source_updated_at")
                    .is_some_and(|value| value != &old["source_updated_at"])
            {
                return Err(fail(409, "directory_revision_conflict"));
            }
        }
        if revision <= previous {
            if restore && revision == previous {
                tx.execute(
                    "UPDATE contact_entries SET hidden=0,updated_at=?2 WHERE name=?1",
                    params![target, time],
                )
                .map_err(storage)?;
            }
            if let Some(service) = service {
                tx.execute(
                    "UPDATE contact_entries SET source_service=?2,source_subject=?3 WHERE name=?1",
                    params![target, service.0, service.1],
                )
                .map_err(storage)?;
            }
            let value = entry(&tx, target)?;
            tx.commit().map_err(storage)?;
            return Ok(value);
        }
        new_period = kind == "group"
            && joined
                > old["member_joined_seq"]
                    .as_str()
                    .and_then(|v| v.parse::<i64>().ok());
    }
    tx.execute(
            "INSERT INTO contact_entries(name,kind,nickname,avatar,host,host_owner_hash,member_joined_seq,local_state,bound_name,bound_subject,source_revision,created_at,source_updated_at,updated_at)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?12)
        ON CONFLICT(name) DO UPDATE SET nickname=excluded.nickname,avatar=excluded.avatar,member_joined_seq=excluded.member_joined_seq,local_state=excluded.local_state,
        bound_name=excluded.bound_name,bound_subject=excluded.bound_subject,source_revision=excluded.source_revision,source_updated_at=excluded.source_updated_at,updated_at=excluded.updated_at,
        hidden=CASE WHEN ?14 THEN 0 ELSE contact_entries.hidden END",

        params![target,kind,nickname,avatar,host,host_hash,joined,state,bound_name,bound_subject,revision,time,source_time,restore||new_period]).map_err(storage)?;
    if let Some(service) = service {
        tx.execute(
            "UPDATE contact_entries SET source_service=?2,source_subject=?3 WHERE name=?1",
            params![target, service.0, service.1],
        )
        .map_err(storage)?;
    }
    let value = entry(&tx, target)?;
    tx.commit().map_err(storage)?;
    Ok(value)
}

pub(super) fn patch(
    db: &Connection,
    target: &str,
    doc: &Value,
    time: i64,
) -> Result<Value, Failure> {
    entry(db, target)?;
    if doc
        .as_object()
        .is_none_or(|obj| obj.is_empty() || obj.keys().any(|key| key != "alias" && key != "hidden"))
    {
        return Err(fail(400, "invalid directory settings"));
    }
    let alias = doc
        .get("alias")
        .map(|v| {
            if v.is_null() {
                Ok(None)
            } else {
                v.as_str()
                    .filter(|v| v.chars().count() <= 80 && !v.chars().any(char::is_control))
                    .map(Some)
                    .ok_or_else(|| fail(400, "invalid alias"))
            }
        })
        .transpose()?;
    let hidden = doc
        .get("hidden")
        .map(|v| v.as_bool().ok_or_else(|| fail(400, "invalid hidden")))
        .transpose()?;
    db.execute(
            "UPDATE contact_entries SET alias=CASE WHEN ?2 THEN ?3 ELSE alias END,hidden=coalesce(?4,hidden),updated_at=?5 WHERE name=?1",

        params![target,alias.is_some(),alias.flatten(),hidden,time]).map_err(storage)?;
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
        if !["name", "group", "title"].contains(&kind) {
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

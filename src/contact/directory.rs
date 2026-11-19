//! Persistent local directory projections. DHTTP authentication remains in daccess.
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};

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
fn failure(error: sea_orm::DbErr) -> Failure {
    if matches!(error.sql_err(), Some(sea_orm::SqlErr::UniqueConstraintViolation(_))) {
        fail(409, "contact alias or name conflict")
    } else {
        fail(500, error.to_string())
    }
}

pub(super) fn is_group_provider(id: &str) -> bool {
    id.split_once('/').is_some_and(|(author, package)| {
        crate::package::is_standard_author(author) && package == "std/comm/group"
    })
}

pub(super) async fn entry(db: &impl ConnectionTrait, name: &str) -> Result<Value, Failure> {
    let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT id,name,CAST(subject_id AS TEXT) AS subject,alias,created_at,updated_at FROM contacts WHERE name=?",
        [group_name(name)?.into()])).await.map_err(failure)?.ok_or_else(|| fail(404,"contact not found"))?;
    Ok(json!({"id":row.try_get::<i64>("","id").map_err(failure)?,"kind":"group","class":"Group","name":name,"bound_subject":row.try_get::<String>("","subject").map_err(failure)?,
        "alias":row.try_get::<Option<String>>("","alias").map_err(failure)?,
        "created_at":row.try_get::<i64>("","created_at").map_err(failure)?,
        "updated_at":row.try_get::<i64>("","updated_at").map_err(failure)?}))
}

pub(super) async fn put(access: &AccessService, doc: &Value, service: Option<&(String,String)>, time:i64) -> Result<Value,Failure> {
    if doc["type"]=="remove_group" {
        if !service.is_some_and(|s|is_group_provider(&s.0)) {return Err(fail(403,"local_group_required"));}
        let name=group_name(string(doc,"name")?)?;
        let subject=string(doc,"bound_subject")?;
        hash(subject)?;
        let id=doc["id"].as_i64().filter(|n|*n>0).ok_or_else(||fail(400,"missing contact generation"))?;
        access.database().execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "DELETE FROM contacts WHERE name=? AND subject_id=? AND id=?",
            [name.into(),subject.as_bytes().to_vec().into(),id.into()])).await.map_err(failure)?;
        return Ok(json!({"status":"ok"}));
    }
    if string(doc,"kind")? != "group" { return Err(fail(400,"invalid directory kind")); }
    if !service.is_some_and(|s| is_group_provider(&s.0)) { return Err(fail(403,"local_group_required")); }
    if doc.as_object().is_none_or(|fields| fields.len()!=3) { return Err(fail(400,"invalid group contact fields")); }
    let name=group_name(string(doc,"name")?)?;
    let subject=string(doc,"bound_subject")?;
    hash(subject)?;
    let tx=access.database().begin().await.map_err(failure)?;
    tx.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO contacts(name,subject_id,class,status,created_at,updated_at,expired_after)
         VALUES(?,?,'Group',2,?,?,9223372036854775807) ON CONFLICT(name) DO NOTHING",
        [name.as_str().into(),subject.as_bytes().to_vec().into(),time.into(),time.into()])).await.map_err(failure)?;
    let result=entry(&tx,&name).await?;
    if result["bound_subject"]!=subject {return Err(fail(409,"group_binding_conflict"));}
    tx.commit().await.map_err(failure)?;
    Ok(result)
}

pub(super) async fn patch(access:&AccessService, target:&str, doc:&Value, time:i64) -> Result<Value,Failure> {
    if doc.as_object().is_none_or(|v|v.len()!=1 || !v.contains_key("alias")) {return Err(fail(400,"expected alias"));}
    let alias=match &doc["alias"] {
        Value::Null=>None,
        Value::String(s) if s.chars().count()<=80 && !s.chars().any(char::is_control)=>Some(s.clone()).filter(|v|!v.is_empty()),
        _=>return Err(fail(400,"invalid alias")),
    };
    let tx=access.database().begin().await.map_err(failure)?;
    tx.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE contacts SET alias=?1,updated_at=max(updated_at,?2) WHERE name=?3 AND alias IS NOT ?1",
        [alias.into(),time.into(),group_name(target)?.into()])).await.map_err(failure)?;
    let result=entry(&tx,target).await?;
    tx.commit().await.map_err(failure)?;
    Ok(result)
}

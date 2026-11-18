//! Authenticated contact delivery and reconciliation of outbound requests.
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use access_control::AccessService;
use dhttp_home::identity::IdentityProfile;
use http::Method;
use http_body_util::BodyExt;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use super::{
    Failure,
    access::{contact, record_outbound},
    fail, now,
    storage::{expire_requests, storage},
    string,
};

async fn remote_response(
    endpoint: &dhttp::Endpoint,
    target: &str,
    method: Method,
    path: &str,
    body: Option<&Value>,
    expected: Option<&str>,
) -> Result<(u16, Value, String), Failure> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let uri: http::Uri = format!("https://{target}{path}")
            .parse()
            .map_err(|_| fail(400, "invalid contact URI"))?;
        let mut request = endpoint
            .request(method, uri)
            .header("if-reviewed".parse().unwrap(), "break".parse().unwrap());
        if let Some(expected) = expected {
            let hash = dhttp_home::certificate::OwnerHash::try_from(expected)
                .map_err(|_| fail(409, "invalid contact identity"))?;
            request = request.expect_remote_owner_hash(hash);
        }
        let response = if let Some(body) = body {
            let (mut writer, response) = request
                .header(
                    http::header::CONTENT_TYPE,
                    "application/json".parse().unwrap(),
                )
                .body(dhttp::WndBuf::new(65536))
                .await
                .map_err(|e| fail(502, e.to_string()))?;
            writer
                .write_all(&serde_json::to_vec(body).unwrap())
                .await
                .map_err(|e| fail(502, e.to_string()))?;
            writer
                .shutdown()
                .await
                .map_err(|e| fail(502, e.to_string()))?;
            response.await.map_err(|e| fail(502, e.to_string()))?
        } else {
            let (mut writer, response) = request
                .body(dhttp::WndBuf::new(65536))
                .await
                .map_err(|e| fail(502, e.to_string()))?;
            writer
                .shutdown()
                .await
                .map_err(|e| fail(502, e.to_string()))?;
            response.await.map_err(|e| fail(502, e.to_string()))?
        };
        let authority = response
            .extensions()
            .get::<dhttp::RemoteAuthority>()
            .ok_or_else(|| fail(502, "missing authenticated contact identity"))?;
        if authority.name() != target {
            return Err(fail(502, "contact authority mismatch"));
        }
        let sid =
            dhttp_home::certificate::extract_dhttp_subject_key_identifier(authority.certificates())
                .map_err(|_| fail(502, "invalid contact identity"))?
                .owner_hash()
                .as_str()
                .to_owned();
        let status = response.status().as_u16();
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| fail(502, e.to_string()))?;
            let data = frame
                .into_data()
                .map_err(|_| fail(502, "contact response trailers are unsupported"))?;
            if data.len() > 1024 * 1024 - bytes.len() {
                return Err(fail(502, "contact response too large"));
            }
            bytes.extend_from_slice(&data);
        }
        let value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) if !matches!(status, 200 | 201) => Value::Null,
            Err(_) => return Err(fail(502, "invalid remote JSON")),
        };
        Ok((status, value, sid))
    })
    .await
    .map_err(|_| fail(504, "contact request timed out"))?
}

pub(super) async fn remote(
    endpoint: &dhttp::Endpoint,
    target: &str,
    method: Method,
    path: &str,
    body: Option<&Value>,
    expected: Option<&str>,
) -> Result<(u16, Value, String), Failure> {
    let response = remote_response(endpoint, target, method, path, body, expected).await?;
    if !matches!(response.0, 200 | 201) {
        return Err(fail(response.0, "remote contact operation rejected"));
    }
    Ok(response)
}

fn pending_review(code: u16, body: &Value) -> Result<Option<i64>, Failure> {
    if !((code == 202 && body["status"] == "pending_review")
        || (code == 400 && body["error"] == "need_review"))
    {
        return Ok(None);
    }
    body["review_id"]
        .as_i64()
        .filter(|id| *id > 0)
        .map(Some)
        .ok_or_else(|| fail(400, "review protocol upgrade required"))
}

fn review_progress(code: u16, body: &Value) -> Result<bool, Failure> {
    if code == 404 {
        return Err(fail(410, "review_expired"));
    }
    if code != 200 {
        return Err(fail(
            if matches!(code, 202 | 400) { 409 } else { code },
            "review status unavailable",
        ));
    }
    match body["status"].as_str() {
        Some("pending") => Ok(false),
        Some("allowed" | "consumed") => Ok(true),
        Some("denied") => Err(fail(409, "review_denied")),
        Some("expired") => Err(fail(410, "review_expired")),
        _ => Err(fail(502, "invalid review status")),
    }
}

struct DeliveryJob {
    target: String,
    application_id: String,
    description: String,
    requested: Value,
    offered: Value,
    state: String,
    contact_id: Option<i64>,
    subject: Option<String>,
    created_at: i64,
    review: Option<i64>,
}

pub(crate) async fn contact_jobs(
    profile: &IdentityProfile,
    endpoint: &dhttp::Endpoint,
    access: &Arc<AccessService>,
    db: &Arc<Mutex<Connection>>,
) -> crate::Result<()> {
    deliver(profile, endpoint, access, db)
        .await
        .map_err(|(_, message)| crate::Error::InvalidConfig(message))
}

async fn deliver(
    profile: &IdentityProfile,
    endpoint: &dhttp::Endpoint,
    access: &Arc<AccessService>,
    db: &Arc<Mutex<Connection>>,
) -> Result<(), Failure> {
    let job_db = db.clone();
    let (job,lease)=tokio::task::spawn_blocking(move||{
        let mut db=job_db.lock().map_err(|_|fail(500,"contact database lock poisoned"))?;
        let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
        expire_requests(&tx)?;
        let job=tx.query_row("SELECT target_name, application_id, description, json_extract(requested,'$.access'),
            json_extract(offered,'$.access'), state,contact_id,target_subject,created_at,review_id
            FROM outbound_contact_request WHERE state IN ('queued','pending') AND next_attempt_at<=?1
            AND (lease_until IS NULL OR lease_until<=?1) ORDER BY next_attempt_at,target_name LIMIT 1",[now()],|r| {
            let snapshot=|i|serde_json::from_str(&r.get::<_,String>(i)?).map_err(|e|rusqlite::Error::FromSqlConversionFailure(i,rusqlite::types::Type::Text,Box::new(e)));
            Ok(DeliveryJob{target:r.get(0)?,application_id:r.get(1)?,description:r.get(2)?,requested:snapshot(3)?,offered:snapshot(4)?,
                state:r.get(5)?,contact_id:r.get(6)?,subject:r.get(7)?,created_at:r.get(8)?,review:r.get(9)?})
        }).optional().map_err(storage)?;
        let lease=now()+120;
        if let Some(job)=&job {
            tx.execute("UPDATE outbound_contact_request SET lease_until=?1 WHERE application_id=?2",params![lease,job.application_id]).map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
        Ok::<_,Failure>((job,lease))
    }).await.map_err(|e|fail(500,e.to_string()))??;
    let Some(job) = job else { return Ok(()) };
    let mut review = job.review;
    let mut subject = job.subject.clone();
    let result=async {
        let existing=match contact(access,endpoint,&job.target).await {
            Ok(c)=>Some(c),Err((404,_))=>None,Err(e)=>return Err(e)
        };
        if existing.as_ref().is_some_and(|c|subject.as_deref().is_some_and(|s|c["subject_id"]!=s)
            || matches!(c["status"].as_str(),Some("blocked"|"expired")))
            || job.contact_id.is_some_and(|id|existing.as_ref().is_none_or(|c|c["id"]!=id)) {
            return Err(fail(409,"local contact removed, replaced or blocked"));
        }
        let expected=subject.as_deref().or_else(||existing.as_ref().and_then(|c|c["subject_id"].as_str()));
        if let Some(id)=review {
            let (code,doc,_)=remote_response(endpoint,&job.target,Method::GET,&format!("/std/acl/status?id={id}"),None,expected).await?;
            if !review_progress(code,&doc)? {return Err(fail(202,"need_review"));}
        }
        let (method,path,body)=if job.state=="queued" {
            (Method::POST,"/std/contact".into(),Some(json!({"from":endpoint.name(),"to":job.target,"application_id":job.application_id,"description":job.description,"class":"Human","requested_access":job.requested,"offers":job.offered})))
        } else {
            (Method::GET,format!("/std/contact?application_id={}",job.application_id),None)
        };
        let (status,receipt,sid)=remote_response(endpoint,&job.target,method,&path,body.as_ref(),expected).await?;
        if let Some(id)=pending_review(status,&receipt)? {
            review=Some(id);
            subject=Some(sid);
            return Err(fail(202,"need_review"));
        }
        if !matches!(status,200|201){return Err(fail(status,"remote contact operation rejected"));}
        if receipt["application_id"]!=job.application_id || receipt["name"]!=job.target || receipt["subject_id"]!=sid {
            return Err(fail(409,"receipt does not match authenticated application"));
        }
        let remote_deadline=receipt["valid_before"].as_i64().ok_or_else(||fail(400,"missing receipt deadline"))?;
        let state=string(&receipt,"status")?;
        if !matches!(state,"pending"|"active"|"expired"|"denied"|"revoked"){return Err(fail(400,"invalid application state"));}
        review=None;
        subject=Some(sid.clone());
        // Serialize local changes after network I/O. Dropping this connection rolls back an unfinished transaction.
        let locked=super::database(profile)?;
        locked.execute_batch("BEGIN IMMEDIATE").map_err(storage)?;
        let local_deadline:Option<i64>=locked.query_row("SELECT local_deadline FROM outbound_contact_request
            WHERE target_name=?1 AND application_id=?2 AND state=?3 AND lease_until=?4 AND lease_until>?5",
            params![job.target,job.application_id,job.state,lease,now()],|r|r.get(0)).optional().map_err(storage)?;
        let Some(deadline)=local_deadline else{return Ok(())};
        if deadline.min(remote_deadline)<=now(){return Err(fail(410,"contact request expired"));}
        let mut id=job.contact_id;
        if matches!(state,"pending"|"active"){
            if id.is_none(){
                let previous=match contact(access,endpoint,&job.target).await {Ok(c)=>Some(c),Err((404,_))=>None,Err(e)=>return Err(e)};
                record_outbound(access,&job.target,&sid,job.requested.clone(),job.offered.clone(),deadline.min(remote_deadline),job.created_at).await?;
                if !job.offered.as_object().is_some_and(|o|o.is_empty()){
                    if let Some(previous)=previous {
                        if previous["created_at"]!=job.created_at || previous["offers"]!=job.offered {
                            return Err(fail(409,"contact generation changed during offer"));
                        }
                        let rules=super::access::local_api(access,endpoint,Method::GET,"/std/acl/allow",None).await?;
                        for (path,ops) in job.offered.as_object().unwrap(){
                            let granted=rules[&job.target][path]["allow"].as_array();
                            if ops["allow"].as_array().is_none_or(|methods|methods.iter().any(|m|granted.is_none_or(|a|!a.contains(m)))) {
                                return Err(fail(409,"offered permission was changed; submit a new request"));
                            }
                        }
                    }else{
                        super::access::apply_offered(access,endpoint,&job.target,&sid,&job.offered).await?;
                    }
                }
            }
            let current=contact(access,endpoint,&job.target).await?;
            if current["subject_id"]!=sid || id.is_some_and(|i|current["id"]!=i) || matches!(current["status"].as_str(),Some("expired"|"blocked")){
                return Err(fail(409,"contact generation changed"));
            }
            id=Some(current["id"].as_i64().ok_or_else(||fail(502,"missing contact generation"))?);
        }
        if state=="active" {
            let owner=super::access::owner(endpoint)?;
            let headers=access_control::Headers{method:Method::PATCH,path:"/std/contact".into(),fields:http::HeaderMap::new()};
            if !matches!(access.auth(headers,Some(owner.name()),Some(owner.subject_id())).await.map_err(|e|fail(500,e.to_string()))?,access_control::AuthResult::Allowed) {
                return Err(fail(403,"contact activation denied by ACL"));
            }
            use sea_orm::{ConnectionTrait,DatabaseBackend,Statement};
            // Guard the daccess write itself against concurrent deletion, blocking or replacement.
            let updated=access.database().execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "UPDATE contacts SET status=2,updated_at=? WHERE id=? AND name=? AND subject_id=? AND status IN (0,2,3) AND expired_after>?",
                [now().into(),id.into(),job.target.as_str().into(),sid.as_bytes().to_vec().into(),now().into()]
            )).await.map_err(|e|fail(500,e.to_string()))?;
            if updated.rows_affected()!=1{return Err(fail(409,"contact generation changed"));}
            locked.execute("DELETE FROM outbound_contact_request WHERE application_id=?1 AND lease_until=?2",params![job.application_id,lease]).map_err(storage)?;
        }else{
            locked.execute("UPDATE outbound_contact_request SET state=?1,remote_deadline=?2,contact_id=?3,target_subject=?4,
                review_id=NULL,lease_until=NULL,last_error=?5,next_attempt_at=?6,updated_at=?7 WHERE application_id=?8 AND lease_until=?9",
                params![if state=="revoked"{"failed"}else{state},remote_deadline,id,sid,(state=="revoked").then_some("remote application revoked"),now()+3,now(),job.application_id,lease]).map_err(storage)?;
        }
        locked.execute_batch("COMMIT").map_err(storage)?;
        Ok(())
    }.await;
    if let Err((code, error)) = result {
        let state = match (code, error.as_str()) {
            (_, "review_denied") => "denied",
            (410, _) => "expired",
            (400 | 403 | 404 | 409, _) => "failed",
            _ => job.state.as_str(),
        };
        let conn = db
            .lock()
            .map_err(|_| fail(500, "contact database lock poisoned"))?;
        conn.execute("UPDATE outbound_contact_request SET state=?1,review_id=?2,target_subject=coalesce(target_subject,?3),last_error=?4,
            next_attempt_at=?5,updated_at=?6,lease_until=NULL WHERE application_id=?7 AND lease_until=?8 AND state=?9",
            params![state,review,subject,error,now()+if review.is_some(){30}else{5},now(),job.application_id,lease,job.state]).map_err(storage)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/contact/delivery/tests.rs"]
mod tests;

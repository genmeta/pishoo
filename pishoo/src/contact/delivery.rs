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
    access::{contact, encoded, local_api, record_outbound},
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

#[derive(Clone)]
struct PendingReview {
    id: i64,
    subject: String,
    method: String,
    path: String,
}

fn review_progress(code: u16, body: &Value) -> Result<bool, Failure> {
    if code == 404 {
        return Err(fail(410, "review_expired"));
    }
    if code != 200 {
        return Err(fail(
            if code == 400 { 409 } else { code },
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

fn operation_snapshot(snapshot: Option<&str>, capabilities: &str) -> Result<Value, Failure> {
    if let Some(snapshot) = snapshot {
        let value: Value = serde_json::from_str(snapshot)
            .map_err(|_| fail(500, "invalid stored operation snapshot"))?;
        if !value.is_object() {
            return Err(fail(500, "invalid stored operation snapshot"));
        }
        return Ok(value);
    }
    let groups: Vec<String> =
        serde_json::from_str(capabilities).map_err(|_| fail(500, "invalid stored capabilities"))?;
    if !groups.is_empty() {
        return Err(fail(
            400,
            "legacy contact request has no operation snapshot; submit a new request",
        ));
    }
    Ok(json!({}))
}

struct DeliveryJob {
    id: i64,
    target: String,
    application_id: String,
    description: String,
    requested_caps: String,
    offered_caps: String,
    status: String,
    contact_id: Option<i64>,
    contact_subject: Option<String>,
    requested_access: Option<String>,
    offered_access: Option<String>,
    started_at: i64,
    review: Option<PendingReview>,
}

pub(crate) async fn contact_jobs(
    _profile: &IdentityProfile,
    endpoint: &dhttp::Endpoint,
    access: &Arc<AccessService>,
    db: &Arc<Mutex<Connection>>,
) -> crate::Result<()> {
    let result = deliver(endpoint, access, db).await;
    result.map_err(|(_, message)| crate::Error::InvalidConfig(message))
}
async fn deliver(
    endpoint: &dhttp::Endpoint,
    access: &Arc<AccessService>,
    db: &Arc<Mutex<Connection>>,
) -> Result<(), Failure> {
    let job_db = db.clone();
    let (job, lease) = tokio::task::spawn_blocking(move || {
        let mut db = job_db.lock().map_err(|_| fail(500, "contact database lock poisoned"))?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
        expire_requests(&tx)?;
        let job = tx.query_row(
            "SELECT id,target_name,application_id,description,requested_capabilities,offered_capabilities,status,contact_id,contact_subject,requested_access,offers,created_at,review_id,review_subject,review_method,review_path \
             FROM outbound_contact_request WHERE status IN ('queued','pending','active') AND next_attempt_at<=?1 AND (status='active' OR expired_after>?1) \
             AND (lease_until IS NULL OR lease_until<=?1) ORDER BY next_attempt_at,id LIMIT 1",
            [now()],
            |row| Ok(DeliveryJob {
                id: row.get(0)?, target: row.get(1)?, application_id: row.get(2)?, description: row.get(3)?,
                requested_caps: row.get(4)?, offered_caps: row.get(5)?, status: row.get(6)?, contact_id: row.get(7)?,
                contact_subject: row.get(8)?, requested_access: row.get(9)?, offered_access: row.get(10)?, started_at: row.get(11)?,
                review: match row.get::<_,Option<i64>>(12)? {
                    Some(id) => Some(PendingReview { id, subject:row.get(13)?, method:row.get(14)?, path:row.get(15)? }),
                    None => None,
                },
            }),
        ).optional().map_err(storage)?;
        let lease = now() + 120;
        if let Some(job) = &job {
            tx.execute("UPDATE outbound_contact_request SET lease_until=?1 WHERE id=?2", params![lease, job.id]).map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
        Ok::<_, Failure>((job, lease))
    }).await.map_err(|e| fail(500, e.to_string()))??;
    let Some(DeliveryJob {
        id,
        target,
        application_id,
        description,
        requested_caps,
        offered_caps,
        status,
        contact_id,
        contact_subject,
        requested_access,
        offered_access,
        started_at,
        review,
    }) = job
    else {
        return Ok(());
    };
    let mut next_review = review;
    let outcome=async {
        let (requests, offers) = if status == "queued" {
            (operation_snapshot(requested_access.as_deref(), &requested_caps)?, operation_snapshot(offered_access.as_deref(), &offered_caps)?)
        } else { (json!({}), json!({})) };
        let existing=match contact(access,endpoint,&target).await {Ok(c)=>Some(c),Err((404,_))=>None,Err(e)=>return Err(e)};
        if let Some(id) = contact_id {
            if existing.as_ref().is_none_or(|c| c["id"] != id || c["subject_id"].as_str() != contact_subject.as_deref()
                || matches!(c["status"].as_str(), Some("blocked" | "expired"))) {
                return Err(fail(410, "local contact was removed, replaced or blocked"));
            }
        }
        let expected=existing.as_ref().and_then(|c|c["subject_id"].as_str());
        let (method,path,body)=if status=="queued" {
            (Method::POST,"/std/contact".to_owned(),Some(json!({"from":endpoint.name(),"to":target,"application_id":application_id,"description":description,"class":"Human","requested_access":requests,"offers":offers})))
        } else {(Method::GET,format!("/std/contact?name={}",encoded(endpoint.name())),None)};
        let reviewed_subject = next_review.as_ref().map(|review| review.subject.clone());
        if let Some(review) = &next_review {
            if review.method != method.as_str() || review.path != path
                || expected.is_some_and(|sid| sid != review.subject) {
                return Err(fail(409, "review target or operation changed"));
            }
            let (code, result, _) = remote_response(endpoint, &target, Method::GET,
                &format!("/std/acl/status?id={}",review.id), None, Some(&review.subject)).await?;
            if !review_progress(code, &result)? { return Err(fail(400,"need_review")); }
        }
        let pinned = reviewed_subject.as_deref().or(expected);
        let (code,mut receipt,sid)=remote_response(endpoint,&target,method.clone(),&path,body.as_ref(),pinned).await?;
        if code == 400 && receipt["error"] == "need_review" {
            let review_id = receipt["review_id"].as_i64().filter(|id|*id>0)
                .ok_or_else(||fail(400,"review protocol upgrade required"))?;
            next_review = Some(PendingReview {id:review_id,subject:sid,method:method.to_string(),path});
            return Err(fail(400,"need_review"));
        }
        if !matches!(code,200|201) {return Err(fail(code,"remote contact operation rejected"));}
        next_review = None;
        if status != "queued" {
            let visitor=super::access::owner(endpoint)?;
            if receipt["name"]!=endpoint.name() || receipt["subject_id"].as_str()!=std::str::from_utf8(visitor.subject_id().as_bytes()).ok() {
                return Err(fail(502,"remote contact state belongs to a different subject"));
            }
            receipt["application_id"]=application_id.clone().into();
            receipt["name"]=target.clone().into();
            receipt["subject_id"]=sid.clone().into();
            if receipt["status"]=="blocked" {receipt["status"]="revoked".into();}
            if receipt["status"]=="transfered" {receipt["status"]="pending".into();}
        }
        if receipt["application_id"]!=application_id || receipt["name"]!=target || receipt["subject_id"]!=sid {return Err(fail(502,"contact receipt identity does not match authenticated peer"));}
        if contact_id.is_none() && existing.is_none() && matches!(receipt["status"].as_str(), Some("pending" | "active")) {
            let valid_before = receipt["valid_before"].as_i64().filter(|expiry| *expiry > now()).unwrap_or_else(|| now() + 7 * 86400);
            record_outbound(access, &target, &sid, requests, offers, valid_before, started_at).await?;
        }
        let current = contact(access, endpoint, &target).await?;
        if let Some(id) = contact_id {
            if current["id"] != id || current["subject_id"].as_str() != contact_subject.as_deref() {
                return Err(fail(410, "local contact changed during delivery"));
            }
        }
        let binding_db = db.clone();
        let local_id = current["id"].as_i64().ok_or_else(|| fail(502, "missing contact ID"))?;
        let local_subject = string(&current, "subject_id")?.to_owned();
        tokio::task::spawn_blocking(move || {
            let db = binding_db.lock().map_err(|_| fail(500, "contact database lock poisoned"))?;
            db.execute("UPDATE outbound_contact_request SET contact_id=?1,contact_subject=?2 WHERE id=?3 AND lease_until=?4 AND contact_id IS NULL",params![local_id,local_subject,id,lease]).map_err(storage)?;
            Ok::<_, Failure>(())
        }).await.map_err(|e| fail(500, e.to_string()))??;
        if receipt["status"]=="active" {
            let c=match contact(access,endpoint,&target).await {
                Ok(c)=>c,
                Err((404,_))=> return Err(fail(410,"local contact was removed")),
                Err(e)=>return Err(e),
            };
            if c["subject_id"]!=sid || matches!(c["status"].as_str(),Some("blocked"|"expired")) {return Err(fail(409,"local contact blocks reconciliation"));}
            if matches!(c["status"].as_str(),Some("pending"|"transfered")) {local_api(access,endpoint,Method::PATCH,&format!("/std/contact?name={}",encoded(&target)),Some(&json!({"status":"active"}))).await?;}
        }
        if receipt["status"]=="pending" && receipt["valid_before"].as_i64().is_none_or(|expires|expires<=now()) {return Err(fail(410,"remote contact request expired"));}
        let state=string(&receipt,"status")?;
        if !matches!(state,"pending"|"active"|"denied"|"expired"|"revoked") {return Err(fail(502,"unknown contact status"));}
        if receipt["valid_before"].as_i64().is_none() {return Err(fail(502,"missing contact expiry"));}
        Ok(receipt)
    }.await;
    let db = db.clone();
    tokio::task::spawn_blocking(move|| {
        let mut db=db.lock().map_err(|_|fail(500,"contact database lock poisoned"))?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
        let db = &tx;
        db.execute("UPDATE outbound_contact_request SET review_id=?1,review_subject=?2,review_method=?3,review_path=?4 WHERE id=?5 AND lease_until=?6",
            params![next_review.as_ref().map(|r|r.id), next_review.as_ref().map(|r|r.subject.as_str()), next_review.as_ref().map(|r|r.method.as_str()), next_review.as_ref().map(|r|r.path.as_str()), id,lease]).map_err(storage)?;
        match outcome {
            Ok(receipt)=> {
                db.execute("UPDATE outbound_contact_request SET status=?1,remote_expired_after=?2,expired_after=CASE WHEN ?1='pending' THEN min(delivery_deadline,?2) ELSE expired_after END,last_checked_at=?3,next_attempt_at=?4,attempt_count=attempt_count+1,error_message=NULL,lease_until=NULL,updated_at=?3 WHERE id=?5 AND lease_until=?6",params![string(&receipt,"status")?,receipt["valid_before"].as_i64().unwrap(),now(),now()+if receipt["status"]=="active" {30}else{3},id,lease]).map_err(storage)?;
            }
            Err((code,error))=> {
                db.execute("UPDATE outbound_contact_request SET status=CASE WHEN ?1 THEN 'failed' ELSE status END,attempt_count=attempt_count+CASE WHEN ?2='need_review' THEN 0 ELSE 1 END,error_message=?2,next_attempt_at=?3,lease_until=NULL,last_checked_at=?4,updated_at=?4 WHERE id=?5 AND lease_until=?6",params![error != "need_review" && matches!(code,400|403|404|409|410),error,now()+if error == "need_review" {30} else {5},now(),id,lease]).map_err(storage)?;
            }
        }
        tx.commit().map_err(storage)
    }).await.map_err(|e|fail(500,e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_polling_never_creates_another_approval_or_accepts_a_relationship() {
        assert!(!review_progress(200, &json!({"status":"pending"})).unwrap());
        for status in ["allowed", "consumed"] {
            assert!(review_progress(200, &json!({"status":status})).unwrap());
        }
        assert_eq!(
            review_progress(200, &json!({"status":"denied"})).unwrap_err(),
            fail(409, "review_denied")
        );
        assert_eq!(
            review_progress(200, &json!({"status":"expired"})).unwrap_err(),
            fail(410, "review_expired")
        );
        assert_eq!(
            review_progress(400, &json!({"error":"need_review","review_id":99}))
                .unwrap_err()
                .0,
            409
        );
    }

    #[test]
    fn legacy_capabilities_are_never_reinterpreted() {
        assert!(operation_snapshot(None, r#"["Receive messages"]"#).is_err());
        assert_eq!(operation_snapshot(None, "[]").unwrap(), json!({}));
        let snapshot = r#"{"/std/message":["POST"]}"#;
        assert_eq!(
            operation_snapshot(Some(snapshot), r#"["Receive messages"]"#).unwrap(),
            json!({"/std/message":["POST"]})
        );
    }
}

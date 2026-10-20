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
    access::{contact, encoded, local_api},
    capabilities::{offered, requested, selected},
    fail, now,
    storage::{expire_requests, storage},
    string,
};

pub(super) async fn remote(
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
        let mut request = endpoint.request(method, uri);
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
        if status != 200 && status != 201 {
            return Err(fail(status, "remote contact operation rejected"));
        }
        let value = serde_json::from_slice(&bytes).map_err(|_| fail(502, "invalid remote JSON"))?;
        Ok((status, value, sid))
    })
    .await
    .map_err(|_| fail(504, "contact request timed out"))?
}

pub(crate) async fn contact_jobs(
    profile: &IdentityProfile,
    endpoint: &dhttp::Endpoint,
    access: &Arc<AccessService>,
    db: &Arc<Mutex<Connection>>,
) -> crate::Result<()> {
    let result = deliver(profile, endpoint, access, db).await;
    result.map_err(|(_, message)| crate::Error::InvalidConfig(message))
}
async fn deliver(
    profile: &IdentityProfile,
    endpoint: &dhttp::Endpoint,
    access: &Arc<AccessService>,
    db: &Arc<Mutex<Connection>>,
) -> Result<(), Failure> {
    let job_db = db.clone();
    let (job,lease)=tokio::task::spawn_blocking(move|| {
        let mut db=job_db.lock().map_err(|_|fail(500,"contact database lock poisoned"))?;
        let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
        expire_requests(&tx)?;
        let job:Option<(i64,String,String,String,String,String,String)>=tx.query_row("SELECT id,target_name,application_id,description,requested_capabilities,offered_capabilities,status FROM outbound_contact_requests WHERE status IN ('queued','pending','active') AND next_attempt_at<=?1 AND (status='active' OR expired_after>?1) AND (lease_until IS NULL OR lease_until<=?1) ORDER BY next_attempt_at,id LIMIT 1",[now()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional().map_err(storage)?;
        let lease=now()+120;
        if let Some((id,..))=&job {tx.execute("UPDATE outbound_contact_requests SET lease_until=?1 WHERE id=?2",params![lease,id]).map_err(storage)?;}
        tx.commit().map_err(storage)?;Ok::<_,Failure>((job,lease))
    }).await.map_err(|e|fail(500,e.to_string()))??;
    let Some((id, target, application_id, description, requested_caps, offered_caps, status)) = job
    else {
        return Ok(());
    };
    let outcome=async {
        let requests:Value=serde_json::from_str(&requested_caps).map_err(|_|fail(500,"invalid stored capabilities"))?;
        let offers:Value=serde_json::from_str(&offered_caps).map_err(|_|fail(500,"invalid stored capabilities"))?;
        let selected_requests=selected(profile,endpoint,&requests,&target).await?;
        let selected_offers=selected(profile,endpoint,&offers,endpoint.name()).await?;
        let existing=match contact(access,endpoint,&target).await {Ok(c)=>Some(c),Err((404,_))=>None,Err(e)=>return Err(e)};
        let expected=existing.as_ref().and_then(|c|c["subject_id"].as_str());
        let (method,path,body)=if status=="queued" {
            (Method::POST,"/std/contact".to_owned(),Some(json!({"application_id":application_id,"description":description,"class":"Human","requested_access":requested(&selected_requests),"offers":offered(&selected_offers)})))
        } else {(Method::GET,format!("/std/contact/self?application_id={application_id}"),None)};
        let (_,receipt,sid)=remote(endpoint,&target,method,&path,body.as_ref(),expected).await?;
        if receipt["application_id"]!=application_id || receipt["name"]!=target || receipt["subject_id"]!=sid {return Err(fail(502,"contact receipt identity does not match authenticated peer"));}
        if receipt["status"]=="active" {
            let c=match contact(access,endpoint,&target).await {
                Ok(c)=>c,
                Err((404,_))=> {
                    local_api(access,endpoint,Method::PUT,&format!("/std/contact/{}",encoded(&target)),Some(&json!({"subject_id":sid,"class":"Human","description":"","status":"active","requested_access":requested(&selected_requests),"offers":offered(&selected_offers)}))).await?;
                    contact(access,endpoint,&target).await?
                }
                Err(e)=>return Err(e),
            };
            if c["subject_id"]!=sid || matches!(c["status"].as_str(),Some("blocked"|"expired")) {return Err(fail(409,"local contact blocks reconciliation"));}
            if matches!(c["status"].as_str(),Some("pending"|"transfered")) {local_api(access,endpoint,Method::PATCH,&format!("/std/contact/{}",encoded(&target)),Some(&json!({"status":"active"}))).await?;}
        }
        if receipt["status"]=="pending" && receipt["expired_after"].as_i64().is_none_or(|expires|expires<=now()) {return Err(fail(410,"remote contact request expired"));}
        let state=string(&receipt,"status")?;
        if !matches!(state,"pending"|"active"|"denied"|"expired"|"revoked") {return Err(fail(502,"unknown contact status"));}
        if receipt["expired_after"].as_i64().is_none() {return Err(fail(502,"missing contact expiry"));}
        Ok(receipt)
    }.await;
    let db = db.clone();
    tokio::task::spawn_blocking(move|| {
        let db=db.lock().map_err(|_|fail(500,"contact database lock poisoned"))?;
        match outcome {
            Ok(receipt)=> {
                db.execute("UPDATE outbound_contact_requests SET status=?1,remote_expired_after=?2,expired_after=CASE WHEN ?1='pending' THEN min(delivery_deadline,?2) ELSE expired_after END,last_checked_at=?3,next_attempt_at=?4,attempt_count=attempt_count+1,error_message=NULL,lease_until=NULL,updated_at=?3 WHERE id=?5 AND lease_until=?6",params![string(&receipt,"status")?,receipt["expired_after"].as_i64().unwrap(),now(),now()+if receipt["status"]=="active" {30}else{3},id,lease]).map_err(storage)?;
            }
            Err((code,error))=> {
                db.execute("UPDATE outbound_contact_requests SET status=CASE WHEN ?1 THEN 'failed' ELSE status END,attempt_count=attempt_count+1,error_message=?2,next_attempt_at=?3,lease_until=NULL,last_checked_at=?4,updated_at=?4 WHERE id=?5 AND lease_until=?6",params![matches!(code,400|403|404|410),error,now()+5,now(),id,lease]).map_err(storage)?;
            }
        }
        Ok(())
    }).await.map_err(|e|fail(500,e.to_string()))?
}

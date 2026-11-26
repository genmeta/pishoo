//! Resolve the particular received application, including its applicant and lifetime.
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};

use super::*;

pub(super) fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) async fn decide(
    access: &AccessService,
    id: &str,
    body: &Value,
) -> Result<access_control::Visitor, Failure> {
    let decision = body["status"]
        .as_str()
        .filter(|s| matches!(*s, "active" | "denied"))
        .ok_or_else(|| fail(400, "invalid decision"))?;
    if body.as_object().is_none_or(|v| v.len() != 1) {
        return Err(fail(400, "expected status"));
    }
    let db_error = |e: sea_orm::DbErr| fail(500, e.to_string());
    let tx = access.database().begin().await.map_err(db_error)?;
    let row=tx.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT a.applicant_name,a.applicant_sid,a.status,a.expired_after,c.id AS contact_id,c.status AS relationship
         FROM contact_applications a LEFT JOIN contacts c ON c.name=a.applicant_name AND c.subject_id=a.applicant_sid WHERE a.application_id=?",
        [id.into()])).await.map_err(db_error)?.ok_or_else(||fail(404,"application not found"))?;
    let name: String = row.try_get("", "applicant_name").map_err(db_error)?;
    let sid: Vec<u8> = row.try_get("", "applicant_sid").map_err(db_error)?;
    let state: String = row.try_get("", "status").map_err(db_error)?;
    if state != decision {
        let deadline: i64 = row.try_get("", "expired_after").map_err(db_error)?;
        let relationship: Option<i32> = row.try_get("", "relationship").map_err(db_error)?;
        if state != "pending" || deadline <= now() || !matches!(relationship, Some(0 | 2 | 3)) {
            return Err(fail(409, "application changed"));
        }
        let changed=tx.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "UPDATE contact_applications SET status=? WHERE application_id=? AND status='pending' AND expired_after>unixepoch()",
            [decision.into(),id.into()])).await.map_err(db_error)?;
        if changed.rows_affected() != 1 {
            return Err(fail(409, "application changed"));
        }
        let contact_id: i64 = row.try_get("", "contact_id").map_err(db_error)?;
        if decision == "active" || relationship == Some(0) {
            let changed=tx.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "UPDATE contacts SET status=?,updated_at=unixepoch() WHERE id=? AND subject_id=? AND status IN (0,2,3) AND expired_after>unixepoch()",
                [(if decision=="active"{2}else{4}).into(),contact_id.into(),sid.clone().into()])).await.map_err(db_error)?;
            if changed.rows_affected() != 1 {
                return Err(fail(409, "contact changed"));
            }
        }
    }
    tx.commit().await.map_err(db_error)?;
    Ok(access_control::Visitor::new(
        name,
        access_control::SubjectId::new(sid).map_err(|_| fail(500, "invalid applicant subject"))?,
    ))
}

pub(super) async fn pending_application(
    access: &AccessService,
    name: &str,
    subject: &str,
) -> Result<Option<(String, i64, i64)>, Failure> {
    let row=access.database().query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT application_id,received_at,expired_after FROM contact_applications WHERE applicant_name=? AND applicant_sid=? AND status='pending' AND expired_after>unixepoch() ORDER BY received_at DESC LIMIT 1",
        [name.into(),subject.as_bytes().to_vec().into()])).await.map_err(|e|fail(500,e.to_string()))?;
    row.map(|r| {
        Ok((
            r.try_get("", "application_id")?,
            r.try_get("", "received_at")?,
            r.try_get("", "expired_after")?,
        ))
    })
    .transpose()
    .map_err(|e: sea_orm::DbErr| fail(500, e.to_string()))
}

// daccess already performs payload-hash deduplication, applicant binding and
// relationship updates in the insert transaction. Keep an additional request
// on an active relationship pending in that same transaction. Both wire formats
// read the persisted decision afterwards, including after a lost response.
pub(super) async fn install_application_trigger(access: &AccessService) -> Result<(), Failure> {
    access.database().execute_unprepared(
        "CREATE TRIGGER IF NOT EXISTS contact_requested_permissions_pending
         AFTER INSERT ON contact_applications WHEN NEW.status='active'
           AND EXISTS(SELECT 1 FROM contacts c, json_each(c.requests) r
                      WHERE c.name=NEW.applicant_name AND c.subject_id=NEW.applicant_sid
                        AND c.status IN (2,3) AND json_array_length(r.value)>0)
         BEGIN UPDATE contact_applications SET status='pending' WHERE application_id=NEW.application_id; END"
    ).await.map_err(|e|fail(500,e.to_string()))?;
    Ok(())
}

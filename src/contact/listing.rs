//! Page the authorized directory across the relationship and outgoing-request databases.
use std::time::Duration;

use dhttp_home::identity::IdentityProfile;
use rusqlite::{Connection, OpenFlags, functions::FunctionFlags, params};
use serde_json::{Value, json};

use super::{Failure, directory, fail, integer, page, parameter, storage::storage};

// Keep the sort input narrow. Permission snapshots and application details are
// fetched only after SQLite has selected the names on the requested page.
const DIRECTORY: &str = r#"
WITH directory AS (
    SELECT c.name, coalesce(c.alias,'') AS alias,
           CASE WHEN instr(c.name,'~#')>0 THEN 'Group' ELSE c.class END AS class,c.created_at,
           max(c.updated_at,coalesce(d.updated_at,c.updated_at)) AS updated_at,
           c.description, instr(c.name,'~#')>0 AS is_group, c.id, d.application_id
    FROM contacts c
    LEFT JOIN drafts.outbound_contact_request d
      ON ?1<>'group' AND d.target_name=c.name AND d.contact_id=c.id AND d.target_subject=subject_text(c.subject_id)
    UNION ALL
    SELECT d.target_name,'','Human',d.created_at,d.updated_at,d.description,0,NULL,d.application_id
    FROM drafts.outbound_contact_request d
    WHERE ?1<>'group' AND d.contact_id IS NULL AND NOT EXISTS (SELECT 1 FROM contacts c WHERE c.name=d.target_name)
), filtered AS MATERIALIZED (
    SELECT name,alias,class,created_at,updated_at,is_group,id,application_id FROM directory
    WHERE (?1='all' OR is_group=(?1='group')) AND updated_at>=?2
      AND (?3='' OR
        CASE WHEN ?1='name' THEN instr(lower(name),lower(?3)) OR instr(lower(alias),lower(?3))
             ELSE contains_folded(name,?3) OR contains_folded(alias,?3)
               OR (NOT is_group AND contains_folded(description,?3)) END)
)
"#;

pub(super) fn read(profile: &IdentityProfile, target: &str, kind: &str) -> Result<Value, Failure> {
    let (number, size) = page(target)?;
    let since = parameter(target, "modified_since")?;
    let minimum = integer(target, "modified_since", 0)?;
    if minimum < 0 {
        return Err(fail(400, "invalid modified_since"));
    }
    let search = parameter(target, "search")?.unwrap_or_default();
    let sort = parameter(target, "sort")?.unwrap_or_else(|| "updated_at".into());
    let sort = match sort.as_str() {
        "names" | "name" => "name",
        "alias" | "class" | "created_at" | "updated_at" => &sort,
        _ => return Err(fail(400, "invalid contact sort")),
    };
    let order = parameter(target, "order")?.unwrap_or_else(|| "desc".into());
    if !matches!(order.as_str(), "asc" | "desc") {
        return Err(fail(400, "invalid contact order"));
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    // macOS system temp paths may start with /var (an alias of /private/var).
    // Resolve the identity root, then refuse links in its database paths.
    let root = profile
        .path()
        .canonicalize()
        .map_err(|e| fail(500, e.to_string()))?;
    let mut db = Connection::open_with_flags(root.join(".volume/sys/access/database"), flags)
        .map_err(storage)?;
    db.busy_timeout(Duration::from_secs(2)).map_err(storage)?;
    let mut drafts = url::Url::from_file_path(root.join(".volume/sys/contact/database"))
        .map_err(|_| fail(500, "invalid contact database path"))?;
    drafts.set_query(Some("mode=ro"));
    db.execute("ATTACH DATABASE ? AS drafts", [drafts.as_str()])
        .map_err(storage)?;
    db.create_scalar_function(
        "subject_text",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let bytes: Option<Vec<u8>> = ctx.get(0)?;
            Ok(bytes.map(|bytes| match std::str::from_utf8(&bytes) {
                Ok(s) if !s.chars().any(char::is_control) => s.to_owned(),
                _ => format!(
                    "hex:{}",
                    bytes
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                ),
            }))
        },
    )
    .map_err(storage)?;
    db.create_scalar_function(
        "contains_folded",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            Ok(ctx
                .get::<String>(0)?
                .to_lowercase()
                .contains(&ctx.get::<String>(1)?))
        },
    )
    .map_err(storage)?;
    let needle = if kind == "name" {
        search
    } else {
        search.to_lowercase()
    };
    let tx = db.transaction().map_err(storage)?;
    // Match the expiry transitions of the native daccess list before projecting its rows.
    tx.execute(
        "UPDATE contacts SET status=? WHERE status=0 AND expired_after<=unixepoch()",
        [access_control::ContactStatus::Expired as i32],
    )
    .map_err(storage)?;
    tx.execute("UPDATE contact_applications SET status='expired' WHERE status='pending' AND expired_after<=unixepoch()",[]).map_err(storage)?;
    tx.commit().map_err(storage)?;
    let tx = db.transaction().map_err(storage)?;
    let query = format!(
        r#"{DIRECTORY}, page AS (
        SELECT * FROM filtered ORDER BY {sort} {order},name {order} LIMIT ?4 OFFSET ?5
    )
    SELECT totals.total,p.*,coalesce(c.description,d.description) AS description,subject_text(c.subject_id) AS subject_id,c.alias AS contact_alias,c.status,c.expired_after,c.requests,c.grants,c.offers,
           EXISTS(SELECT 1 FROM contact_applications a WHERE a.applicant_name=p.name AND a.applicant_sid=c.subject_id) AS incoming,
           d.state,d.last_error,min(d.local_deadline,coalesce(d.remote_deadline,d.local_deadline)) AS deadline
    FROM (SELECT count(*) AS total FROM filtered) totals LEFT JOIN page p ON true
    LEFT JOIN contacts c ON c.id=p.id
    LEFT JOIN drafts.outbound_contact_request d ON d.application_id=p.application_id
    ORDER BY p.{sort} {order},p.name {order}"#
    );
    let mut total = 0_i64;
    let mut items = Vec::new();
    {
        let mut stmt = tx.prepare(&query).map_err(storage)?;
        let mut rows = stmt
            .query(params![kind, minimum, needle, size, (number - 1) * size])
            .map_err(storage)?;
        while let Some(row) = rows.next().map_err(storage)? {
            total = row.get("total").map_err(storage)?;
            if row.get_ref("name").map_err(storage)? != rusqlite::types::ValueRef::Null {
                items.push(item(row)?);
            }
        }
    }
    let mut deleted = Vec::new();
    if since.is_some() && kind == "name" {
        let mut stmt=tx.prepare("SELECT name,updated_at FROM contact_deletions WHERE updated_at>=? ORDER BY updated_at,name").map_err(storage)?;
        deleted = stmt
            .query_map([minimum], |r| {
                Ok(json!({"name":r.get::<_,String>(0)?,"updated_at":r.get::<_,i64>(1)?}))
            })
            .map_err(storage)?
            .collect::<rusqlite::Result<_>>()
            .map_err(storage)?;
    }
    tx.commit().map_err(storage)?;
    Ok(json!({"page":number,"page_size":size,"total":total,"items":items,"deleted":deleted}))
}

fn item(row: &rusqlite::Row<'_>) -> Result<Value, Failure> {
    let name: String = row.get("name").map_err(storage)?;
    let id: Option<i64> = row.get("id").map_err(storage)?;
    let alias: Option<String> = row.get("contact_alias").map_err(storage)?;
    let subject: Option<String> = row.get("subject_id").map_err(storage)?;
    let mut doc = json!({"id":id,"name":name,"alias":alias,"class":row.get::<_,String>("class").map_err(storage)?,
        "created_at":row.get::<_,i64>("created_at").map_err(storage)?,"updated_at":row.get::<_,i64>("updated_at").map_err(storage)?});
    if row.get::<_, bool>("is_group").map_err(storage)? {
        directory::group_name(&name)?;
        doc["kind"] = json!("group");
        doc["class"] = json!("Group");
        doc["bound_subject"] = json!(subject);
        return Ok(doc);
    }
    doc["kind"] = json!("name");
    doc["subject_id"] = json!(subject);
    doc["description"] = json!(row.get::<_, String>("description").map_err(storage)?);
    doc["incoming"] = json!(row.get::<_, bool>("incoming").map_err(storage)?);
    let status = row
        .get::<_, Option<i32>>("status")
        .map_err(storage)?
        .unwrap_or(0);
    doc["status"] = json!(
        access_control::ContactStatus::try_from(status)
            .map_err(|_| fail(500, "invalid contact status"))?
    );
    doc["valid_before"] = json!(
        row.get::<_, Option<i64>>("expired_after")
            .map_err(storage)?
            .or(row.get::<_, Option<i64>>("deadline").map_err(storage)?)
    );
    for (field, column) in [
        ("requested_access", "requests"),
        ("granted_access", "grants"),
        ("offers", "offers"),
    ] {
        doc[field] = match row.get::<_, Option<String>>(column).map_err(storage)? {
            Some(raw) => serde_json::from_str(&raw).map_err(|e| fail(500, e.to_string()))?,
            None => json!({}),
        };
    }
    if let Some(application) = row
        .get::<_, Option<String>>("application_id")
        .map_err(storage)?
    {
        doc["application_id"] = json!(application);
        doc["delivery_status"] = json!(row.get::<_, String>("state").map_err(storage)?);
        doc["delivery_error"] = json!(
            row.get::<_, Option<String>>("last_error")
                .map_err(storage)?
        );
    }
    Ok(doc)
}

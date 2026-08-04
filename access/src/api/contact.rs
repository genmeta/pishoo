use std::collections::BTreeMap;

use sea_orm::{
    ConnectionTrait, DatabaseBackend, DbErr, Order, Statement, TransactionTrait, TryGetable,
};
use serde::{Deserialize, Serialize};

use super::{Page, default_order, deserialize_order, order_sql, pagination};
use crate::{AccessService, Effect, Grantee, Method, SubjectId, policy::database_api};

pub type RequestedAccess = BTreeMap<String, Vec<Method>>;
pub type GrantedAccess = BTreeMap<String, GrantedMethods>;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GrantedMethods {
    #[serde(default)]
    pub allow: Vec<Method>,
    #[serde(default)]
    pub review: Vec<Method>,
    #[serde(default)]
    pub deny: Vec<Method>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewContact {
    pub name: String,
    pub subject_id: SubjectId,
    pub class: String,
    pub description: Option<String>,
    pub requested_access: RequestedAccess,
    pub granted_access: GrantedAccess,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContactPatch {
    LocalApproval,
    GrantedUpdate { granted_access: GrantedAccess },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContactRecord {
    pub name: String,
    pub subject_id: SubjectId,
    pub class: String,
    pub description: Option<String>,
    pub status: i32,
    pub requested_access: RequestedAccess,
    pub granted_access: GrantedAccess,
}

#[derive(Deserialize)]
pub(crate) struct NewContactBody {
    name: String,
    subject_id: String,
    #[serde(default)]
    class: String,
    description: Option<String>,
    #[serde(default)]
    requested_access: RequestedAccess,
    #[serde(default)]
    granted_access: GrantedAccess,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContactPatchBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    granted_access: Option<GrantedAccess>,
}

#[derive(Serialize)]
pub(crate) struct ContactBody {
    name: String,
    subject_id: String,
    class: String,
    description: Option<String>,
    status: i32,
    requested_access: RequestedAccess,
    granted_access: GrantedAccess,
}

#[derive(Deserialize)]
pub(crate) struct DeleteContactsBody {
    names: Vec<String>,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Sort {
    #[serde(alias = "names")]
    Name,
    Alias,
    #[default]
    UpdatedAt,
    CreatedAt,
    Class,
}

impl Sort {
    fn column(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Alias => "alias",
            Self::UpdatedAt => "updated_at",
            Self::CreatedAt => "created_at",
            Self::Class => "class",
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct ContactListQuery {
    page: Option<u64>,
    page_size: Option<u64>,
    #[serde(default)]
    sort: Sort,
    #[serde(default = "default_order", deserialize_with = "deserialize_order")]
    order: Order,
}

impl AccessService {
    pub async fn create_contact(&self, contact: NewContact) -> Result<(), DbErr> {
        let requests = encode_requested_access(&contact.requested_access)?;
        let grants = encode_granted_access(&contact.granted_access)?;
        let transaction = self.db.begin().await?;
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                r#"INSERT INTO contacts
                   (name, subject_id, class, grants, requests, description, status, updated_at, created_at)
                   VALUES (?, ?, ?, ?, ?, ?, 0,
                           CAST(strftime('%s', 'now') AS INTEGER),
                           CAST(strftime('%s', 'now') AS INTEGER))"#,
                [
                    contact.name.clone().into(),
                    contact.subject_id.as_bytes().to_vec().into(),
                    contact.class.into(),
                    grants.into(),
                    requests.into(),
                    contact.description.into(),
                ],
            ))
            .await?;
        transaction.commit().await?;
        self.contacts
            .write()
            .await
            .insert(contact.name, contact.subject_id);
        Ok(())
    }

    pub async fn contact(&self, name: &str) -> Result<ContactRecord, DbErr> {
        let row = self
            .db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT name, subject_id, class, description, status, requests, grants FROM contacts WHERE name = ?",
                [name.into()],
            ))
            .await?
            .ok_or_else(|| DbErr::RecordNotFound(format!("contact {name:?}")))?;
        decode_contact(row)
    }

    async fn list_contacts(
        &self,
        offset: i64,
        limit: i64,
        sort: Sort,
        order: Order,
    ) -> Result<(u64, Vec<ContactRecord>), DbErr> {
        let total = self
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS total FROM contacts",
            ))
            .await?
            .expect("aggregate query always returns one row");
        let total = i64::try_get(&total, "", "total")? as u64;
        let tie_breaker = if matches!(sort, Sort::Name) {
            ""
        } else {
            ", name ASC"
        };
        let sql = format!(
            "SELECT name, subject_id, class, description, status, requests, grants FROM contacts \
             ORDER BY {} {}{} LIMIT ? OFFSET ?",
            sort.column(),
            order_sql(&order),
            tie_breaker,
        );
        let contacts = self
            .db
            .query_all_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                sql,
                [limit.into(), offset.into()],
            ))
            .await?
            .into_iter()
            .map(decode_contact)
            .collect::<Result<_, _>>()?;
        Ok((total, contacts))
    }

    pub async fn patch_contact(&self, name: &str, patch: ContactPatch) -> Result<(), DbErr> {
        let transaction = self.db.begin().await?;
        match patch {
            ContactPatch::GrantedUpdate { granted_access } => {
                let grants = encode_granted_access(&granted_access)?;
                let result = transaction
                    .execute_raw(Statement::from_sql_and_values(
                        DatabaseBackend::Sqlite,
                        "UPDATE contacts SET grants = ?, status = 2, updated_at = CAST(strftime('%s', 'now') AS INTEGER) WHERE name = ? AND status IN (0, 1, 2)",
                        [grants.into(), name.into()],
                    ))
                    .await?;
                require_contact(result.rows_affected(), name)?;
            }
            ContactPatch::LocalApproval => {
                let result = transaction
                    .execute_raw(Statement::from_sql_and_values(
                        DatabaseBackend::Sqlite,
                        "UPDATE contacts SET status = 1, updated_at = CAST(strftime('%s', 'now') AS INTEGER) WHERE name = ? AND status IN (0, 1, 2)",
                        [name.into()],
                    ))
                    .await?;
                require_contact(result.rows_affected(), name)?;
            }
        }
        transaction.commit().await?;
        Ok(())
    }

    async fn activate_contact(&self, name: &str) -> Result<(), DbErr> {
        let result = self
            .db
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE contacts SET status = 2, updated_at = CAST(strftime('%s', 'now') AS INTEGER) WHERE name = ? AND status = 1",
                [name.into()],
            ))
            .await?;
        require_contact(result.rows_affected(), name)
    }

    pub async fn delete_contact(&self, name: &str) -> Result<(), DbErr> {
        self.delete_contacts(&[name.to_owned()]).await
    }

    pub async fn delete_contacts(&self, names: &[String]) -> Result<(), DbErr> {
        let transaction = self.db.begin().await?;
        let mut removed_rules = Vec::new();
        for name in names {
            removed_rules.push((name.clone(), query_exact_rules(&transaction, name).await?));
            let result = transaction
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "DELETE FROM contacts WHERE name = ?",
                    [name.as_str().into()],
                ))
                .await?;
            require_contact(result.rows_affected(), name)?;
        }
        transaction.commit().await?;
        let mut contacts = self.contacts.write().await;
        for name in names {
            contacts.remove(name);
        }
        drop(contacts);
        let mut policies = self.policies.write().await;
        for (name, rules) in removed_rules {
            let grantee = Grantee::One(name);
            for (method, api) in rules {
                policies.remove(method, &api, grantee.clone());
            }
        }
        Ok(())
    }
}

fn require_contact(rows_affected: u64, name: &str) -> Result<(), DbErr> {
    (rows_affected != 0)
        .then_some(())
        .ok_or_else(|| DbErr::RecordNotFound(format!("contact {name:?}")))
}

async fn query_exact_rules(
    db: &impl ConnectionTrait,
    name: &str,
) -> Result<Vec<(Method, String)>, DbErr> {
    db.query_all_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT method, api FROM access_rules WHERE grantee = ?",
        [name.into()],
    ))
    .await?
    .into_iter()
    .map(|row| {
        Ok((
            String::try_get(&row, "", "method")?.parse()?,
            String::try_get(&row, "", "api")?,
        ))
    })
    .collect()
}

async fn collect_granted_access(
    db: &impl ConnectionTrait,
    name: &str,
) -> Result<GrantedAccess, DbErr> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT method, api, effect FROM access_rules \
             WHERE grantee_type = 0 AND grantee = ? \
             ORDER BY api, effect, method",
            [name.into()],
        ))
        .await?;
    let mut access = GrantedAccess::new();
    for row in rows {
        let method = String::try_get(&row, "", "method")?.parse()?;
        let api = String::try_get(&row, "", "api")?;
        let effect = String::try_get(&row, "", "effect")?.parse()?;
        let methods = access.entry(api).or_default();
        match effect {
            Effect::Allow => methods.allow.push(method),
            Effect::Review => methods.review.push(method),
            Effect::Deny => methods.deny.push(method),
        }
    }
    Ok(access)
}

fn normalize_requested_access(access: &RequestedAccess) -> Result<RequestedAccess, DbErr> {
    let mut normalized = RequestedAccess::new();
    for (api, source) in access {
        if !api.starts_with('/') {
            return Err(DbErr::Type(format!("invalid access API {api:?}")));
        }
        let api = database_api(api);
        let methods = normalized.entry(api.clone()).or_default();
        for method in source {
            if methods.contains(method) {
                return Err(DbErr::Type(format!(
                    "duplicate requested access {} {api:?}",
                    method.to_string()
                )));
            }
            methods.push(method.clone());
        }
    }
    Ok(normalized)
}

fn normalize_granted_access(access: &GrantedAccess) -> Result<GrantedAccess, DbErr> {
    let mut normalized = GrantedAccess::new();
    for (api, source) in access {
        if !api.starts_with('/') {
            return Err(DbErr::Type(format!("invalid access API {api:?}")));
        }
        let api = database_api(api);
        let methods = normalized.entry(api.clone()).or_default();
        for (effect, source) in [
            (Effect::Allow, &source.allow),
            (Effect::Review, &source.review),
            (Effect::Deny, &source.deny),
        ] {
            for method in source {
                if methods.allow.contains(method)
                    || methods.review.contains(method)
                    || methods.deny.contains(method)
                {
                    return Err(DbErr::Type(format!(
                        "duplicate granted access {} {api:?}",
                        method.to_string()
                    )));
                }
                match effect {
                    Effect::Allow => methods.allow.push(method.clone()),
                    Effect::Review => methods.review.push(method.clone()),
                    Effect::Deny => methods.deny.push(method.clone()),
                }
            }
        }
    }
    Ok(normalized)
}

fn encode_requested_access(access: &RequestedAccess) -> Result<String, DbErr> {
    serde_json::to_string(&normalize_requested_access(access)?)
        .map_err(|error| DbErr::Type(error.to_string()))
}

fn encode_granted_access(access: &GrantedAccess) -> Result<String, DbErr> {
    serde_json::to_string(&normalize_granted_access(access)?)
        .map_err(|error| DbErr::Type(error.to_string()))
}

fn decode_requested_access(value: String) -> Result<RequestedAccess, DbErr> {
    let access = serde_json::from_str(&value).map_err(|error| DbErr::Type(error.to_string()))?;
    normalize_requested_access(&access)
}

fn decode_granted_access(value: String) -> Result<GrantedAccess, DbErr> {
    let access = serde_json::from_str(&value).map_err(|error| DbErr::Type(error.to_string()))?;
    normalize_granted_access(&access)
}

fn decode_contact(row: sea_orm::QueryResult) -> Result<ContactRecord, DbErr> {
    let subject_id = SubjectId::new(Vec::<u8>::try_get(&row, "", "subject_id")?)
        .map_err(|_| DbErr::Type(String::from("contact subject_id is invalid")))?;
    Ok(ContactRecord {
        name: String::try_get(&row, "", "name")?,
        subject_id,
        class: String::try_get(&row, "", "class")?,
        description: Option::<String>::try_get(&row, "", "description")?,
        status: i32::try_get(&row, "", "status")?,
        requested_access: decode_requested_access(String::try_get(&row, "", "requests")?)?,
        granted_access: decode_granted_access(String::try_get(&row, "", "grants")?)?,
    })
}

pub(crate) async fn create(
    axum::extract::State(state): axum::extract::State<crate::api::ApiState>,
    axum::Json(body): axum::Json<NewContactBody>,
) -> Result<http::StatusCode, crate::api::ApiError> {
    if body.name == state.owner_name {
        return Err(crate::api::bad_request(
            "profile owner cannot be created as a contact",
        ));
    }
    let contact = NewContact {
        name: body.name,
        subject_id: SubjectId::new(body.subject_id.into_bytes()).map_err(|_| {
            (
                http::StatusCode::BAD_REQUEST,
                String::from("invalid subject_id"),
            )
        })?,
        class: body.class,
        description: body.description,
        requested_access: validate_requested_access(body.requested_access)?,
        granted_access: validate_granted_access(body.granted_access)?,
    };
    state
        .service
        .create_contact(contact)
        .await
        .map_err(crate::api::database)?;
    Ok(http::StatusCode::CREATED)
}

pub(crate) async fn list(
    axum::extract::State(state): axum::extract::State<crate::api::ApiState>,
    axum::extract::Query(query): axum::extract::Query<ContactListQuery>,
) -> Result<axum::Json<Page<Vec<ContactBody>>>, crate::api::ApiError> {
    let (page, page_size, offset, limit) = pagination(query.page, query.page_size)?;
    let (total, contacts) = state
        .service
        .list_contacts(offset, limit, query.sort, query.order)
        .await
        .map_err(crate::api::database)?;
    Ok(axum::Json(Page {
        items: contacts.into_iter().map(ContactBody::from).collect(),
        total,
        page,
        page_size,
    }))
}

pub(crate) async fn get(
    axum::extract::State(state): axum::extract::State<crate::api::ApiState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<axum::Json<ContactBody>, crate::api::ApiError> {
    state
        .service
        .contact(&name)
        .await
        .map(ContactBody::from)
        .map(axum::Json)
        .map_err(crate::api::database)
}

pub(crate) async fn patch(
    axum::extract::State(state): axum::extract::State<crate::api::ApiState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    visitor: Option<axum::Extension<crate::Visitor>>,
    axum::Json(body): axum::Json<ContactPatchBody>,
) -> Result<http::StatusCode, crate::api::ApiError> {
    let visitor = visitor
        .as_ref()
        .map(|identity| identity.name())
        .ok_or_else(|| crate::api::bad_request("verified visitor identity is required"))?;

    if visitor == state.owner_name {
        if body.granted_access.is_some() {
            return Err(crate::api::bad_request(
                "local approval must not include granted_access",
            ));
        }
        let contact = state
            .service
            .contact(&name)
            .await
            .map_err(crate::api::database)?;
        if !matches!(contact.status, 0 | 1 | 2) {
            return Err((
                http::StatusCode::CONFLICT,
                String::from("contact cannot be approved in its current state"),
            ));
        }
        state
            .service
            .patch_contact(&name, ContactPatch::LocalApproval)
            .await
            .map_err(crate::api::database)?;
        let granted_access = collect_granted_access(&state.service.db, &name)
            .await
            .map_err(crate::api::database)?;
        let notification = serde_json::to_vec(&ContactPatchBody {
            granted_access: Some(granted_access),
        })
        .map_err(crate::api::internal)?;
        let notifier = state.notifier.as_ref().ok_or_else(|| {
            (
                http::StatusCode::SERVICE_UNAVAILABLE,
                String::from("contact notification is unavailable"),
            )
        })?;
        notifier
            .granted_update(&name, notification)
            .await
            .map_err(|error| (http::StatusCode::BAD_GATEWAY, error.to_string()))?;
        state
            .service
            .activate_contact(&name)
            .await
            .map_err(crate::api::database)?;
    } else if visitor == name {
        let granted_access = body
            .granted_access
            .ok_or_else(|| crate::api::bad_request("contact update requires granted_access"))?;
        state
            .service
            .patch_contact(
                &name,
                ContactPatch::GrantedUpdate {
                    granted_access: validate_granted_access(granted_access)?,
                },
            )
            .await
            .map_err(crate::api::database)?;
    } else {
        return Err(crate::api::bad_request(
            "visitor must be the profile owner or the contact being updated",
        ));
    };
    Ok(http::StatusCode::NO_CONTENT)
}

pub(crate) async fn delete(
    axum::extract::State(state): axum::extract::State<crate::api::ApiState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<http::StatusCode, crate::api::ApiError> {
    let _change = state.policy_changes.lock().await;
    state
        .service
        .validate_admin_contact_removal(&state.owner_name, std::slice::from_ref(&name))
        .await
        .map_err(crate::api::database)?;
    state
        .service
        .delete_contact(&name)
        .await
        .map_err(crate::api::database)?;
    Ok(http::StatusCode::NO_CONTENT)
}

pub(crate) async fn delete_many(
    axum::extract::State(state): axum::extract::State<crate::api::ApiState>,
    axum::Json(body): axum::Json<DeleteContactsBody>,
) -> Result<http::StatusCode, crate::api::ApiError> {
    let _change = state.policy_changes.lock().await;
    state
        .service
        .validate_admin_contact_removal(&state.owner_name, &body.names)
        .await
        .map_err(crate::api::database)?;
    state
        .service
        .delete_contacts(&body.names)
        .await
        .map_err(crate::api::database)?;
    Ok(http::StatusCode::NO_CONTENT)
}

fn validate_requested_access(
    access: RequestedAccess,
) -> Result<RequestedAccess, crate::api::ApiError> {
    normalize_requested_access(&access).map_err(crate::api::database)
}

fn validate_granted_access(access: GrantedAccess) -> Result<GrantedAccess, crate::api::ApiError> {
    normalize_granted_access(&access).map_err(crate::api::database)
}

impl From<ContactRecord> for ContactBody {
    fn from(value: ContactRecord) -> Self {
        Self {
            name: value.name,
            subject_id: String::from_utf8_lossy(value.subject_id.as_bytes()).into_owned(),
            class: value.class,
            description: value.description,
            status: value.status,
            requested_access: value.requested_access,
            granted_access: value.granted_access,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use http_body_util::BodyExt;
    use radix_trie::TrieCommon;
    use sea_orm::{ConnectionTrait, Database, TryGetable};
    use tower::ServiceExt;

    use super::*;

    async fn service() -> AccessService {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute_unprepared(include_str!("../migrations/0.sql"))
            .await
            .unwrap();
        AccessService::new(db)
    }

    struct RecordingNotifier {
        service: Arc<AccessService>,
        fail: bool,
        notifications: Mutex<Vec<(String, i32, ContactPatchBody)>>,
    }

    impl crate::api::ContactNotifier for RecordingNotifier {
        fn granted_update<'a>(
            &'a self,
            contact: &'a str,
            body: Vec<u8>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), crate::api::NotifyError>> + Send + 'a>,
        > {
            Box::pin(async move {
                let status = self.service.contact(contact).await?.status;
                let body = serde_json::from_slice(&body)?;
                self.notifications
                    .lock()
                    .unwrap()
                    .push((contact.to_owned(), status, body));
                if self.fail {
                    Err(Box::new(std::io::Error::other("peer rejected update"))
                        as crate::api::NotifyError)
                } else {
                    Ok(())
                }
            })
        }
    }

    fn request(
        method: http::Method,
        uri: &str,
        body: &'static str,
    ) -> http::Request<axum::body::Body> {
        http::Request::builder()
            .method(method)
            .uri(uri)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body))
            .unwrap()
    }

    fn as_visitor(
        mut request: http::Request<axum::body::Body>,
        name: &str,
    ) -> http::Request<axum::body::Body> {
        request.extensions_mut().insert(crate::Visitor::new(
            name,
            SubjectId::new(b"verified-subject".to_vec()).unwrap(),
        ));
        request
    }

    fn requested(api: &str) -> RequestedAccess {
        [(api.into(), vec![Method::Specified(http::Method::GET)])].into()
    }

    fn granted(api: &str, effect: Effect) -> GrantedAccess {
        let method = Method::Specified(http::Method::GET);
        let methods = match effect {
            Effect::Allow => GrantedMethods {
                allow: vec![method],
                review: Vec::new(),
                deny: Vec::new(),
            },
            Effect::Review => GrantedMethods {
                allow: Vec::new(),
                review: vec![method],
                deny: Vec::new(),
            },
            Effect::Deny => GrantedMethods {
                allow: Vec::new(),
                review: Vec::new(),
                deny: vec![method],
            },
        };
        [(api.into(), methods)].into()
    }

    #[test]
    fn access_shapes_serialize_as_aggregated_maps() {
        let requested: RequestedAccess = [
            (
                String::from("/api/1"),
                vec![
                    Method::Specified(http::Method::GET),
                    Method::Specified(http::Method::POST),
                ],
            ),
            (String::from("/api/2"), vec![Method::Unspecified]),
        ]
        .into();
        assert_eq!(
            serde_json::to_value(requested).unwrap(),
            serde_json::json!({"/api/1":["GET","POST"],"/api/2":["*"]})
        );
        let granted: GrantedAccess = [(
            String::from("/api/1"),
            GrantedMethods {
                allow: vec![Method::Specified(http::Method::GET)],
                review: vec![Method::Specified(http::Method::POST)],
                deny: vec![Method::Specified(http::Method::DELETE)],
            },
        )]
        .into();
        assert_eq!(
            serde_json::to_value(granted).unwrap(),
            serde_json::json!({"/api/1":{"allow":["GET"],"review":["POST"],"deny":["DELETE"]}})
        );
    }

    fn contact() -> NewContact {
        NewContact {
            name: "alice.example".into(),
            subject_id: SubjectId::new([1]).unwrap(),
            class: "human".into(),
            description: Some("Alice".into()),
            requested_access: requested("/requested"),
            granted_access: granted("/granted", Effect::Allow),
        }
    }

    #[tokio::test]
    async fn create_stores_pending_contact_without_rules() {
        let service = service().await;
        service.create_contact(contact()).await.unwrap();

        let stored = service.contact("alice.example").await.unwrap();
        assert_eq!(stored.status, 0);
        assert_eq!(stored.class, "human");
        assert_eq!(stored.requested_access, requested("/requested"));
        assert_eq!(stored.granted_access, granted("/granted", Effect::Allow));
        let count = service
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) count FROM access_rules",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(i64::try_get(&count, "", "count").unwrap(), 0);
    }

    #[tokio::test]
    async fn duplicate_create_and_invalid_api_leave_memory_unchanged() {
        let service = service().await;
        service.create_contact(contact()).await.unwrap();
        assert!(service.create_contact(contact()).await.is_err());
        let mut invalid = contact();
        invalid.name = "invalid.example".into();
        invalid.requested_access = requested("relative");
        assert!(service.create_contact(invalid).await.is_err());
        assert_eq!(service.contacts.read().await.len(), 1);
    }

    #[tokio::test]
    async fn local_approval_marks_contact_syncing_without_changing_rules() {
        let service = service().await;
        service.create_contact(contact()).await.unwrap();
        service
            .set_policy(
                Method::Specified(http::Method::GET),
                "/local",
                Effect::Review,
                Grantee::One("alice.example".into()),
            )
            .await
            .unwrap();
        service
            .patch_contact("alice.example", ContactPatch::LocalApproval)
            .await
            .unwrap();

        let stored = service.contact("alice.example").await.unwrap();
        assert_eq!(stored.status, 1);
        assert_eq!(stored.requested_access, requested("/requested"));
        assert_eq!(stored.granted_access, granted("/granted", Effect::Allow));
        let rows = service
            .db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT api, effect FROM access_rules WHERE grantee = 'alice.example'",
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(String::try_get(&rows[0], "", "api").unwrap(), "/local");
        assert_eq!(
            service
                .policies
                .read()
                .await
                .evaluate(
                    &http::Method::GET,
                    "/local",
                    &[Grantee::One("alice.example".into())]
                )
                .0,
            Effect::Review
        );
    }

    #[tokio::test]
    async fn granted_update_never_changes_local_rules_and_is_idempotent() {
        let service = service().await;
        service.create_contact(contact()).await.unwrap();
        service
            .set_policy(
                Method::Specified(http::Method::GET),
                "/local",
                Effect::Allow,
                Grantee::One("alice.example".into()),
            )
            .await
            .unwrap();
        service
            .patch_contact("alice.example", ContactPatch::LocalApproval)
            .await
            .unwrap();
        let patch = ContactPatch::GrantedUpdate {
            granted_access: granted("/remote", Effect::Review),
        };
        service
            .patch_contact("alice.example", patch.clone())
            .await
            .unwrap();
        service.patch_contact("alice.example", patch).await.unwrap();
        assert_eq!(
            service
                .contact("alice.example")
                .await
                .unwrap()
                .granted_access,
            granted("/remote", Effect::Review)
        );
        let rows = service
            .db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT api FROM access_rules WHERE grantee = 'alice.example'",
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(String::try_get(&rows[0], "", "api").unwrap(), "/local");
    }

    #[tokio::test]
    async fn delete_removes_contact_database_rules_and_memory_rules() {
        let service = service().await;
        service.create_contact(contact()).await.unwrap();
        service
            .set_policy(
                Method::Specified(http::Method::GET),
                "/local",
                Effect::Allow,
                Grantee::One("alice.example".into()),
            )
            .await
            .unwrap();
        service
            .patch_contact("alice.example", ContactPatch::LocalApproval)
            .await
            .unwrap();
        service.delete_contact("alice.example").await.unwrap();
        assert!(service.contact("alice.example").await.is_err());
        assert!(service.contacts.read().await.get("alice.example").is_none());
        assert_eq!(
            service
                .policies
                .read()
                .await
                .evaluate(
                    &http::Method::GET,
                    "/local",
                    &[Grantee::One("alice.example".into())]
                )
                .0,
            Effect::Deny
        );
        assert!(service.delete_contact("alice.example").await.is_err());
    }

    #[tokio::test]
    async fn http_contact_endpoints_cover_create_list_get_granted_update_and_batch_delete() {
        let service = Arc::new(service().await);
        let app = crate::api::router(service.clone(), "owner.example");
        let owner = request(
            http::Method::POST,
            "/contacts",
            r#"{"name":"owner.example","subject_id":"owner-hash"}"#,
        );
        assert_eq!(
            app.clone().oneshot(owner).await.unwrap().status(),
            http::StatusCode::BAD_REQUEST
        );
        let create = request(
            http::Method::POST,
            "/contacts",
            r#"{"name":"alice.example","subject_id":"owner-hash","class":"agent","description":"Alice","requested_access":{},"granted_access":{}}"#,
        );
        assert_eq!(
            app.clone().oneshot(create).await.unwrap().status(),
            http::StatusCode::CREATED
        );
        let duplicate = request(
            http::Method::POST,
            "/contacts",
            r#"{"name":"alice.example","subject_id":"owner-hash","class":"agent","description":"Alice","requested_access":{},"granted_access":{}}"#,
        );
        assert_eq!(
            app.clone().oneshot(duplicate).await.unwrap().status(),
            http::StatusCode::CONFLICT
        );

        let get = request(http::Method::GET, "/contact/alice.example", "");
        let response = app.clone().oneshot(get).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["subject_id"], "owner-hash");
        assert_eq!(body["class"], "agent");

        let list = request(http::Method::GET, "/contacts", "");
        assert_eq!(
            app.clone().oneshot(list).await.unwrap().status(),
            http::StatusCode::OK
        );

        for update in [
            request(
                http::Method::PATCH,
                "/contact/alice.example",
                r#"{"granted_access":{}}"#,
            ),
            as_visitor(
                request(
                    http::Method::PATCH,
                    "/contact/alice.example",
                    r#"{"granted_access":{}}"#,
                ),
                "mallory.example",
            ),
        ] {
            assert_eq!(
                app.clone().oneshot(update).await.unwrap().status(),
                http::StatusCode::BAD_REQUEST
            );
        }
        let legacy = as_visitor(
            request(
                http::Method::PATCH,
                "/contact/alice.example",
                r#"{"kind":"granted_update","granted_access":{}}"#,
            ),
            "alice.example",
        );
        assert_eq!(
            app.clone().oneshot(legacy).await.unwrap().status(),
            http::StatusCode::UNPROCESSABLE_ENTITY
        );

        let update = request(
            http::Method::PATCH,
            "/contact/alice.example",
            r#"{"granted_access":{"/remote":{"review":["GET"]}}}"#,
        );
        assert_eq!(
            app.clone()
                .oneshot(as_visitor(update, "alice.example"))
                .await
                .unwrap()
                .status(),
            http::StatusCode::NO_CONTENT
        );
        assert_eq!(
            service
                .contact("alice.example")
                .await
                .unwrap()
                .granted_access,
            granted("/remote", Effect::Review)
        );

        let delete = request(
            http::Method::DELETE,
            "/contacts",
            r#"{"names":["alice.example"]}"#,
        );
        assert_eq!(
            app.clone().oneshot(delete).await.unwrap().status(),
            http::StatusCode::NO_CONTENT
        );
        assert!(service.contact("alice.example").await.is_err());
        let missing = request(http::Method::GET, "/contact/alice.example", "");
        assert_eq!(
            app.oneshot(missing).await.unwrap().status(),
            http::StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn http_lists_contacts_by_page_and_selected_sort() {
        let service = Arc::new(service().await);
        service
            .db
            .execute_unprepared(
                r#"INSERT INTO contacts
                   (name, subject_id, alias, class, grants, requests, status, updated_at, created_at)
                   VALUES
                   ('charlie.example', X'01', 'beta',  'service', '{}', '{}', 2, 30, 10),
                   ('alice.example',   X'02', 'gamma', 'human',   '{}', '{}', 2, 10, 30),
                   ('bob.example',     X'03', 'alpha', 'admin',   '{}', '{}', 2, 20, 20)"#,
            )
            .await
            .unwrap();
        let app = crate::api::router(service, "owner.example");

        for (sort, expected) in [
            ("name", "alice.example"),
            ("names", "alice.example"),
            ("alias", "bob.example"),
            ("updated_at", "alice.example"),
            ("created_at", "charlie.example"),
            ("class", "bob.example"),
        ] {
            let response = app
                .clone()
                .oneshot(request(
                    http::Method::GET,
                    &format!("/contacts?page=1&page_size=1&sort={sort}&order=asc"),
                    "",
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), http::StatusCode::OK);
            let body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(body["total"], 3);
            assert_eq!(body["page"], 1);
            assert_eq!(body["page_size"], 1);
            assert_eq!(body["items"][0]["name"], expected);
        }

        let second_page = app
            .clone()
            .oneshot(request(
                http::Method::GET,
                "/contacts?page=2&page_size=1&sort=name&order=asc",
                "",
            ))
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&second_page.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["items"][0]["name"], "bob.example");

        for (uri, expected) in [
            ("/contacts?page_size=1", "charlie.example"),
            (
                "/contacts?page_size=1&sort=name&order=desc",
                "charlie.example",
            ),
        ] {
            let response = app
                .clone()
                .oneshot(request(http::Method::GET, uri, ""))
                .await
                .unwrap();
            let body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(body["items"][0]["name"], expected);
        }

        for uri in [
            "/contacts?page=0",
            "/contacts?page_size=101",
            "/contacts?sort=unknown",
            "/contacts?order=sideways",
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(request(http::Method::GET, uri, ""))
                    .await
                    .unwrap()
                    .status(),
                http::StatusCode::BAD_REQUEST
            );
        }
    }

    #[tokio::test]
    async fn local_approval_marks_notifying_then_activates_after_peer_accepts() {
        let service = Arc::new(service().await);
        service.create_contact(contact()).await.unwrap();
        service
            .set_policy(
                Method::Specified(http::Method::GET),
                "/local",
                Effect::Allow,
                Grantee::One("alice.example".into()),
            )
            .await
            .unwrap();
        service
            .set_policy(
                Method::Specified(http::Method::POST),
                "/local",
                Effect::Review,
                Grantee::One("alice.example".into()),
            )
            .await
            .unwrap();
        service
            .set_policy(
                Method::Specified(http::Method::DELETE),
                "/local",
                Effect::Deny,
                Grantee::One("alice.example".into()),
            )
            .await
            .unwrap();
        let notifier = Arc::new(RecordingNotifier {
            service: service.clone(),
            fail: false,
            notifications: Mutex::new(Vec::new()),
        });
        let app = crate::api::router_with_notifier(
            service.clone(),
            "owner.example",
            Some(notifier.clone()),
        );
        let approval = request(http::Method::PATCH, "/contact/alice.example", r#"{}"#);
        assert_eq!(
            app.oneshot(as_visitor(approval, "owner.example"))
                .await
                .unwrap()
                .status(),
            http::StatusCode::NO_CONTENT
        );

        let notifications = notifier.notifications.lock().unwrap();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].0, "alice.example");
        assert_eq!(
            notifications[0].1, 1,
            "contact must remain retryable until notification succeeds"
        );
        let granted_access = notifications[0].2.granted_access.as_ref().unwrap();
        assert_eq!(granted_access.len(), 1);
        assert_eq!(
            granted_access["/local"].allow,
            [Method::Specified(http::Method::GET)]
        );
        assert_eq!(
            granted_access["/local"].review,
            [Method::Specified(http::Method::POST)]
        );
        assert_eq!(
            granted_access["/local"].deny,
            [Method::Specified(http::Method::DELETE)]
        );
        drop(notifications);

        let stored = service.contact("alice.example").await.unwrap();
        assert_eq!(stored.status, 2);
        assert_eq!(stored.requested_access, requested("/requested"));
        assert_eq!(stored.granted_access, granted("/granted", Effect::Allow));
    }

    #[tokio::test]
    async fn failed_peer_notification_leaves_contact_syncing_and_retryable() {
        let service = Arc::new(service().await);
        service.create_contact(contact()).await.unwrap();
        service
            .set_policy(
                Method::Specified(http::Method::GET),
                "/local",
                Effect::Allow,
                Grantee::One("alice.example".into()),
            )
            .await
            .unwrap();
        let notifier = Arc::new(RecordingNotifier {
            service: service.clone(),
            fail: true,
            notifications: Mutex::new(Vec::new()),
        });
        let app = crate::api::router_with_notifier(
            service.clone(),
            "owner.example",
            Some(notifier.clone()),
        );
        let approval = request(http::Method::PATCH, "/contact/alice.example", r#"{}"#);
        assert_eq!(
            app.clone()
                .oneshot(as_visitor(approval, "owner.example"))
                .await
                .unwrap()
                .status(),
            http::StatusCode::BAD_GATEWAY
        );

        let stored = service.contact("alice.example").await.unwrap();
        assert_eq!(stored.status, 1);
        assert_eq!(stored.requested_access, requested("/requested"));
        let rows = service
            .db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT api FROM access_rules WHERE grantee = 'alice.example'",
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(String::try_get(&rows[0], "", "api").unwrap(), "/local");

        let missing = request(http::Method::PATCH, "/contact/missing.example", r#"{}"#);
        assert_eq!(
            app.oneshot(as_visitor(missing, "owner.example"))
                .await
                .unwrap()
                .status(),
            http::StatusCode::NOT_FOUND
        );
        assert_eq!(notifier.notifications.lock().unwrap().len(), 1);

        let retry_notifier = Arc::new(RecordingNotifier {
            service: service.clone(),
            fail: false,
            notifications: Mutex::new(Vec::new()),
        });
        let retry = crate::api::router_with_notifier(
            service.clone(),
            "owner.example",
            Some(retry_notifier.clone()),
        );
        let approval = request(http::Method::PATCH, "/contact/alice.example", r#"{}"#);
        assert_eq!(
            retry
                .oneshot(as_visitor(approval, "owner.example"))
                .await
                .unwrap()
                .status(),
            http::StatusCode::NO_CONTENT
        );
        assert_eq!(service.contact("alice.example").await.unwrap().status, 2);
        assert_eq!(retry_notifier.notifications.lock().unwrap().len(), 1);
    }
}

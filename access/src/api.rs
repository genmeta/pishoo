pub mod access;
pub mod contact;
pub mod review;

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{Router, routing::get};
pub use contact::{
    ContactPatch, ContactRecord, GrantedAccess, GrantedMethods, NewContact, RequestedAccess,
};
pub use review::{ReviewRecord, ReviewTarget};
use sea_orm::Order;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::AccessService;

const DEFAULT_PAGE_SIZE: u64 = 20;
const MAX_PAGE_SIZE: u64 = 100;

#[derive(Serialize)]
pub(crate) struct Page<T> {
    pub(crate) items: T,
    pub(crate) total: u64,
    pub(crate) page: u64,
    pub(crate) page_size: u64,
}

pub(crate) fn default_order() -> Order {
    Order::Desc
}

pub(crate) fn deserialize_order<'de, D>(deserializer: D) -> Result<Order, D::Error>
where
    D: Deserializer<'de>,
{
    match String::deserialize(deserializer)?.as_str() {
        "asc" => Ok(Order::Asc),
        "desc" => Ok(Order::Desc),
        value => Err(D::Error::custom(format!(
            "unknown order {value:?}, expected asc or desc"
        ))),
    }
}

pub(crate) fn order_sql(order: &Order) -> &'static str {
    match order {
        Order::Asc => "ASC",
        Order::Desc => "DESC",
        _ => unreachable!("HTTP query only accepts ascending or descending order"),
    }
}

pub(crate) fn pagination(
    page: Option<u64>,
    page_size: Option<u64>,
) -> Result<(u64, u64, i64, i64), ApiError> {
    let page = page.unwrap_or(1);
    let page_size = page_size.unwrap_or(DEFAULT_PAGE_SIZE);
    if page == 0 {
        return Err(bad_request("page must be greater than zero"));
    }
    if !(1..=MAX_PAGE_SIZE).contains(&page_size) {
        return Err(bad_request(format!(
            "page_size must be between 1 and {MAX_PAGE_SIZE}"
        )));
    }
    let offset = page
        .checked_sub(1)
        .and_then(|page| page.checked_mul(page_size))
        .and_then(|offset| i64::try_from(offset).ok())
        .ok_or_else(|| bad_request("page is too large"))?;
    Ok((page, page_size, offset, page_size as i64))
}

pub type NotifyError = Box<dyn std::error::Error + Send + Sync>;

pub trait ContactNotifier: Send + Sync {
    fn granted_update<'a>(
        &'a self,
        contact: &'a str,
        body: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>>;
}

#[derive(Clone)]
pub(crate) struct ApiState {
    pub(crate) service: Arc<AccessService>,
    pub(crate) owner_name: String,
    pub(crate) notifier: Option<Arc<dyn ContactNotifier>>,
    pub(crate) policy_changes: Arc<tokio::sync::Mutex<()>>,
}

pub fn router(service: Arc<AccessService>, owner_name: impl Into<String>) -> Router {
    router_with_notifier(service, owner_name, None)
}

pub fn router_with_notifier(
    service: Arc<AccessService>,
    owner_name: impl Into<String>,
    notifier: Option<Arc<dyn ContactNotifier>>,
) -> Router {
    Router::new()
        .route(
            "/contacts",
            get(contact::list)
                .post(contact::create)
                .delete(contact::delete_many),
        )
        .route(
            "/contact/{name}",
            get(contact::get)
                .patch(contact::patch)
                .delete(contact::delete),
        )
        .route(
            "/acl/access",
            get(access::list_apis)
                .post(access::set)
                .patch(access::set)
                .delete(access::delete),
        )
        .route("/acl/access/all", get(access::list_all_rules_by_api))
        .route("/acl/access/rules", get(access::list_rules_by_api))
        .route(
            "/acl/allow",
            get(access::list_apis)
                .post(access::set)
                .patch(access::set)
                .delete(access::delete),
        )
        .route("/acl/allow/all", get(access::list_all_rules_by_name))
        .route("/acl/allow/rules", get(access::list_rules_by_name))
        .route("/acl/reviews", axum::routing::patch(review::decide))
        .route("/acl/reviews/live", get(review::list_live))
        .route("/acl/reviews/persistent", get(review::list_persistent))
        .with_state(ApiState {
            service,
            owner_name: owner_name.into(),
            notifier,
            policy_changes: Arc::new(tokio::sync::Mutex::new(())),
        })
}

pub(crate) type ApiError = (http::StatusCode, String);

pub(crate) fn internal(error: impl std::fmt::Display) -> ApiError {
    (http::StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

pub(crate) fn bad_request(error: impl std::fmt::Display) -> ApiError {
    (http::StatusCode::BAD_REQUEST, error.to_string())
}

pub(crate) fn database(error: sea_orm::DbErr) -> ApiError {
    let status = if matches!(
        error.sql_err(),
        Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
    ) {
        http::StatusCode::CONFLICT
    } else {
        match error {
            sea_orm::DbErr::RecordNotFound(_) => http::StatusCode::NOT_FOUND,
            sea_orm::DbErr::Type(_) => http::StatusCode::BAD_REQUEST,
            _ => http::StatusCode::INTERNAL_SERVER_ERROR,
        }
    };
    (status, error.to_string())
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;
    use sea_orm::{ConnectionTrait, Database};
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn management_router_exposes_contact_rule_and_review_endpoints() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute_unprepared(include_str!("migrations/0.sql"))
            .await
            .unwrap();
        let app = router(Arc::new(AccessService::new(db)), "owner.example");
        let create = http::Request::builder()
            .method(http::Method::POST)
            .uri("/contacts")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(r#"{"name":"alice.example","subject_id":"01","description":"Alice","requested_access":{},"granted_access":{}}"#))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(create).await.unwrap().status(),
            http::StatusCode::CREATED
        );

        let response = app
            .clone()
            .oneshot(
                http::Request::builder()
                    .uri("/contacts")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("alice.example")
        );
        assert_eq!(
            app.clone()
                .oneshot(
                    http::Request::builder()
                        .uri("/acl/access")
                        .body(axum::body::Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            http::StatusCode::OK
        );
        for uri in ["/acl/reviews/live", "/acl/reviews/persistent"] {
            assert_eq!(
                app.clone()
                    .oneshot(
                        http::Request::builder()
                            .uri(uri)
                            .body(axum::body::Body::empty())
                            .unwrap()
                    )
                    .await
                    .unwrap()
                    .status(),
                http::StatusCode::OK
            );
        }
    }
}

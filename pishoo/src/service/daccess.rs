use std::sync::Arc;

use access_control::{
    AccessService, AuthResult, Headers, PendingReviewResponse, SubjectId, Visitor,
    is_contact_status_path, is_review_status_path, management_router,
};
use axum::{
    Json, Router,
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};
use dhttp::identity::Identity;
use snafu::{ResultExt, Snafu};

use crate::{
    chat::{self, ChatState, store::StoreError as ChatStoreError},
    workspace::{self, WorkspaceState, store::StoreError as WorkspaceStoreError},
};

/// The profile-local daccess resources used by one pishoo server.
pub struct DaccessService {
    pub service: Arc<AccessService>,
    pub workspace: Arc<WorkspaceState>,
    pub(crate) chat: Arc<ChatState>,
}

#[derive(Clone)]
pub struct DaccessAuthState {
    service: Arc<AccessService>,
    workspace: Arc<WorkspaceState>,
    client_names: gateway::reverse::access_control::ClientNameResolver,
}

impl DaccessAuthState {
    pub fn new(service: Arc<AccessService>, workspace: Arc<WorkspaceState>) -> Self {
        Self {
            service,
            workspace,
            client_names: Default::default(),
        }
    }

    pub fn client_names(&self) -> gateway::reverse::access_control::ClientNameResolver {
        self.client_names.clone()
    }
}

/// pishoo's in-process authorization middleware.
///
/// `ClientNameResolver` reads the name and certificate owner hash from the
/// connection that DHTTP already authenticated. The resulting `Visitor` is
/// inserted only after that extraction has succeeded.
pub async fn authorize(
    State(state): State<DaccessAuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    let visitor = state
        .client_names
        .resolve_identity(&request)
        .await
        .and_then(|identity| {
            identity.owner_hash().and_then(|owner_hash| {
                SubjectId::new(owner_hash.as_bytes().to_vec())
                    .ok()
                    .map(|subject_id| Visitor::new(identity.name(), subject_id))
            })
        });
    if let Some(visitor) = &visitor {
        request.extensions_mut().insert(visitor.clone());
    }
    if is_review_status_path(request.method(), request.uri().path())
        || is_contact_status_path(request.method(), request.uri().path())
        || is_public_profile_path(request.method(), request.uri().path())
    {
        return next.run(request).await;
    }

    let headers = Headers {
        method: request.method().clone(),
        path: request.uri().path_and_query().map_or_else(
            || request.uri().path().to_owned(),
            |path| path.as_str().to_owned(),
        ),
        fields: request.headers().clone(),
    };

    match state
        .service
        .auth(
            headers,
            visitor.as_ref().map(Visitor::name),
            visitor.as_ref().map(Visitor::subject_id),
        )
        .await
    {
        Ok(AuthResult::Allowed) => {
            if request.method() == http::Method::POST && request.uri().path() == "/std/message" {
                let Some(visitor) = visitor.as_ref() else {
                    return (
                        http::StatusCode::FORBIDDEN,
                        "verified Chat contact required",
                    )
                        .into_response();
                };
                match workspace::directory::approved_chat_grant(&state.workspace, visitor).await {
                    Ok(true) => {}
                    Ok(false) => {
                        return (
                            http::StatusCode::FORBIDDEN,
                            "Chat capability is not granted",
                        )
                            .into_response();
                    }
                    Err(error) => {
                        tracing::error!(error = %error, "Chat capability authorization failed");
                        return (
                            http::StatusCode::INTERNAL_SERVER_ERROR,
                            "Chat capability authorization failed",
                        )
                            .into_response();
                    }
                }
            }
            next.run(request).await
        }
        Ok(AuthResult::Denied) => (http::StatusCode::FORBIDDEN, "access denied").into_response(),
        Ok(AuthResult::Reviewing(review)) => (
            http::StatusCode::ACCEPTED,
            Json(PendingReviewResponse::from(review)),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(error = %error, "daccess authorization failed");
            (
                http::StatusCode::INTERNAL_SERVER_ERROR,
                "access authorization failed",
            )
                .into_response()
        }
    }
}

fn is_public_profile_path(method: &http::Method, path: &str) -> bool {
    crate::workspace::capabilities::BuiltInCapability::public_endpoint(method, path)
}

pub fn management_app(access: &DaccessService) -> Router {
    workspace::router(access.workspace.clone())
        .merge(chat::router(access.chat.clone()))
        .merge(management_router(access.service.clone()))
}

#[derive(Debug, Snafu)]
#[snafu(module(daccess_load_error))]
pub enum DaccessLoadError {
    #[snafu(display("server identity has no valid DHTTP subject key identifier"))]
    OwnerIdentity {
        source: dhttp::identity::ExtractDhttpSubjectKeyIdentifierError,
    },
    #[snafu(display("server identity owner hash is invalid for daccess"))]
    OwnerSubjectId,
    #[snafu(display("failed to create daccess database directory `{}`", path.display()))]
    DatabaseDirectory {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("failed to load daccess database `{database_uri}`"))]
    Service {
        database_uri: String,
        source: sea_orm::DbErr,
    },
    #[snafu(display("failed to load profile Workspace store"))]
    Workspace { source: WorkspaceStoreError },
    #[snafu(display("failed to load profile Chat store"))]
    Chat { source: ChatStoreError },
}

pub async fn load(
    identity: Identity,
    profile: &dhttp::home::identity::IdentityProfile,
    database_uri: String,
) -> Result<DaccessService, DaccessLoadError> {
    let owner_name = identity.name().as_full().to_owned();
    let owner_hash = identity
        .dhttp_subject_key_identifier()
        .context(daccess_load_error::OwnerIdentitySnafu)?
        .owner_hash()
        .to_string();
    let owner_subject_id =
        SubjectId::new(owner_hash.into_bytes()).map_err(|_| DaccessLoadError::OwnerSubjectId)?;
    let service = AccessService::load_from_db(&database_uri, &owner_name, &owner_subject_id)
        .await
        .context(daccess_load_error::ServiceSnafu {
            database_uri: database_uri.clone(),
        })?;
    let service = Arc::new(service);
    let store = workspace::store::WorkspaceStore::open(profile)
        .await
        .context(daccess_load_error::WorkspaceSnafu)?;
    let workspace = Arc::new(WorkspaceState::new(
        profile.name().as_full().to_owned(),
        owner_name.clone(),
        owner_subject_id.clone(),
        store,
        service.clone(),
    ));
    let chat_store = chat::store::ChatStore::open(profile)
        .await
        .context(daccess_load_error::ChatSnafu)?;
    let chat = Arc::new(ChatState::new(
        owner_name,
        owner_subject_id,
        chat_store,
        service.clone(),
    ));
    workspace.configure_chat(chat.clone()).await;
    chat.start_worker();
    Ok(DaccessService {
        service,
        workspace,
        chat,
    })
}

pub fn profile_database_uri(
    profile: &dhttp::home::identity::IdentityProfile,
) -> Result<String, DaccessLoadError> {
    let path = profile.access_db_path();
    std::fs::create_dir_all(path.parent().expect("access database path has a parent"))
        .context(daccess_load_error::DatabaseDirectorySnafu { path: path.clone() })?;
    Ok(format!("sqlite://{}?mode=rwc", path.display()))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use dhttp::home::identity::IdentityProfile;

    #[test]
    fn profile_database_uri_creates_the_database_parent_and_uses_sqlite() {
        let root = std::env::temp_dir().join(format!(
            "pishoo-daccess-uri-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time is after the Unix epoch")
                .as_nanos()
        ));
        let profile_path = root.join("spike.liu");
        let profile = IdentityProfile::try_from(profile_path.clone()).expect("valid profile name");

        let uri = super::profile_database_uri(&profile).expect("profile database URI");

        assert_eq!(
            uri,
            format!(
                "sqlite://{}?mode=rwc",
                profile_path.join("db/access.db").display()
            )
        );
        assert!(profile_path.join("db").is_dir());

        fs::remove_dir_all(PathBuf::from(root)).expect("remove test directory");
    }
}

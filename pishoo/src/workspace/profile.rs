use std::{
    ffi::OsStr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use access_control::Visitor;
use axum::{
    Extension, Json,
    body::Body,
    extract::{Path, State},
    response::Response,
};
use bytes::Bytes;
use dhttp::name::DhttpName;
use http::{HeaderMap, Method, StatusCode, header};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    WorkspaceState,
    settings::{ApiError, ProfileSettings, profile_settings, read_stored_profile, require_owner},
};

const MAX_AVATAR_BYTES: usize = 1024 * 1024;
const MAX_AVATAR_DIMENSION: usize = 2048;
const PUBLIC_CACHE: &str = "public, max-age=300, must-revalidate";
const PRIVATE_CACHE: &str = "private, max-age=300, must-revalidate";
pub(super) const REMOTE_PROFILE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Serialize)]
pub(crate) struct PublicProfile {
    display_name: Option<String>,
    avatar_url: Option<String>,
    updated_at: i64,
}

#[derive(Deserialize)]
struct RemotePublicProfile {
    display_name: Option<String>,
    avatar_url: Option<String>,
    updated_at: i64,
}

#[derive(Clone, Copy)]
enum AvatarFormat {
    Jpeg,
    Png,
    Webp,
}

impl AvatarFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Webp => "webp",
        }
    }

    fn media_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
        }
    }
}

fn io_error(error: std::io::Error) -> ApiError {
    tracing::error!(error = %error, "Workspace profile asset operation failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Workspace profile asset failed",
    )
}

fn validate_avatar(media_type: &str, body: &[u8]) -> Result<AvatarFormat, ApiError> {
    if body.is_empty() || body.len() > MAX_AVATAR_BYTES {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "invalid avatar size"));
    }
    let image_type = imagesize::image_type(body)
        .map_err(|_| (StatusCode::UNSUPPORTED_MEDIA_TYPE, "invalid avatar image"))?;
    let format = match (media_type, image_type) {
        ("image/jpeg", imagesize::ImageType::Jpeg) => AvatarFormat::Jpeg,
        ("image/png", imagesize::ImageType::Png) => AvatarFormat::Png,
        ("image/webp", imagesize::ImageType::Webp) => AvatarFormat::Webp,
        _ => {
            return Err((
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "avatar type does not match image data",
            ));
        }
    };
    let size = imagesize::blob_size(body)
        .map_err(|_| (StatusCode::UNSUPPORTED_MEDIA_TYPE, "invalid avatar image"))?;
    if size.width == 0
        || size.height == 0
        || size.width > MAX_AVATAR_DIMENSION
        || size.height > MAX_AVATAR_DIMENSION
    {
        return Err((StatusCode::BAD_REQUEST, "invalid avatar dimensions"));
    }
    Ok(format)
}

fn avatar_name(format: AvatarFormat, body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    let hash = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("avatar-{hash}.{}", format.extension())
}

fn avatar_path(root: &std::path::Path, name: &str) -> Option<PathBuf> {
    let path = std::path::Path::new(name);
    let file_name = path.file_name()?.to_str()?;
    if file_name != name || !name.starts_with("avatar-") {
        return None;
    }
    let extension = path.extension()?.to_str()?;
    let stem = path.file_stem()?.to_str()?.strip_prefix("avatar-")?;
    if stem.len() != 64
        || !stem.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !matches!(extension, "jpg" | "png" | "webp")
    {
        return None;
    }
    Some(root.join(name))
}

fn format_from_name(name: &str) -> Option<AvatarFormat> {
    match std::path::Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
    {
        Some("jpg") => Some(AvatarFormat::Jpeg),
        Some("png") => Some(AvatarFormat::Png),
        Some("webp") => Some(AvatarFormat::Webp),
        _ => None,
    }
}

fn etag_for(name: &str) -> String {
    format!("\"{name}\"")
}

fn bytes_response(
    body: Bytes,
    format: AvatarFormat,
    etag: &str,
    cache_control: &'static str,
    request_headers: &HeaderMap,
) -> Result<Response, ApiError> {
    if request_headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|value| value.as_bytes() == etag.as_bytes())
    {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .header(header::CACHE_CONTROL, cache_control)
            .body(Body::empty())
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "invalid avatar response"));
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, format.media_type())
        .header(header::CONTENT_LENGTH, body.len())
        .header(header::CACHE_CONTROL, cache_control)
        .header(header::ETAG, etag)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(Body::from(body))
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "invalid avatar response"))
}

async fn stored_avatar(
    state: &WorkspaceState,
    request_headers: &HeaderMap,
    cache_control: &'static str,
) -> Result<Response, ApiError> {
    let stored = read_stored_profile(state.store.db()).await?;
    let name = stored
        .avatar_name
        .ok_or((StatusCode::NOT_FOUND, "avatar not found"))?;
    let format = format_from_name(&name).ok_or((StatusCode::NOT_FOUND, "avatar not found"))?;
    let path = avatar_path(state.store.profile_assets(), &name)
        .ok_or((StatusCode::NOT_FOUND, "avatar not found"))?;
    let body = tokio::fs::read(path).await.map_err(io_error)?;
    bytes_response(
        Bytes::from(body),
        format,
        &etag_for(&name),
        cache_control,
        request_headers,
    )
}

pub(crate) async fn get_public_profile(
    State(state): State<Arc<WorkspaceState>>,
) -> Result<([(header::HeaderName, &'static str); 1], Json<PublicProfile>), ApiError> {
    let stored = read_stored_profile(state.store.db()).await?;
    Ok((
        [(header::CACHE_CONTROL, PUBLIC_CACHE)],
        Json(PublicProfile {
            display_name: stored.display_name,
            avatar_url: stored
                .avatar_name
                .map(|_| String::from("/std/profile/avatar")),
            updated_at: stored.updated_at,
        }),
    ))
}

pub(crate) async fn get_public_avatar(
    State(state): State<Arc<WorkspaceState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    stored_avatar(&state, &headers, PUBLIC_CACHE).await
}

pub(crate) async fn get_avatar(
    State(state): State<Arc<WorkspaceState>>,
    visitor: Option<Extension<Visitor>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    stored_avatar(&state, &headers, PRIVATE_CACHE).await
}

pub(crate) async fn put_avatar(
    State(state): State<Arc<WorkspaceState>>,
    visitor: Option<Extension<Visitor>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ProfileSettings>, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let _guard = state.profile_write.lock().await;
    let media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .ok_or((StatusCode::UNSUPPORTED_MEDIA_TYPE, "avatar type required"))?;
    let format = validate_avatar(media_type, &body)?;
    let name = avatar_name(format, &body);
    let destination = state.store.profile_assets().join(&name);
    let created = if destination.exists() {
        false
    } else {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "invalid system clock"))?
            .as_nanos();
        let temporary = state
            .store
            .profile_assets()
            .join(format!(".avatar-{}-{nonce}.tmp", std::process::id()));
        tokio::fs::write(&temporary, &body)
            .await
            .map_err(io_error)?;
        if let Err(error) = tokio::fs::rename(&temporary, &destination).await {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(io_error(error));
        }
        true
    };

    let transaction = state
        .store
        .db()
        .begin()
        .await
        .map_err(super::settings::storage_error)?;
    let old = read_stored_profile(&transaction).await?.avatar_name;
    if let Err(error) = transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE profile_preferences SET avatar_name = ?, updated_at = MAX(CAST(strftime('%s', 'now') AS INTEGER), updated_at + 1) WHERE id = 1",
            [name.clone().into()],
        ))
        .await
    {
        if created {
            let _ = tokio::fs::remove_file(&destination).await;
        }
        return Err(super::settings::storage_error(error));
    }
    let settings = profile_settings(state.owner.name(), read_stored_profile(&transaction).await?);
    if let Err(error) = transaction.commit().await {
        if created {
            let _ = tokio::fs::remove_file(&destination).await;
        }
        return Err(super::settings::storage_error(error));
    }
    if let Some(old) = old.filter(|old| old != &name)
        && let Some(path) = avatar_path(state.store.profile_assets(), &old)
    {
        let _ = tokio::fs::remove_file(path).await;
    }
    Ok(Json(settings))
}

pub(crate) async fn delete_avatar(
    State(state): State<Arc<WorkspaceState>>,
    visitor: Option<Extension<Visitor>>,
) -> Result<Json<ProfileSettings>, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let _guard = state.profile_write.lock().await;
    let transaction = state
        .store
        .db()
        .begin()
        .await
        .map_err(super::settings::storage_error)?;
    let old = read_stored_profile(&transaction).await?.avatar_name;
    transaction
        .execute_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "UPDATE profile_preferences SET avatar_name = NULL, updated_at = MAX(CAST(strftime('%s', 'now') AS INTEGER), updated_at + 1) WHERE id = 1".to_owned(),
        ))
        .await
        .map_err(super::settings::storage_error)?;
    let settings = profile_settings(state.owner.name(), read_stored_profile(&transaction).await?);
    transaction
        .commit()
        .await
        .map_err(super::settings::storage_error)?;
    if let Some(old) = old
        && let Some(path) = avatar_path(state.store.profile_assets(), &old)
    {
        let _ = tokio::fs::remove_file(path).await;
    }
    Ok(Json(settings))
}

fn validate_remote_profile(profile: &RemotePublicProfile) -> Result<(), ApiError> {
    if profile.updated_at < 0
        || profile.display_name.as_ref().is_some_and(|name| {
            name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control)
        })
        || profile
            .avatar_url
            .as_deref()
            .is_some_and(|url| url != "/std/profile/avatar")
    {
        return Err((StatusCode::BAD_GATEWAY, "invalid remote profile"));
    }
    Ok(())
}

async fn remote_request(
    state: &WorkspaceState,
    name: String,
    path: &'static str,
) -> Result<(String, super::outbound::RemoteResponse), ApiError> {
    let target = DhttpName::try_from(name)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid profile name"))?
        .as_full()
        .to_owned();
    let transport = state.outbound.read().await.clone().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "outbound connector unavailable",
    ))?;
    let started = Instant::now();
    let response = tokio::time::timeout(
        REMOTE_PROFILE_TIMEOUT,
        transport.request(&target, Method::GET, path, Bytes::new()),
    )
        .await
        .map_err(|_| {
            tracing::warn!(
                %target, path, elapsed_ms = started.elapsed().as_millis(),
                timeout_ms = REMOTE_PROFILE_TIMEOUT.as_millis(),
                "remote profile request timed out"
            );
            (StatusCode::GATEWAY_TIMEOUT, "remote profile timed out")
        })?
        .map_err(|error| {
            tracing::warn!(%target, path, %error, elapsed_ms = started.elapsed().as_millis(), "remote profile request failed");
            (StatusCode::BAD_GATEWAY, "remote profile unavailable")
        })?;
    Ok((target, response))
}

pub(crate) async fn get_remote_profile(
    State(state): State<Arc<WorkspaceState>>,
    visitor: Option<Extension<Visitor>>,
    Path(name): Path<String>,
) -> Result<([(header::HeaderName, &'static str); 1], Json<PublicProfile>), ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let (target, response) = remote_request(&state, name, "/std/profile").await?;
    if response.status != StatusCode::OK {
        return Err((StatusCode::BAD_GATEWAY, "remote profile unavailable"));
    }
    let profile: RemotePublicProfile = serde_json::from_slice(&response.body)
        .map_err(|_| (StatusCode::BAD_GATEWAY, "invalid remote profile"))?;
    validate_remote_profile(&profile)?;
    let avatar_url = profile.avatar_url.map(|_| {
        format!(
            "/workspace-api/profiles/{target}/avatar?v={}",
            profile.updated_at
        )
    });
    Ok((
        [(header::CACHE_CONTROL, PRIVATE_CACHE)],
        Json(PublicProfile {
            display_name: profile.display_name,
            avatar_url,
            updated_at: profile.updated_at,
        }),
    ))
}

pub(crate) async fn get_remote_avatar(
    State(state): State<Arc<WorkspaceState>>,
    visitor: Option<Extension<Visitor>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let (_, response) = remote_request(&state, name, "/std/profile/avatar").await?;
    if response.status != StatusCode::OK {
        return Err((StatusCode::BAD_GATEWAY, "remote avatar unavailable"));
    }
    let media_type = response
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .ok_or((StatusCode::BAD_GATEWAY, "invalid remote avatar"))?;
    let format = validate_avatar(media_type, &response.body)
        .map_err(|_| (StatusCode::BAD_GATEWAY, "invalid remote avatar"))?;
    let name = avatar_name(format, &response.body);
    bytes_response(
        response.body,
        format,
        &etag_for(&name),
        "private, max-age=86400, immutable",
        &headers,
    )
}

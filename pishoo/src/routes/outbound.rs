use axum::{body::Body as AxumBody, response::Response};
use http::{Request, Uri, header};
use http_body_util::BodyExt;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::{DHTTP_PREFIX, proxy::clean_hop_headers, reject};
use crate::{Error, Result};

pub(crate) async fn forward_dhttp(
    endpoint: dhttp::Endpoint,
    request: Request<AxumBody>,
) -> Response {
    let result: Result<Response> = async {
        let handshake = request
            .extensions()
            .get::<dhttp::HandshakeSummary>()
            .ok_or(Error::MissingHandshake)?;
        let local = handshake.local.as_ref().ok_or(Error::MissingHandshake)?;
        let remote = handshake.remote.as_ref().ok_or(Error::Denied)?;
        if dhttp_home::normalize_name(local.name()).as_deref() != Some(endpoint.name())
            || dhttp_home::normalize_name(remote.name()).as_deref() != Some(endpoint.name())
        {
            return Err(Error::Denied);
        }
        let local_ski =
            dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
                .map_err(|_| Error::Denied)?;
        let remote_ski =
            dhttp_home::certificate::extract_dhttp_subject_key_identifier(remote.certificates())
                .map_err(|_| Error::Denied)?;
        if local_ski.owner_hash() != remote_ski.owner_hash() {
            return Err(Error::Denied);
        }
        if request.headers().contains_key(header::TRAILER) {
            return Err(Error::BadRequest(
                "DHTTP request trailers are not supported".into(),
            ));
        }

        let (uri, authority) = outbound_uri(request.uri())?;
        let (mut parts, body) = request.into_parts();
        parts.uri = uri;
        parts.extensions.clear();
        clean_hop_headers(&mut parts.headers);
        parts.headers.insert(
            header::HOST,
            authority
                .parse()
                .map_err(|_| Error::BadRequest("invalid DHTTP target".into()))?,
        );
        let outbound = Request::from_parts(parts, dhttp::WndBuf::new(64 * 1024));
        let (mut writer, response) = endpoint.from_request(outbound).await?;
        let upload = scopeguard::guard(
            tokio::spawn(async move {
                if let Err(error) = upload_body(body, &mut writer).await {
                    // Dropping the unfinished RequestWriter resets the upstream
                    // request; do not treat unsupported trailers as a clean EOF.
                    tracing::warn!(%error, "DHTTP proxy upload failed");
                }
            }),
            |task| task.abort(),
        );
        let mut response = response.await?;
        // The two stream directions remain independent after response delivery.
        drop(scopeguard::ScopeGuard::into_inner(upload));
        clean_hop_headers(response.headers_mut());
        Ok(response.map(AxumBody::new))
    }
    .await;
    result.unwrap_or_else(reject)
}

async fn upload_body(
    mut body: AxumBody,
    writer: &mut (impl AsyncWrite + Unpin + Send),
) -> Result<()> {
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|error| Error::Io(std::io::Error::other(error)))?;
        match frame.into_data() {
            Ok(data) => writer.write_all(&data).await?,
            Err(_) => {
                return Err(Error::BadRequest(
                    "DHTTP request trailers are not supported".into(),
                ));
            }
        }
    }
    writer.shutdown().await?;
    Ok(())
}

fn outbound_uri(uri: &Uri) -> Result<(Uri, String)> {
    let path = uri
        .path()
        .strip_prefix(DHTTP_PREFIX)
        .ok_or(Error::RouteNotFound)?;
    let (target, suffix) = path.split_once('/').unwrap_or((path, ""));
    let (name, sequence) = match target.split_once(':') {
        Some((name, sequence)) => (name, Some(sequence)),
        None => (target, None),
    };
    let name = dhttp_home::normalize_name(name)
        .ok_or_else(|| Error::BadRequest("invalid DHTTP target".into()))?;
    let authority = match sequence {
        Some(sequence) => {
            let number = sequence
                .parse::<u64>()
                .map_err(|_| Error::BadRequest("invalid DHTTP target sequence".into()))?;
            dhttp_home::certificate::CertificateSequence::try_from(number)
                .map_err(|_| Error::BadRequest("invalid DHTTP target sequence".into()))?;
            format!("{name}:{sequence}")
        }
        None => name,
    };
    let path = match uri.query() {
        Some(query) => format!("/{suffix}?{query}"),
        None => format!("/{suffix}"),
    };
    let uri = format!("https://{authority}{path}")
        .parse()
        .map_err(|_| Error::BadRequest("invalid DHTTP target URI".into()))?;
    Ok((uri, authority))
}

#[cfg(test)]
#[path = "../../tests/unit/routes/outbound.rs"]
mod tests;

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use rustls::{
    Error,
    pki_types::{CertificateDer, UnixTime},
};

use super::wire::{OcspStatus, build_ocsp_request_der, decode_unvalidated_ocsp_response_der};

pub const CERT_SERVER_BASE_URL_ENV: &str = "CERT_SERVER_BASE_URL";

#[cfg(debug_assertions)]
pub const DEFAULT_CERT_SERVER_BASE_URL: &str = "http://127.0.0.1:3001";

#[cfg(not(debug_assertions))]
pub const DEFAULT_CERT_SERVER_BASE_URL: &str = "https://license.genmeta.net";

pub const OCSP_REFRESH_RETRY_DELAY: Duration = Duration::from_secs(5 * 60);
pub const OCSP_REFRESH_MAX_DELAY: Duration = Duration::from_secs(60 * 60);
pub const OCSP_REFRESH_EXPIRY_SKEW: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone)]
pub struct StapledOcspResponse {
    pub response_der: Vec<u8>,
    pub valid_until: UnixTime,
}

#[derive(Debug, Clone)]
struct CertServerOcspFetcher {
    base_url: Arc<str>,
    http_client: reqwest::Client,
}

impl CertServerOcspFetcher {
    fn new(base_url: Arc<str>) -> Self {
        let root_cert =
            reqwest::Certificate::from_pem(include_bytes!(concat!(env!("OUT_DIR"), "/root.crt")))
                .expect("gateway root certificate must be valid pem");
        let http_client = reqwest::Client::builder()
            .tls_certs_merge([root_cert])
            .gzip(true)
            .zstd(true)
            .build()
            .expect("gateway OCSP HTTP client must build");

        Self {
            base_url,
            http_client,
        }
    }

    async fn request_ocsp_response(&self, request_der: Vec<u8>) -> Result<Vec<u8>, Error> {
        let response = self
            .http_client
            .post(format!("{}/ocsp", self.base_url))
            .header(CONTENT_TYPE, "application/ocsp-request")
            .header(ACCEPT, "application/ocsp-response")
            .body(request_der)
            .send()
            .await
            .map_err(request_error)?;

        if !response.status().is_success() {
            let status = response.status();
            let message = response.text().await.unwrap_or_default();
            return Err(Error::General(format!(
                "OCSP responder returned HTTP status {status}: {message}"
            )));
        }

        response
            .bytes()
            .await
            .map(|body| body.to_vec())
            .map_err(request_error)
    }
}

pub fn cert_server_base_url() -> Arc<str> {
    std::env::var(CERT_SERVER_BASE_URL_ENV)
        .unwrap_or_else(|_| DEFAULT_CERT_SERVER_BASE_URL.to_owned())
        .into()
}

pub async fn fetch_stapled_ocsp(
    certificate_chain: &[CertificateDer<'_>],
) -> Result<StapledOcspResponse, Error> {
    let Some(end_entity) = certificate_chain.first() else {
        return Err(Error::General(
            "certificate chain is missing end-entity certificate".to_owned(),
        ));
    };
    let Some(issuer) = certificate_chain.get(1) else {
        return Err(Error::General(
            "certificate chain is missing issuer certificate".to_owned(),
        ));
    };

    let fetcher = shared_ocsp_fetcher();
    let request_der = build_ocsp_request_der(end_entity, issuer)?;
    let response_der = fetcher.request_ocsp_response(request_der).await?;
    let parsed = decode_unvalidated_ocsp_response_der(&response_der, now())?;

    match parsed.status {
        OcspStatus::Good => Ok(StapledOcspResponse {
            response_der,
            valid_until: parsed.valid_until,
        }),
        OcspStatus::Revoked => Err(Error::General(
            "OCSP responder reported the certificate as revoked".to_owned(),
        )),
        OcspStatus::Unknown => Err(Error::General(
            "OCSP responder reported the certificate status as unknown".to_owned(),
        )),
    }
}

pub fn next_stapling_refresh_delay(valid_until: UnixTime, now: UnixTime) -> Duration {
    let remaining = valid_until.as_secs().saturating_sub(now.as_secs());
    if remaining == 0 {
        return OCSP_REFRESH_RETRY_DELAY;
    }

    let delay_secs = remaining.saturating_sub(OCSP_REFRESH_EXPIRY_SKEW.as_secs());
    Duration::from_secs(delay_secs).clamp(OCSP_REFRESH_RETRY_DELAY, OCSP_REFRESH_MAX_DELAY)
}

fn shared_ocsp_fetcher() -> Arc<CertServerOcspFetcher> {
    static FETCHER: OnceLock<Arc<CertServerOcspFetcher>> = OnceLock::new();

    FETCHER
        .get_or_init(|| Arc::new(CertServerOcspFetcher::new(cert_server_base_url())))
        .clone()
}

fn now() -> UnixTime {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    UnixTime::since_unix_epoch(now)
}

fn request_error(error: reqwest::Error) -> Error {
    Error::General(format!("failed to query OCSP responder: {error}"))
}

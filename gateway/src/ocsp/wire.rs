use der::{
    Choice, Decode, Encode, Enumerated, Sequence,
    asn1::{Any, GeneralizedTime, Null, ObjectIdentifier, OctetString},
    oid::db::{rfc5912::ID_SHA_1, rfc6960::ID_PKIX_OCSP_BASIC},
};
use rustls::{
    Error,
    pki_types::{CertificateDer, UnixTime},
};
use sha1::{Digest, Sha1};
use x509_cert::{
    Certificate, ext::Extensions, serial_number::SerialNumber, spki::AlgorithmIdentifierOwned,
};
use x509_parser::parse_x509_certificate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OcspStatus {
    Good,
    Revoked,
    Unknown,
}

#[derive(Debug, Clone)]
pub(super) struct ParsedOcspResponse {
    pub(super) status: OcspStatus,
    pub(super) valid_until: UnixTime,
    pub(super) basic: BasicOcspResponse,
}

pub(super) fn build_ocsp_request_der(
    end_entity: &CertificateDer<'_>,
    issuer: &CertificateDer<'_>,
) -> Result<Vec<u8>, Error> {
    let end_entity = Certificate::from_der(end_entity.as_ref())
        .map_err(|error| Error::General(format!("failed to decode end-entity cert: {error}")))?;
    let issuer = Certificate::from_der(issuer.as_ref())
        .map_err(|error| Error::General(format!("failed to decode issuer cert: {error}")))?;
    build_request_der(&end_entity, &issuer)
}

pub(super) fn decode_unvalidated_ocsp_response_der(
    response_der: &[u8],
    now: UnixTime,
) -> Result<ParsedOcspResponse, Error> {
    let response = OcspResponse::from_der(response_der).map_err(der_error)?;
    if response.response_status != OcspResponseStatus::Successful {
        return Err(Error::General(format!(
            "OCSP responder returned non-success status: {:?}",
            response.response_status
        )));
    }

    let response_bytes = response
        .response_bytes
        .ok_or_else(|| Error::General("OCSP response is missing response bytes".to_owned()))?;
    if response_bytes.response_type != ID_PKIX_OCSP_BASIC {
        return Err(Error::General("unsupported OCSP response type".to_owned()));
    }

    let basic =
        BasicOcspResponse::from_der(response_bytes.response.as_bytes()).map_err(der_error)?;
    let [single] = basic.tbs_response_data.responses.as_slice() else {
        return Err(Error::General(
            "OCSP response must contain exactly one single response".to_owned(),
        ));
    };

    let produced_at = as_unix_time(&basic.tbs_response_data.produced_at);
    if produced_at.as_secs() > now.as_secs() {
        return Err(Error::General(
            "OCSP response produced_at is in the future".to_owned(),
        ));
    }

    let this_update = as_unix_time(&single.this_update);
    if this_update.as_secs() > now.as_secs() {
        return Err(Error::General(
            "OCSP response this_update is in the future".to_owned(),
        ));
    }

    let valid_until = single
        .next_update
        .as_ref()
        .map(as_unix_time)
        .unwrap_or(this_update);
    if valid_until.as_secs() < this_update.as_secs() {
        return Err(Error::General(
            "OCSP response next_update is earlier than this_update".to_owned(),
        ));
    }
    if valid_until.as_secs() < now.as_secs() {
        return Err(Error::General(
            "OCSP response is already expired".to_owned(),
        ));
    }

    let status = match &single.cert_status {
        CertStatus::Good(_) => OcspStatus::Good,
        CertStatus::Revoked(_) => OcspStatus::Revoked,
        CertStatus::Unknown(_) => OcspStatus::Unknown,
    };

    Ok(ParsedOcspResponse {
        status,
        valid_until,
        basic,
    })
}

pub(super) fn parse_x509_certificate_der<'a>(
    cert_der: &'a [u8],
    label: &str,
) -> Result<x509_parser::certificate::X509Certificate<'a>, Error> {
    parse_x509_certificate(cert_der)
        .map(|(_, cert)| cert)
        .map_err(|error| Error::General(format!("failed to parse {label}: {error:?}")))
}

pub(super) fn responder_id_matches_certificate(
    responder_id: &ResponderId,
    certificate: &Certificate,
) -> Result<bool, Error> {
    match responder_id {
        ResponderId::ByName(name) => Ok(name.to_der().map_err(der_error)?
            == certificate
                .tbs_certificate()
                .subject()
                .to_der()
                .map_err(der_error)?),
        ResponderId::ByKey(key_hash) => Ok(key_hash.as_bytes()
            == Sha1::digest(
                certificate
                    .tbs_certificate()
                    .subject_public_key_info()
                    .subject_public_key
                    .raw_bytes(),
            )
            .as_slice()),
    }
}

pub(super) fn matches_cert_id(actual: &CertId, expected: &CertId) -> bool {
    actual.hash_algorithm.oid == expected.hash_algorithm.oid
        && actual.issuer_name_hash == expected.issuer_name_hash
        && actual.issuer_key_hash == expected.issuer_key_hash
        && actual.serial_number == expected.serial_number
}

pub(super) fn build_cert_id(
    end_entity: &Certificate,
    issuer: &Certificate,
) -> Result<CertId, Error> {
    let issuer_name_hash = Sha1::digest(
        issuer
            .tbs_certificate()
            .subject()
            .to_der()
            .map_err(der_error)?,
    );
    let issuer_key_hash = Sha1::digest(
        issuer
            .tbs_certificate()
            .subject_public_key_info()
            .subject_public_key
            .raw_bytes(),
    );

    Ok(CertId {
        hash_algorithm: AlgorithmIdentifierOwned {
            oid: ID_SHA_1,
            parameters: Some(Null.into()),
        },
        issuer_name_hash: OctetString::new(issuer_name_hash.as_slice()).map_err(der_error)?,
        issuer_key_hash: OctetString::new(issuer_key_hash.as_slice()).map_err(der_error)?,
        serial_number: end_entity.tbs_certificate().serial_number().clone(),
    })
}

pub(super) fn der_error(error: impl std::fmt::Display) -> Error {
    Error::General(format!("failed to process OCSP DER: {error}"))
}

fn build_request_der(end_entity: &Certificate, issuer: &Certificate) -> Result<Vec<u8>, Error> {
    OcspRequest {
        tbs_request: TbsRequest {
            version: Version::default(),
            requestor_name: None,
            request_list: vec![RequestEntry {
                req_cert: build_cert_id(end_entity, issuer)?,
                single_request_extensions: None,
            }],
            request_extensions: None,
        },
        optional_signature: None,
    }
    .to_der()
    .map_err(der_error)
}

fn as_unix_time(time: &GeneralizedTime) -> UnixTime {
    UnixTime::since_unix_epoch(time.to_unix_duration())
}

#[derive(Clone, Debug, Default, Copy, PartialEq, Eq, Enumerated)]
#[asn1(type = "INTEGER")]
#[repr(u8)]
enum Version {
    #[default]
    V1 = 0,
}

#[derive(Clone, Debug, Eq, PartialEq, Sequence)]
struct OcspRequest {
    tbs_request: TbsRequest,

    #[asn1(context_specific = "0", optional = "true", tag_mode = "EXPLICIT")]
    optional_signature: Option<Any>,
}

#[derive(Clone, Debug, Eq, PartialEq, Sequence)]
struct TbsRequest {
    #[asn1(
        context_specific = "0",
        default = "Default::default",
        tag_mode = "EXPLICIT"
    )]
    version: Version,

    #[asn1(context_specific = "1", optional = "true", tag_mode = "EXPLICIT")]
    requestor_name: Option<Any>,

    request_list: Vec<RequestEntry>,

    #[asn1(context_specific = "2", optional = "true", tag_mode = "EXPLICIT")]
    request_extensions: Option<Extensions>,
}

#[derive(Clone, Debug, Eq, PartialEq, Sequence)]
struct RequestEntry {
    req_cert: CertId,

    #[asn1(context_specific = "0", optional = "true", tag_mode = "EXPLICIT")]
    single_request_extensions: Option<Extensions>,
}

#[derive(Clone, Debug, Eq, PartialEq, Sequence)]
pub(super) struct CertId {
    pub(super) hash_algorithm: AlgorithmIdentifierOwned,
    pub(super) issuer_name_hash: OctetString,
    pub(super) issuer_key_hash: OctetString,
    pub(super) serial_number: SerialNumber,
}

#[derive(Clone, Debug, Eq, PartialEq, Choice)]
enum CertStatus {
    #[asn1(context_specific = "0", tag_mode = "IMPLICIT")]
    Good(Null),

    #[asn1(context_specific = "1", tag_mode = "IMPLICIT", constructed = "true")]
    Revoked(RevokedInfo),

    #[asn1(context_specific = "2", tag_mode = "IMPLICIT")]
    Unknown(Null),
}

#[derive(Clone, Debug, Eq, PartialEq, Sequence)]
struct RevokedInfo {
    revocation_time: GeneralizedTime,

    #[asn1(context_specific = "0", optional = "true", tag_mode = "EXPLICIT")]
    revocation_reason: Option<Any>,
}

#[derive(Clone, Debug, Eq, PartialEq, Sequence)]
pub(super) struct SingleResponse {
    pub(super) cert_id: CertId,
    cert_status: CertStatus,
    this_update: GeneralizedTime,

    #[asn1(context_specific = "0", optional = "true", tag_mode = "EXPLICIT")]
    next_update: Option<GeneralizedTime>,

    #[asn1(context_specific = "1", optional = "true", tag_mode = "EXPLICIT")]
    single_extensions: Option<Extensions>,
}

#[derive(Clone, Debug, Eq, PartialEq, Choice)]
pub(super) enum ResponderId {
    #[asn1(context_specific = "1", tag_mode = "EXPLICIT", constructed = "true")]
    ByName(Any),

    #[asn1(context_specific = "2", tag_mode = "EXPLICIT", constructed = "true")]
    ByKey(OctetString),
}

#[derive(Clone, Debug, Eq, PartialEq, Sequence)]
pub(super) struct ResponseData {
    #[asn1(
        context_specific = "0",
        default = "Default::default",
        tag_mode = "EXPLICIT"
    )]
    version: Version,
    pub(super) responder_id: ResponderId,
    produced_at: GeneralizedTime,
    pub(super) responses: Vec<SingleResponse>,

    #[asn1(context_specific = "1", optional = "true", tag_mode = "EXPLICIT")]
    response_extensions: Option<Extensions>,
}

#[derive(Clone, Debug, Eq, PartialEq, Sequence)]
pub(super) struct BasicOcspResponse {
    pub(super) tbs_response_data: ResponseData,
    pub(super) signature_algorithm: AlgorithmIdentifierOwned,
    pub(super) signature: der::asn1::BitString,

    #[asn1(context_specific = "0", optional = "true", tag_mode = "EXPLICIT")]
    pub(super) certs: Option<Vec<Certificate>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Sequence)]
struct ResponseBytes {
    response_type: ObjectIdentifier,
    response: OctetString,
}

#[derive(Enumerated, Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
enum OcspResponseStatus {
    Successful = 0,
    MalformedRequest = 1,
    InternalError = 2,
    TryLater = 3,
    SigRequired = 5,
    Unauthorized = 6,
}

#[derive(Clone, Debug, Eq, PartialEq, Sequence)]
struct OcspResponse {
    response_status: OcspResponseStatus,

    #[asn1(context_specific = "0", optional = "true", tag_mode = "EXPLICIT")]
    response_bytes: Option<ResponseBytes>,
}

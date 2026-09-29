//! WASI HTTP denial hook and identity capabilities exposed to WASM guests.

use http::Request;
use wasmtime_wasi_http::p2::{
    HttpResult, WasiHttpHooks,
    bindings::http::types,
    body::HyperOutgoingBody,
    types::{HostFutureIncomingResponse, OutgoingRequestConfig},
};

use super::{DenyOutgoing, StoreData};

impl WasiHttpHooks for DenyOutgoing {
    fn send_request(
        &mut self,
        _request: Request<HyperOutgoingBody>,
        _config: OutgoingRequestConfig,
    ) -> HttpResult<HostFutureIncomingResponse> {
        Err(types::ErrorCode::HttpRequestDenied.into())
    }
}

// Existing identity WIT bindings and capability-checked host operations.

pub(super) mod identity {
    wasmtime::component::bindgen!({
        path: "../wit/pishoo-identity",
        inline: "package pishoo:host; world identity-host { import pishoo:identity/signatures@0.1.0; }",
        imports: { default: async },
    });
}

impl identity::pishoo::identity::signatures::Host for StoreData {
    async fn sign(
        &mut self,
        data: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, identity::pishoo::identity::signatures::SignError> {
        use identity::pishoo::identity::signatures::SignError;
        if data.len() > 1024 * 1024 {
            return Err(SignError::InputTooLarge);
        }
        let mut signature = None;
        for scheme in [
            qtls::SignatureScheme::RSA_PSS_SHA512,
            qtls::SignatureScheme::ECDSA_NISTP256_SHA256,
            qtls::SignatureScheme::ECDSA_NISTP384_SHA384,
            qtls::SignatureScheme::ED25519,
        ] {
            match self.local.sign(scheme, &data) {
                Ok(value) => {
                    signature = Some(value);
                    break;
                }
                Err(qtls::SignError::UnsupportedScheme { .. }) => {}
                Err(_) => return Err(SignError::Failed),
            }
        }
        let signature = signature.ok_or(SignError::Failed)?;
        if signature.len() > 8192 {
            return Err(SignError::Failed);
        }
        Ok(signature)
    }

    async fn verify(
        &mut self,
        signature: Vec<u8>,
        data: Vec<u8>,
        name: String,
    ) -> std::result::Result<bool, identity::pishoo::identity::signatures::VerifyError> {
        use identity::pishoo::identity::signatures::VerifyError;
        if data.len() > 1024 * 1024 || signature.len() > 8192 {
            return Err(VerifyError::InputTooLarge);
        }
        let name = dhttp_home::normalize_name(&name).ok_or(VerifyError::InvalidIdentity)?;
        if name == self.local.name() {
            return dhttp_home::certificate::verify_signature(
                self.local.public_key().as_ref(),
                &data,
                &signature,
            )
            .map_err(|_| VerifyError::Failed);
        }
        if let Some(remote) = &self.remote
            && name == remote.name()
        {
            return dhttp_home::certificate::verify_signature(
                remote.public_key().as_ref(),
                &data,
                &signature,
            )
            .map_err(|_| VerifyError::Failed);
        }
        Err(VerifyError::Unavailable)
    }
}

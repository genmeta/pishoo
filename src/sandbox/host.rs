//! Standard WASI HTTP and verified identity capabilities for WASM guests.

use super::StoreData;

pub(super) mod execution;
mod http;
pub(super) mod sqlite;

// Existing identity WIT bindings and capability-checked host operations.

pub(super) mod identity {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "identity-v1",
        imports: { default: async },
    });
}

pub(super) mod identity_v2 {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "identity-v2",
        imports: { default: async | trappable },
    });
}

pub(super) mod call_context {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "call-context",
        imports: { default: async | trappable },
    });
}

impl call_context::pishoo::identity0_3_0::call_context::Host for StoreData {
    async fn caller_service(&mut self) -> wasmtime::Result<Option<(String, String, String)>> {
        Ok(self.caller_service.clone())
    }
}

impl identity_v2::pishoo::identity0_2_0::request_identity::Host for StoreData {
    async fn local(&mut self) -> wasmtime::Result<(String, String)> {
        let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(
            self.local.certificates(),
        )
        .map_err(|error| wasmtime::Error::msg(error.to_string()))?;
        Ok((
            self.local.name().to_owned(),
            ski.owner_hash().as_str().to_owned(),
        ))
    }

    async fn caller(&mut self) -> wasmtime::Result<Option<(String, String)>> {
        let Some(remote) = &self.remote else {
            return Ok(None);
        };
        let ski =
            dhttp_home::certificate::extract_dhttp_subject_key_identifier(remote.certificates())
                .map_err(|error| wasmtime::Error::msg(error.to_string()))?;
        Ok(Some((
            remote.name().to_owned(),
            ski.owner_hash().as_str().to_owned(),
        )))
    }
}

impl identity::pishoo::identity0_1_0::request_identity::Host for StoreData {
    async fn local_name(&mut self) -> String {
        self.local.name().to_owned()
    }

    async fn caller_name(&mut self) -> Option<String> {
        self.remote.as_ref().map(|remote| remote.name().to_owned())
    }

    async fn caller_is_owner(&mut self) -> bool {
        let Some(remote) = &self.remote else {
            return false;
        };
        let Some(local_name) = dhttp_home::normalize_name(self.local.name()) else {
            return false;
        };
        let Some(caller_name) = dhttp_home::normalize_name(remote.name()) else {
            return false;
        };
        if caller_name != local_name {
            return false;
        }
        let Ok(local) = dhttp_home::certificate::extract_dhttp_subject_key_identifier(
            self.local.certificates(),
        ) else {
            return false;
        };
        let Ok(remote) =
            dhttp_home::certificate::extract_dhttp_subject_key_identifier(remote.certificates())
        else {
            return false;
        };
        local.owner_hash() == remote.owner_hash()
    }
}

impl identity::pishoo::identity0_1_0::signatures::Host for StoreData {
    async fn sign(
        &mut self,
        data: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, identity::pishoo::identity0_1_0::signatures::SignError> {
        use identity::pishoo::identity0_1_0::signatures::SignError;
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
    ) -> std::result::Result<bool, identity::pishoo::identity0_1_0::signatures::VerifyError> {
        use identity::pishoo::identity0_1_0::signatures::VerifyError;
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

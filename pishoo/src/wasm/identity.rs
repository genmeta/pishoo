// Existing identity WIT bindings and capability-checked host operations.

mod identity {
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
        if !self.policy.sign {
            return Err(SignError::Denied);
        }
        if data.len() > 1024 * 1024 {
            return Err(SignError::InputTooLarge);
        }
        if self.outgoing.cancel.is_cancelled() {
            return Err(SignError::Unavailable);
        }
        let signature =
            dhttp::certificate::sign(&self.local, &data).map_err(|_| SignError::Failed)?;
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
        if !self.policy.verify {
            return Err(VerifyError::Unavailable);
        }
        if data.len() > 1024 * 1024 || signature.len() > 8192 {
            return Err(VerifyError::InputTooLarge);
        }
        if self.outgoing.cancel.is_cancelled() {
            return Err(VerifyError::Unavailable);
        }
        let name = dhttp_home::normalize_name(&name).ok_or(VerifyError::InvalidIdentity)?;
        if name == self.local.name() {
            return dhttp::certificate::verify_signature(
                self.local.public_key().as_ref(),
                &data,
                &signature,
            )
            .map_err(|_| VerifyError::Failed);
        }
        if let Some(remote) = &self.remote {
            if name == remote.name() {
                return dhttp::certificate::verify_signature(
                    remote.public_key().as_ref(),
                    &data,
                    &signature,
                )
                .map_err(|_| VerifyError::Failed);
            }
        }
        let endpoint = self
            .outgoing
            .endpoint
            .as_ref()
            .ok_or(VerifyError::Unavailable)?;
        let uri = format!("https://{name}/")
            .parse()
            .map_err(|_| VerifyError::InvalidIdentity)?;
        if self.outgoing.remaining_requests == 0
            || !outgoing_allowed(&self.outgoing.policy, &Method::GET, &uri)
        {
            return Err(VerifyError::Unavailable);
        }
        self.outgoing.remaining_requests -= 1;
        let remote = tokio::select! {
            biased;
            _ = self.outgoing.cancel.cancelled() => return Err(VerifyError::Unavailable),
            remote = dhttp::certificate::resolve_remote(endpoint, &name) => remote.map_err(|_| VerifyError::UnknownIdentity)?,
        };
        dhttp::certificate::verify_signature(remote.public_key().as_ref(), &data, &signature)
            .map_err(|_| VerifyError::Failed)
    }
}

use std::sync::Arc;

use derive_more::From;
use firewall_base::{
    action::ConnectionAction,
    expr::atomics::ConnectRequest,
    matcher::{DomainRulesMatcher, MatchRuleFailed},
};
use gm_quic::prelude::{
    AuthClient, ClientAgentVerifyResult, ClientNameVerifyResult, LocalAgent, RemoteAgent,
    handy::ToPrivateKey,
};
use rustls::{SignatureScheme, sign::SigningKey};
use snafu::{OptionExt as _, ResultExt as _};
use x509_parser::prelude::*;

use crate::error::Result;

#[derive(Debug, From, Clone)]
pub struct ClientAuther {
    firewall: Arc<DomainRulesMatcher>,
}

impl AuthClient for ClientAuther {
    fn verify_client_name(
        &self,
        host: &LocalAgent,
        client_name: Option<&str>,
    ) -> ClientNameVerifyResult {
        match self
            .firewall
            .match_rule(host.name(), &ConnectRequest::new(client_name))
        {
            Ok((rule_matched_domain, action)) => match action {
                ConnectionAction::Allow => ClientNameVerifyResult::Accept,
                ConnectionAction::Deny => {
                    tracing::info!(
                        target: "connect",
                        "Refuse client `{client_name}` connection to `{}` silently: matched access rule for domain `{rule_matched_domain}`",
                        host.name(),
                        client_name = client_name.unwrap_or("<anonymous>")
                    );
                    ClientNameVerifyResult::SilentRefuse("access firewall rules".to_owned())
                }
            },
            Err(MatchRuleFailed::MatchRuleInSet) | Err(MatchRuleFailed::MatchSet { .. }) => {
                ClientNameVerifyResult::Accept
            }
        }
    }

    fn verify_client_agent(
        &self,
        host: &LocalAgent,
        client_agent: &RemoteAgent,
    ) -> ClientAgentVerifyResult {
        let client_name = client_agent.name();
        let client_certs = client_agent.cert_chain();

        match parse_client_certs(client_certs).any(|cert| {
            extract_client_names(&cert)
                .filter_map(|name| name.replace('*', "\\w+").parse::<regex::Regex>().ok())
                .any(|regex| regex.is_match(client_name))
        }) {
            true => ClientAgentVerifyResult::Accept,
            false => {
                let reason = format!(
                    "Client name `{client_name}` does not match any names in the provided client certificates",
                );
                tracing::info!(target: "connect", "Refuse client `{client_name}` connection to {}: {reason}", host.name() );
                ClientAgentVerifyResult::Refuse(reason)
            }
        }
    }
}

fn parse_client_certs<'a>(
    certs: &'a [rustls::pki_types::CertificateDer<'_>],
) -> impl Iterator<Item = X509Certificate<'a>> {
    certs.iter().filter_map(|cert| {
        x509_parser::parse_x509_certificate(cert.as_ref())
            .ok()
            .map(|(_, cert)| cert)
    })
}

fn extract_client_names<'c>(cert: &'c X509Certificate<'c>) -> impl Iterator<Item = &'c str> {
    let common_names = cert
        .subject()
        .iter_common_name()
        .filter_map(|cn| cn.as_str().ok());

    let san = cert
        .extensions()
        .iter()
        .find(|ext| ext.oid == x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME)
        .and_then(|ext| x509_parser::extensions::SubjectAlternativeName::from_der(ext.value).ok());

    // Check Subject Alternative Names
    let sans = san.into_iter().flat_map(move |(_, san)| {
        san.general_names.into_iter().filter_map(|name| match name {
            x509_parser::prelude::GeneralName::DNSName(str) => Some(str),
            _ => None,
        })
    });

    common_names.chain(sans)
}

pub(super) fn load_key(path: &std::path::Path) -> Result<(Arc<dyn SigningKey>, SignatureScheme)> {
    let key_bytes = std::fs::read(path).whatever_context::<_, crate::error::Whatever>(format!(
        "Failed to read key file {}",
        path.display()
    ))?;
    let key_der = key_bytes.to_private_key();
    let key = rustls::crypto::ring::sign::any_supported_type(&key_der)
        .whatever_context::<_, crate::error::Whatever>("Unsupported key type")?;

    let supported_schemes = [
        SignatureScheme::ECDSA_NISTP256_SHA256,
        SignatureScheme::ECDSA_NISTP384_SHA384,
        SignatureScheme::ED25519,
        SignatureScheme::RSA_PSS_SHA256,
        SignatureScheme::RSA_PSS_SHA384,
        SignatureScheme::RSA_PSS_SHA512,
        SignatureScheme::RSA_PKCS1_SHA256,
        SignatureScheme::RSA_PKCS1_SHA384,
        SignatureScheme::RSA_PKCS1_SHA512,
    ];

    let scheme = supported_schemes
        .iter()
        .find(|&&scheme| key.choose_scheme(&[scheme]).is_some())
        .copied()
        .whatever_context::<_, crate::error::Whatever>(
            "No supported signature scheme found for key",
        )?;

    Ok((key, scheme))
}

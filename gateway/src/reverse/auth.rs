use std::sync::Arc;

use derive_more::From;
use firewall_base::{
    action::ConnectionAction,
    expr::atomics::ConnectRequest,
    matcher::{DomainRulesMatcher, MatchRuleFailed},
};
use gm_quic::prelude::{
    AuthClient, ClientAgentVerifyResult, ClientNameVerifyResult, LocalAgent, RemoteAgent,
};
use x509_parser::prelude::*;

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
                        client_name = client_name.unwrap_or("<anonymous>"),
                        host = host.name(),
                        matched_domain = %rule_matched_domain,
                        "silently refused client connection because an access rule matched"
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
                    "client name `{client_name}` does not match any names in the provided client certificates",
                );
                tracing::info!(
                    client_name,
                    host = host.name(),
                    reason,
                    "refused client connection"
                );
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

use std::sync::Arc;

use derive_more::From;
use firewall_base::{
    action::ConnectionAction,
    expr::atomics::ConnectRequest,
    matcher::{DomainRulesMatcher, MatchRuleFailed},
};
use gm_quic::{AuthClient, ClientCertsVerifyResult, ClientNameVerifyResult};
use x509_parser::prelude::*;

#[derive(Debug, From, Clone)]
pub struct ClientAuther {
    firewall: Arc<DomainRulesMatcher>,
}

impl AuthClient for ClientAuther {
    fn verify_client_name(&self, host: &str, client_name: Option<&str>) -> ClientNameVerifyResult {
        match self
            .firewall
            .match_rule(host, &ConnectRequest::new(client_name))
        {
            Ok((rule_matched_domain, action)) => match action {
                ConnectionAction::Allow => ClientNameVerifyResult::Accept,
                ConnectionAction::Deny => {
                    tracing::info!(
                        target: "connect",
                        "Refuse client `{client_name}` connection to `{host}` silently: matched access rule for domain `{rule_matched_domain}`",
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

    fn verify_client_certs(
        &self,
        host: &str,
        client_name: Option<&str>,
        client_certs: &[u8],
    ) -> ClientCertsVerifyResult {
        let Some(client_name) = client_name else {
            return ClientCertsVerifyResult::Accept; // 不需要验证client name和证书是否匹配
        };
        match parse_client_certs(client_certs).any(|cert| {
            extract_client_names(&cert)
                .filter_map(|name| name.replace('*', "\\w+").parse::<regex::Regex>().ok())
                .any(|regex| regex.is_match(client_name))
        }) {
            true => ClientCertsVerifyResult::Accept,
            false => {
                let reason = format!(
                    "Client name `{client_name}` does not match any names in the provided client certificates",
                );
                tracing::info!(target: "connect", "Refuse client `{client_name}` connection to {host}: {reason}" );
                ClientCertsVerifyResult::Refuse(reason)
            }
        }
    }
}

fn parse_client_certs(cert: &'_ [u8]) -> impl Iterator<Item = X509Certificate<'_>> {
    let mut rest = cert;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        match x509_parser::parse_x509_certificate(rest) {
            Ok((r, cert)) => {
                rest = r;
                Some(cert)
            }
            Err(_) => None,
        }
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

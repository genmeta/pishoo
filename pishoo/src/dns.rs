//! Process-level resolver installation and maintenance of actual DNS resources.
use std::{collections::HashMap, io, net::SocketAddr, sync::Arc, time::Duration};

use ddns::{
    H3Resolver,
    mdns::{MdnsBinding, MdnsResolverSet},
};
use dhttp::{Endpoint, resolve as qresolve};
use qprotocol::{AddressBook, Dock};
use tokio::time::Instant;

const PUBLISH_TIMEOUT: Duration = Duration::from_secs(3);
const MAINTENANCE_RETRY: Duration = Duration::from_secs(5);
const PUBLISH_INTERVAL: Duration = Duration::from_secs(20);

pub(crate) fn install() -> io::Result<MdnsResolverSet> {
    let origin = ddns::resolvers::DHTTP_NAME_SERVICE
        .parse()
        .map_err(io::Error::other)?;
    let h3 = H3Resolver::anonymous(origin).map_err(io::Error::other)?;
    let mdns = MdnsResolverSet::new(ddns::resolvers::DHTTP_MDNS_SERVICE_DOMAIN);
    qresolve::Resolver::add(Arc::new(qresolve::SystemResolver));
    qresolve::Resolver::add(Arc::new(h3));
    qresolve::Resolver::add(Arc::new(mdns.clone()));
    Ok(mdns)
}

pub(crate) fn authority(endpoint: &Endpoint) -> io::Result<qtls::LocalAuthority> {
    let authority = endpoint.local_authority().map_err(io::Error::other)?;
    let ski =
        dhttp_home::certificate::extract_dhttp_subject_key_identifier(authority.certificates())
            .map_err(io::Error::other)?;
    if ski.chain().usage() != dhttp_home::certificate::CertificateUsage::ClientAndServer {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{}: listening requires ClientAndServer publishing credentials",
                endpoint.name()
            ),
        ));
    }
    Ok(authority)
}

pub(crate) fn publisher(endpoint: &Endpoint) -> io::Result<Arc<H3Resolver>> {
    authority(endpoint)?;
    let origin = ddns::resolvers::DHTTP_NAME_SERVICE
        .parse()
        .map_err(io::Error::other)?;
    Ok(Arc::new(
        H3Resolver::new(origin, endpoint).map_err(io::Error::other)?,
    ))
}

pub(crate) async fn maintain_mdns(
    mdns: &MdnsResolverSet,
    endpoints: &[Endpoint],
    removed_bounds: &[SocketAddr],
) -> io::Result<()> {
    let mut error = None;
    // A binding can be removed and recreated with the same name/IP before this batch.
    // Retire its former mDNS resource before comparing the current snapshot.
    for instance in mdns.snapshot() {
        if removed_bounds
            .iter()
            .any(|bound| bound.ip() == instance.bound_ip())
        {
            let binding = MdnsBinding::new(instance.bound_device(), instance.bound_ip());
            if let Err(failed) = mdns.remove(&binding).await {
                let failed = io::Error::other(failed);
                tracing::warn!(
                    device = binding.device(),
                    ip = %binding.ip(),
                    error = %failed,
                    "mDNS remove failed"
                );
                error.get_or_insert(failed);
            }
        }
    }
    let addresses = AddressBook::global();
    let mut desired = HashMap::<MdnsBinding, Vec<qresolve::EndpointAddr>>::new();
    for (bound, device) in addresses.inner_bindings() {
        let Some(socket) = Dock::global().find_socket(bound) else {
            continue;
        };
        if socket.local_addr().ok() != Some(bound) || socket.bound_device() != Some(&device) {
            continue;
        }
        desired
            .entry(MdnsBinding::new(device.name(), bound.ip()))
            .or_default()
            .extend(addresses.mdns_endpoints(bound).iter().copied());
    }
    for values in desired.values_mut() {
        values.retain(|address| {
            address.scope() == Some(dhttp::Scope::Internal) && address.addr().port() != 0
        });
        values.sort_unstable();
        values.dedup();
    }
    desired.retain(|_, values| !values.is_empty());
    for instance in mdns.snapshot() {
        let binding = MdnsBinding::new(instance.bound_device(), instance.bound_ip());
        if !desired.contains_key(&binding) {
            if let Err(failed) = mdns.remove(&binding).await {
                let failed = io::Error::other(failed);
                tracing::warn!(
                    device = binding.device(),
                    ip = %binding.ip(),
                    error = %failed,
                    "mDNS remove failed"
                );
                error.get_or_insert(failed);
            }
        }
    }
    for (binding, values) in desired {
        let instance = match mdns.upsert(binding.clone()).await {
            Ok(instance) => instance,
            Err(failed) => {
                let failed = io::Error::other(failed);
                tracing::warn!(
                    device = binding.device(),
                    ip = %binding.ip(),
                    error = %failed,
                    "mDNS bind failed"
                );
                error.get_or_insert(failed);
                continue;
            }
        };
        for endpoint in endpoints {
            let result = authority(endpoint).and_then(|identity| {
                instance
                    .publish_endpoints(&identity, endpoint.name(), values.iter().copied())
                    .map_err(io::Error::other)
            });
            if let Err(failed) = result {
                tracing::warn!(
                    identity = endpoint.name(),
                    device = binding.device(),
                    ip = %binding.ip(),
                    error = %failed,
                    "mDNS publish failed"
                );
                error.get_or_insert(failed);
            }
        }
    }
    error.map_or(Ok(()), Err)
}

pub(crate) async fn publish(
    name: String,
    publisher: Arc<H3Resolver>,
    addresses: Arc<[qresolve::EndpointAddr]>,
) -> Option<Instant> {
    // Stop renewing absent addresses; existing records expire with their lease.
    if addresses.is_empty() {
        return None;
    }
    let started = Instant::now();
    match tokio::time::timeout(
        PUBLISH_TIMEOUT,
        publisher.publish_endpoints(&name, addresses.iter().copied()),
    )
    .await
    {
        Ok(Ok(_)) => Some(started + PUBLISH_INTERVAL),
        Ok(Err(error)) => {
            tracing::warn!(identity = %name, ?error, "DNS publish failed");
            Some(Instant::now() + MAINTENANCE_RETRY)
        }
        Err(error) => {
            tracing::warn!(identity = %name, %error, "DNS publish timed out");
            Some(Instant::now() + MAINTENANCE_RETRY)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(kind: u8) -> Endpoint {
        let name = "dns-authority-test.dhttp.net";
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
        let ski = format!("1:{kind}:{}", "0123456789abcdef".repeat(4));
        let mut extension = vec![0x04, ski.len() as u8];
        extension.extend_from_slice(ski.as_bytes());
        params
            .custom_extensions
            .push(rcgen::CustomExtension::from_oid_content(
                &[2, 5, 29, 14],
                extension,
            ));
        let cert = params.self_signed(&key).unwrap();
        Endpoint::new(
            qbase::endpoint::Endpoint::new(
                name,
                vec![cert.der().clone()],
                qtls::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
                vec![1],
            )
            .unwrap(),
        )
    }

    #[test]
    fn publication_requires_primary_credentials() {
        let primary = endpoint(0);
        assert_eq!(authority(&primary).unwrap().name(), primary.name());
        assert!(publisher(&primary).is_ok());
        assert_eq!(
            authority(&endpoint(1)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(publisher(&endpoint(1)).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn failed_publication_schedules_a_finite_retry() {
        let endpoint = endpoint(0);
        let publisher =
            Arc::new(H3Resolver::new("https://dns.invalid/".parse().unwrap(), &endpoint).unwrap());
        let started = Instant::now();
        // Unit tests do not install a network; the error must schedule maintenance.
        let due = publish(
            endpoint.name().to_owned(),
            publisher,
            Arc::from(["127.0.0.1:8080".parse::<SocketAddr>().unwrap().into()]),
        )
        .await
        .unwrap();
        assert_eq!(due, started + MAINTENANCE_RETRY);
    }

    #[tokio::test]
    async fn empty_publication_stops_renewal_without_network() {
        let endpoint = endpoint(0);
        let publisher =
            Arc::new(H3Resolver::new("https://dns.invalid/".parse().unwrap(), &endpoint).unwrap());
        assert_eq!(
            publish(endpoint.name().to_owned(), publisher, Arc::from([])).await,
            None
        );
    }

    #[tokio::test]
    async fn empty_mdns_maintenance_needs_no_network() {
        let mdns = MdnsResolverSet::new(ddns::resolvers::DHTTP_MDNS_SERVICE_DOMAIN);
        maintain_mdns(&mdns, &[], &[]).await.unwrap();
        mdns.shutdown().await.unwrap();
    }
}

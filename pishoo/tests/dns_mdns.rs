//! Actual mDNS resource reconciliation. Run explicitly on a machine with a LAN interface.
#[path = "../src/dns.rs"]
#[allow(dead_code)]
mod dns;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use dhttp::{
    Endpoint,
    resolve::{Family, Resolve, Source},
};
use futures::StreamExt;
use qprotocol::{AddressBook, Dock, UdpSocket};

#[tokio::test]
#[ignore = "requires an internal IPv4 interface and local UDP/multicast socket permission"]
async fn merges_quic_ports_rebuilds_removed_bindings_and_removes_names() {
    let mdns = dns::install().unwrap();
    dhttp::DhttpNetwork::init().await.unwrap();
    let addresses = AddressBook::global();
    let bindings = addresses.inner_bindings();
    let (bound, device) = bindings
        .iter()
        .filter(|(bound, _)| bound.is_ipv4())
        .find(|(_, device)| device.name() == "en0" || device.name() == "eth0")
        .or_else(|| bindings.iter().find(|(bound, _)| bound.is_ipv4()))
        .expect("an internal IPv4 interface");
    let mut sockets = Vec::new();
    for _ in 0..2 {
        let socket = Arc::new(
            UdpSocket::bind_to_device(SocketAddr::new(bound.ip(), 0), device.clone()).unwrap(),
        );
        Dock::global().add(socket.clone()).unwrap().unwrap();
        addresses
            .insert_inner(&socket, socket.local_addr().unwrap().into())
            .unwrap();
        sockets.push(socket);
    }
    let sockets = scopeguard::guard(sockets, |sockets| {
        for socket in sockets {
            AddressBook::global().remove_bound(socket.local_addr().unwrap());
            Dock::global().remove(&socket);
        }
    });
    let name = format!("pishoo-mdns-{}.dhttp.net", std::process::id());
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec![name.clone()]).unwrap();
    let ski = format!("1:0:{}", "0123456789abcdef".repeat(4));
    let mut extension = vec![0x04, ski.len() as u8];
    extension.extend_from_slice(ski.as_bytes());
    params
        .custom_extensions
        .push(rcgen::CustomExtension::from_oid_content(
            &[2, 5, 29, 14],
            extension,
        ));
    let cert = params.self_signed(&key).unwrap();
    let endpoint = Endpoint::new(
        qbase::endpoint::Endpoint::new(
            &name,
            vec![cert.der().clone()],
            qtls::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
            vec![1],
        )
        .unwrap(),
    );
    let listener = endpoint
        .listen(
            dhttp::Scope::Internal.into(),
            tower::service_fn(|_: http::Request<dhttp::Body>| async {
                Ok::<_, std::convert::Infallible>(http::Response::new(http_body_util::Empty::<
                    bytes::Bytes,
                >::new()))
            }),
        )
        .await
        .unwrap();
    if let Err(error) = dns::maintain_mdns(&mdns, std::slice::from_ref(&endpoint), &[]).await {
        eprintln!("another interface could not join mDNS: {error}");
    }
    let instance = mdns
        .snapshot()
        .into_iter()
        .find(|instance| {
            instance.bound_device() == device.name() && instance.bound_ip() == bound.ip()
        })
        .unwrap();
    // DNS spelling normalization preserves the certificate sequence.
    let records = tokio::time::timeout(Duration::from_secs(2), async {
        mdns.lookup(
            &format!("{}.:1", name.to_ascii_uppercase()),
            "",
            Some(Family::V4),
        )
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
    })
    .await
    .unwrap();
    for socket in sockets.iter() {
        let address = socket.local_addr().unwrap();
        assert!(records.iter().any(|(source, endpoint)| *source
            == Source::Mdns {
                nic: device.name().into(),
                family: Family::V4,
            }
            && endpoint.addr() == address));
    }
    // Even if the same IP remains, an explicitly removed binding retires the old resource.
    let removed = sockets[0].local_addr().unwrap();
    addresses.remove_bound(removed);
    Dock::global().remove(&sockets[0]);
    if let Err(error) = dns::maintain_mdns(&mdns, std::slice::from_ref(&endpoint), &[removed]).await
    {
        eprintln!("another interface could not join mDNS: {error}");
    }
    let replacement = mdns
        .snapshot()
        .into_iter()
        .find(|instance| {
            instance.bound_device() == device.name() && instance.bound_ip() == bound.ip()
        })
        .unwrap();
    assert!(!Arc::ptr_eq(&instance, &replacement));
    let records = replacement
        .lookup(&name, "", Some(Family::V4))
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(
        !records
            .iter()
            .any(|(_, endpoint)| endpoint.addr() == removed)
    );
    assert!(
        records
            .iter()
            .any(|(_, endpoint)| endpoint.addr() == sockets[1].local_addr().unwrap())
    );
    for instance in mdns.snapshot() {
        instance.remove_name(endpoint.name());
    }
    assert!(
        replacement
            .lookup(&name, "", Some(Family::V4))
            .await
            .is_err()
    );
    // Query resources still exist without listening identities.
    dns::maintain_mdns(&mdns, &[], &[]).await.ok();
    assert!(!mdns.snapshot().is_empty());
    drop(listener);
    mdns.shutdown().await.unwrap();
    assert!(mdns.snapshot().is_empty());
}

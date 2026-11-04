//! Publish and renew test service records through the real signed H3 DNS API.
use std::{error::Error, sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> Result<(),Box<dyn Error+Send+Sync>> {
    let home=std::env::var_os("DHTTP_HOME").ok_or("isolated DHTTP_HOME required")?;
    if !std::path::Path::new(&home).is_absolute(){return Err("absolute test home required".into());}
    dhttp::resolve::Resolver::add(Arc::new(dhttp::resolve::SystemResolver));
    dhttp::DhttpNetwork::init().await?;
    let home=dhttp_home::DhttpHome::load(dhttp_home::HomeScope::User)?;
    let mut services=Vec::new();
    for (name,port) in [("host.sandbox.dhttp.net",13445),("store.dhttp.net",13444)] {
        let endpoint=dhttp::Endpoint::load_from(home.identity_profile(name)?.path()).await?;
        let publisher=ddns::H3Resolver::new("https://localhost:19443".parse()?,&endpoint)?;
        services.push((name,port,publisher));
    }
    loop {
        for (name,port,publisher) in &services {
            if let Err(error)=publisher.publish_endpoints(name,[dhttp::resolve::EndpointAddr::direct(([127,0,0,1],*port).into())]).await { eprintln!("DNS publication for {name}: {error}"); }
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

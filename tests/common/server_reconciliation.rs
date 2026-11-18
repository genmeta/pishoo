//! Keep isolated discovery scenarios supplied with a device authority database.
use std::{collections::BTreeMap, sync::Arc};

use dhttp_home::DhttpHome;

use crate::{Result, WasmRuntime, server::Server};

pub(super) async fn reconcile_profiles(
    mdns: &ddns::mdns::MdnsResolverSet,
    home: &DhttpHome,
    user_homes: Option<&[std::path::PathBuf]>,
    servers: &mut BTreeMap<String, Server>,
    runtime: Arc<WasmRuntime>,
) -> Result<bool> {
    // Discovery tests deliberately remove or invalidate the home being scanned.
    let device = tempfile::tempdir()?;
    let database = crate::system::open(&DhttpHome::new(device.path().into()))?;
    crate::server::run::reconcile_profiles(mdns, home, user_homes, servers, runtime, &database)
        .await
}

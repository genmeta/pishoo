use std::sync::Arc;

use gateway::{parse::config::ResolvedAccessLogConfig, reverse::log::AccessLogOutput};

use super::source::ListenerSpec;

#[derive(Clone, Debug)]
pub struct AccessLogResourcePlan {
    pub server: ResolvedAccessLogConfig,
    pub locations: Box<[ResolvedAccessLogConfig]>,
}

#[derive(Clone, Debug)]
pub struct AccessLogResources {
    pub server: Option<Arc<AccessLogOutput>>,
    pub locations: Box<[Option<Arc<AccessLogOutput>>]>,
}

pub struct ServerResources<L> {
    listener: Option<L>,
    listener_spec: ListenerSpec,
    access_logs: AccessLogResources,
}

impl<L> ServerResources<L> {
    pub fn new(listener: L, listener_spec: ListenerSpec, access_logs: AccessLogResources) -> Self {
        Self {
            listener: Some(listener),
            listener_spec,
            access_logs,
        }
    }

    pub fn listener_spec(&self) -> &ListenerSpec {
        &self.listener_spec
    }

    pub fn take_listener(&mut self) -> L {
        self.listener
            .take()
            .expect("a stopped server owns its listener")
    }

    pub fn put_listener(&mut self, listener: L) {
        assert!(
            self.listener.replace(listener).is_none(),
            "listener returned exactly once"
        );
    }

    pub fn replace_access_logs(&mut self, access_logs: AccessLogResources) {
        self.access_logs = access_logs;
    }

    pub fn access_logs(&self) -> &AccessLogResources {
        &self.access_logs
    }
}

use std::collections::BTreeMap;

use access_control::{
    Effect, GrantedAccess, GrantedMethods, Method as AccessMethod, RequestedAccess,
};
use http::Method;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CapabilityEndpoint {
    pub(crate) method: &'static str,
    pub(crate) path: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChatCapability;

impl ChatCapability {
    pub(crate) const fn id(self) -> &'static str {
        "chat"
    }

    pub(crate) const fn version(self) -> &'static str {
        "1"
    }

    pub(crate) const fn endpoints(self) -> &'static [CapabilityEndpoint] {
        &[CapabilityEndpoint {
            method: "POST",
            path: "/std/message",
        }]
    }

    pub(crate) fn requested_access(self) -> RequestedAccess {
        BTreeMap::from([(
            String::from("/std/message"),
            vec![AccessMethod::Specified(Method::POST)],
        )])
    }

    pub(crate) fn offers(self) -> GrantedAccess {
        BTreeMap::from([(
            String::from("/std/message"),
            GrantedMethods {
                allow: vec![AccessMethod::Specified(Method::POST)],
                review: Vec::new(),
                deny: Vec::new(),
            },
        )])
    }

    pub(crate) fn fixed_rules(self) -> [(AccessMethod, &'static str, Effect); 1] {
        [(
            AccessMethod::Specified(Method::POST),
            "/std/message",
            Effect::Allow,
        )]
    }

    #[allow(dead_code)]
    pub(crate) fn matches(self, method: &Method, path: &str) -> bool {
        self.endpoints()
            .iter()
            .any(|endpoint| endpoint.method == method.as_str() && endpoint.path == path)
    }
}

#[cfg(test)]
mod tests {
    use http::Method;

    use super::{AccessMethod, ChatCapability};

    #[test]
    fn chat_is_contact_scoped_and_uses_push_delivery_only() {
        let chat = ChatCapability;
        assert_eq!(chat.id(), "chat");
        assert!(chat.matches(&Method::POST, "/std/message"));
        assert!(!chat.matches(&Method::GET, "/std/message"));
        assert!(!chat.matches(&Method::DELETE, "/std/message"));
    }

    #[test]
    fn chat_descriptor_exposes_fixed_request_offer_and_rules() {
        let chat = ChatCapability;
        let requested = chat.requested_access();
        assert_eq!(
            requested["/std/message"],
            vec![AccessMethod::Specified(Method::POST)]
        );
        assert_eq!(
            chat.offers()["/std/message"].allow,
            vec![AccessMethod::Specified(Method::POST)]
        );
        assert!(chat.fixed_rules().iter().all(|(_, path, effect)| {
            *path == "/std/message" && *effect == access_control::Effect::Allow
        }));
    }
}

#![allow(dead_code)] // Public profile remains a Workspace-owned presentation capability.

use std::sync::Arc;

use access_control::{GrantedAccess, RequestedAccess};
use axum::{Extension, Json, extract::State};
use http::{Method, StatusCode};
use serde::Serialize;

use super::Workspace;
use crate::chat::CHAT_CAPABILITY;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CapabilityVisibility {
    Public,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CapabilityApproval {
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CapabilityEndpoint {
    pub(crate) method: &'static str,
    pub(crate) path: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CapabilityDescriptor {
    pub(crate) id: &'static str,
    pub(crate) version: &'static str,
    pub(crate) visibility: &'static str,
    pub(crate) approval_mode: &'static str,
    pub(crate) selectable: bool,
    pub(crate) endpoints: Vec<CapabilityEndpoint>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BuiltInCapability {
    PublicProfile,
}

impl BuiltInCapability {
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::PublicProfile => "public_profile",
        }
    }

    pub(crate) const fn version(self) -> &'static str {
        "1"
    }

    pub(crate) const fn visibility(self) -> CapabilityVisibility {
        match self {
            Self::PublicProfile => CapabilityVisibility::Public,
        }
    }

    pub(crate) const fn approval(self) -> CapabilityApproval {
        match self {
            Self::PublicProfile => CapabilityApproval::None,
        }
    }

    pub(crate) const fn endpoints(self) -> &'static [CapabilityEndpoint] {
        match self {
            Self::PublicProfile => &[
                CapabilityEndpoint {
                    method: "GET",
                    path: "/std/profile",
                },
                CapabilityEndpoint {
                    method: "GET",
                    path: "/std/profile/avatar",
                },
            ],
        }
    }

    pub(crate) fn matches(self, method: &Method, path: &str) -> bool {
        self.endpoints()
            .iter()
            .any(|endpoint| endpoint.method == method.as_str() && endpoint.path == path)
    }

    pub(crate) fn public_endpoint(method: &Method, path: &str) -> bool {
        Self::PublicProfile.matches(method, path)
    }
}

pub(crate) fn descriptors() -> Vec<CapabilityDescriptor> {
    let public = BuiltInCapability::PublicProfile;
    vec![
        CapabilityDescriptor {
            id: public.id(),
            version: public.version(),
            visibility: "public",
            approval_mode: "none",
            selectable: false,
            endpoints: public.endpoints().to_vec(),
        },
        CapabilityDescriptor {
            id: CHAT_CAPABILITY.id(),
            version: CHAT_CAPABILITY.version(),
            visibility: "contact",
            approval_mode: "capability",
            selectable: true,
            endpoints: CHAT_CAPABILITY
                .endpoints()
                .iter()
                .map(|endpoint| CapabilityEndpoint {
                    method: endpoint.method,
                    path: endpoint.path,
                })
                .collect(),
        },
    ]
}

pub(crate) fn requested_access_for(id: &str) -> Option<RequestedAccess> {
    (id == CHAT_CAPABILITY.id()).then(|| CHAT_CAPABILITY.requested_access())
}

pub(crate) fn offered_access_for(id: &str) -> Option<GrantedAccess> {
    (id == CHAT_CAPABILITY.id()).then(|| CHAT_CAPABILITY.offers())
}

pub(crate) async fn list(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<access_control::Visitor>>,
) -> Result<Json<Vec<CapabilityDescriptor>>, StatusCode> {
    state
        .owner
        .ensure(visitor.as_ref().map(|extension| &extension.0))?;
    Ok(Json(descriptors()))
}

#[cfg(test)]
mod tests {
    use http::Method;

    use super::{BuiltInCapability, CapabilityApproval, CapabilityVisibility};

    #[test]
    fn public_profile_contains_only_exact_public_get_endpoints() {
        let public = BuiltInCapability::PublicProfile;
        assert_eq!(public.id(), "public_profile");
        assert_eq!(public.visibility(), CapabilityVisibility::Public);
        assert_eq!(public.approval(), CapabilityApproval::None);
        assert!(public.matches(&Method::GET, "/std/profile"));
        assert!(public.matches(&Method::GET, "/std/profile/avatar"));
        assert!(!public.matches(&Method::POST, "/std/profile"));
        assert!(!public.matches(&Method::GET, "/std/profile/other"));
        assert!(!public.matches(&Method::GET, "/std/profile?name=other"));
    }

    #[test]
    fn public_endpoint_check_does_not_bypass_other_paths() {
        assert!(BuiltInCapability::public_endpoint(
            &Method::GET,
            "/std/profile"
        ));
        assert!(BuiltInCapability::public_endpoint(
            &Method::GET,
            "/std/profile/avatar"
        ));
        assert!(!BuiltInCapability::public_endpoint(
            &Method::POST,
            "/std/profile"
        ));
        assert!(!BuiltInCapability::public_endpoint(
            &Method::GET,
            "/std/profile/avatar/other"
        ));
    }

    #[test]
    fn capability_registry_keeps_public_profile_and_chat_isolated() {
        let descriptors = super::descriptors();
        assert_eq!(descriptors.len(), 2);
        assert_eq!(descriptors[0].id, "public_profile");
        assert_eq!(descriptors[1].id, "chat");
        assert_eq!(descriptors[0].visibility, "public");
        assert_eq!(descriptors[1].visibility, "contact");
        assert!(
            descriptors[0]
                .endpoints
                .iter()
                .all(|endpoint| endpoint.path.starts_with("/std/profile"))
        );
        assert_eq!(descriptors[1].endpoints.len(), 1);
        assert_eq!(descriptors[1].endpoints[0].method, "POST");
        assert_eq!(descriptors[1].endpoints[0].path, "/std/message");
        assert!(super::requested_access_for("public_profile").is_none());
        assert!(super::offered_access_for("public_profile").is_none());
        assert!(
            super::requested_access_for("chat")
                .expect("chat request mapping")
                .contains_key("/std/message")
        );
        assert!(
            super::offered_access_for("chat")
                .expect("chat offer mapping")
                .contains_key("/std/message")
        );
    }
}

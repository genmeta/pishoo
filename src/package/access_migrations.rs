use crate::{Error, Result, sandbox::Lib};

// The first store transition has exactly one supported interpretation. Treat
// these values as a closed contract before allowing an installer to use them.
pub(super) fn validate_declaration(spec: &oas3::OpenApiV3Spec) -> Result<()> {
    let Some(mappings) = spec.extensions.get("pishoo-access-migrations") else {
        return Ok(());
    };
    let expected = serde_json::json!([{
        "source": {"package":"std/comm/group","version":"0.1.4","method":"POST","path":"/comm/group/{topic}/broadcast"},
        "target": {"package":"std/comm/message","version":"0.3.8","operationId":"submitConversationMessage"},
        "role": "active_group_member",
        "bindings": {
            "name": {"owner":"resource_owner","owner_subject":"local_identity.subject","topic":"path.topic"},
            "speaker":"authenticated_peer",
            "member_period":"active_membership.period"
        }
    }]);
    if spec.info.version != "0.1.5" || mappings != &expected {
        return Err(Error::InvalidComponent(
            "unsupported access migration or incomplete parameter binding".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_dependency<'a>(
    lib: &Lib,
    resolve: impl FnOnce(&(String, String)) -> Option<&'a Lib>,
) -> Result<()> {
    if !lib
        .openapi
        .extensions
        .contains_key("pishoo-access-migrations")
    {
        return Ok(());
    }
    let invalid = || {
        Error::InvalidComponent("group broadcast migration requires the official Group and its exact Message submission dependency".into())
    };
    let key = lib
        .dependencies
        .get("std/comm/message")
        .ok_or_else(invalid)?;
    if lib.id != "dev.genmeta.tech.dhttp.net/std/comm/group"
        || key.0 != "dev.genmeta.tech.dhttp.net/std/comm/message"
        || key.1 != "0.3.8"
    {
        return Err(invalid());
    }
    let target = resolve(key).ok_or_else(invalid)?;
    let operation = target
        .openapi
        .paths
        .as_ref()
        .and_then(|paths| paths.get("/comm/message/{name}"))
        .and_then(|item| item.post.as_ref())
        .and_then(|post| post.operation_id.as_deref());
    if target.openapi.info.version != key.1 || operation != Some("submitConversationMessage") {
        return Err(invalid());
    }
    Ok(())
}

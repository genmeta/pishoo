use std::{self, sync::Arc};

use bytes::Bytes;
use firewall_base::pattern::{LocationPattern, LocationPatternKind};
use h3::server::RequestStream;
use h3_shim::{RecvStream, SendStream};
use http::Request;
use snafu::ResultExt;

use crate::{
    error::{Result, StreamSnafu},
    h3::{H3Sink, H3Stream},
    parse::Node,
};

/// ``` conf
/// location /ssh {
///     ssh_login basic ssl; # ssl 需要结合防火墙使用
///     ssh_deny root;
/// }
/// ```
///
/// 配置精确locations匹配，既可免密登录
/// ``` shell
/// access domain "ssh.api.server" "= /ssh/ubuntu" allow "*.admin.api.server"
/// ```
pub async fn serve(
    location: &Arc<Node>,
    final_pattern: String,
    rule_set: Option<&LocationPattern>,
    request: Request<()>,
    client_name: String,
    recver: RequestStream<RecvStream, Bytes>,
    sender: RequestStream<SendStream<Bytes>, Bytes>,
) -> Result<()> {
    let Some(crate::parse::Value::StringVec(ssh_login)) = location.get("ssh_login") else {
        unreachable!()
    };

    let ssh_deny = location
        .get("ssh_deny")
        .map(|v| {
            let crate::parse::Value::StringVec(vec) = v else {
                unreachable!()
            };
            vec.to_owned()
        })
        .unwrap_or_default();

    let config = genmeta_ssh3_server::Config {
        ssh_login: ssh_login.to_owned(),
        ssh_deny,
    };

    // TODO: 暂时使用了一个很粗糙的方案来融合模块
    // 自研HTTP3迫在眉睫
    genmeta_ssh3_server::serve::<_, _, _, H3Sink, H3Stream>(
        Arc::new(config),
        request,
        final_pattern,
        rule_set.is_some_and(|pat| matches!(pat.kind(), LocationPatternKind::Exact)),
        client_name,
        recver,
        sender,
        async |sender, response| sender.send_response(response).await.context(StreamSnafu),
    )
    .await
}

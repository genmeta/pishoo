use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use firewall_base::pattern::{LocationPattern, LocationPatternKind};
use futures::{Stream, StreamExt};
use h3x::message::stream::{
    BoxMessageStreamReader, BoxMessageStreamWriter, ReadStream, WriteStream,
};
use http::Request;
use snafu::{Report, ResultExt};

use crate::{
    error::{Result, StreamSnafu},
    parse::Node,
    reverse::log::RequestInfo,
};

struct IoMessageStreamReader(Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>);

impl From<ReadStream> for IoMessageStreamReader {
    fn from(value: ReadStream) -> Self {
        let reader: BoxMessageStreamReader<'static> = value.into_box_reader();
        Self(Box::pin(
            reader.map(|result| result.map_err(io::Error::from)),
        ))
    }
}

impl Stream for IoMessageStreamReader {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().0.as_mut().poll_next(cx)
    }
}

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
    recver: ReadStream,
    sender: WriteStream,
) -> Result<()> {
    let req_info = RequestInfo::from_request(&request);

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

    let result = genmeta_ssh3_server::serve::<
        _,
        _,
        _,
        BoxMessageStreamWriter<'static>,
        IoMessageStreamReader,
    >(
        Arc::new(config),
        request,
        final_pattern,
        rule_set.is_some_and(|pat| matches!(pat.kind(), LocationPatternKind::Exact)),
        client_name,
        recver,
        sender,
        async |sender, response| {
            let (parts, _body) = response.into_parts();
            sender
                .send_hyper_response_parts(parts)
                .await
                .context(StreamSnafu)
        },
    )
    .await;

    match &result {
        Ok(()) => {
            req_info.log_access(200, 0).await;
        }
        Err(e) => {
            req_info.log_error(Report::from_error(&e)).await;
            req_info.log_access(500, 0).await;
        }
    }

    result
}

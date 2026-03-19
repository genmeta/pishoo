use std::{
    self, io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use firewall_base::pattern::{LocationPattern, LocationPatternKind};
use futures::Stream;
use h3x::message::stream::{ReadStream, WriteStream};
use http::Request;
use snafu::{Report, ResultExt};

use crate::{
    error::{Result, StreamSnafu},
    parse::Node,
    reverse::log::RequestInfo,
};

/// Newtype wrapper around ReadStream that implements `From<ReadStream>` and `Stream`
///
/// Needed because `genmeta_ssh3_server::serve` requires `St: From<R> + Stream<Item=Result<Bytes, io::Error>>`
pub struct H3ReadStreamWrapper {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send + 'static>>,
}

impl From<ReadStream> for H3ReadStreamWrapper {
    fn from(read_stream: ReadStream) -> Self {
        use futures::TryStreamExt;
        let stream = read_stream
            .into_bytes_stream()
            .map_err(|e| io::Error::from(e));
        Self {
            inner: Box::pin(stream),
        }
    }
}

impl Stream for H3ReadStreamWrapper {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl Unpin for H3ReadStreamWrapper {}

/// Newtype wrapper around WriteStream that implements `From<WriteStream>` and `AsyncWrite`
///
/// Needed because `genmeta_ssh3_server::serve` requires `Si: From<W> + AsyncWrite`
pub struct H3WriteStreamWrapper {
    inner: Pin<Box<dyn tokio::io::AsyncWrite + Send + 'static>>,
}

impl From<WriteStream> for H3WriteStreamWrapper {
    fn from(write_stream: WriteStream) -> Self {
        Self {
            inner: Box::pin(write_stream.into_writer()),
        }
    }
}

impl tokio::io::AsyncWrite for H3WriteStreamWrapper {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        self.inner.as_mut().poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.inner.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        self.inner.as_mut().poll_shutdown(cx)
    }
}

impl Unpin for H3WriteStreamWrapper {}

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

    let result = genmeta_ssh3_server::serve::<_, _, _, H3WriteStreamWrapper, H3ReadStreamWrapper>(
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
            req_info
                .log_error(Report::from_error(&e).to_string())
                .await;
            req_info.log_access(500, 0).await;
        }
    }

    result
}

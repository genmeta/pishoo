use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Buf;
use dhttp::log::access::{
    AccessRequestTarget, BodyBytesEmitted, ClientAddress, OptionalReferer, OptionalUserAgent,
};
use http_body::{Body, Frame};

use crate::reverse::log::AccessLogOutput;

pub(super) struct AccessRecordSeed {
    pub client: ClientAddress,
    pub method: http::Method,
    pub target: AccessRequestTarget,
    pub version: http::Version,
    pub referer: OptionalReferer,
    pub user_agent: OptionalUserAgent,
    pub status: http::StatusCode,
}

pub(super) struct AccessLogBody<B> {
    body: B,
    output: Arc<AccessLogOutput>,
    seed: Option<AccessRecordSeed>,
    bytes: BodyBytesEmitted,
}

impl<B> AccessLogBody<B> {
    pub fn new(body: B, output: Arc<AccessLogOutput>, seed: AccessRecordSeed) -> Self {
        Self {
            body,
            output,
            seed: Some(seed),
            bytes: BodyBytesEmitted::ZERO,
        }
    }

    fn finalize(&mut self) {
        let Some(seed) = self.seed.take() else { return };
        self.output.write(&seed.finish(self.bytes));
    }
}

impl<B> Body for AccessLogBody<B>
where
    B: Body + Unpin,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let frame = match Pin::new(&mut self.body).poll_frame(context) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(frame) => frame,
        };

        match &frame {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    match self.bytes.checked_add(data.remaining()) {
                        Some(bytes) => self.bytes = bytes,
                        None => {
                            tracing::warn!("access log response body byte count overflowed");
                            self.seed = None;
                        }
                    }
                }
            }
            Some(Err(_)) | None => self.finalize(),
        }
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

impl<B> Drop for AccessLogBody<B> {
    fn drop(&mut self) {
        self.finalize();
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use bytes::Bytes;
    use futures::{StreamExt, stream};
    use http_body_util::{BodyExt, StreamBody};

    use super::*;
    use crate::parse::domain::ResolvedConfigPath;

    struct TempLog(PathBuf);

    impl TempLog {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "gateway-access-body-{}-{}.log",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )))
        }

        fn output(&self) -> Arc<AccessLogOutput> {
            Arc::new(
                AccessLogOutput::open(ResolvedConfigPath::try_from(self.0.clone()).unwrap())
                    .unwrap(),
            )
        }

        fn wait_for_line(&self) -> String {
            for _ in 0..100 {
                if let Ok(contents) = std::fs::read_to_string(&self.0)
                    && contents.ends_with('\n')
                {
                    return contents;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("access record was not delivered to {}", self.0.display());
        }
    }

    impl Drop for TempLog {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn seed() -> AccessRecordSeed {
        AccessRecordSeed {
            client: ClientAddress::Unknown,
            method: http::Method::GET,
            target: "/body".parse().unwrap(),
            version: http::Version::HTTP_3,
            referer: OptionalReferer::default(),
            user_agent: OptionalUserAgent::default(),
            status: http::StatusCode::OK,
        }
    }

    #[tokio::test]
    async fn body_error_and_drop_record_actual_data_once() {
        let log = TempLog::new();
        let output = log.output();
        let frames = stream::iter([
            Ok(Frame::data(Bytes::from_static(b"abc"))),
            Err(std::io::Error::other("boom")),
        ]);
        let mut body = std::pin::pin!(AccessLogBody::new(StreamBody::new(frames), output, seed(),));

        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            Bytes::from_static(b"abc")
        );
        assert!(body.frame().await.unwrap().is_err());
        let contents = log.wait_for_line();
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains(" 200 3 "), "{contents}");
    }

    #[tokio::test]
    async fn normal_end_counts_only_data_frames_once() {
        let log = TempLog::new();
        let output = log.output();
        let frames = stream::iter([
            Ok::<_, std::io::Error>(Frame::data(Bytes::from_static(b"ab"))),
            Ok(Frame::trailers(http::HeaderMap::new())),
            Ok(Frame::data(Bytes::from_static(b"c"))),
        ]);
        let body = AccessLogBody::new(StreamBody::new(frames), output, seed());

        let collected = body.collect().await.unwrap();
        assert_eq!(collected.to_bytes(), Bytes::from_static(b"abc"));

        let contents = log.wait_for_line();
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains(" 200 3 "), "{contents}");
    }

    #[tokio::test]
    async fn dropping_a_pending_body_records_only_emitted_data() {
        let log = TempLog::new();
        let output = log.output();
        let frames = stream::iter([Ok::<_, std::io::Error>(Frame::data(Bytes::from_static(
            b"ab",
        )))])
        .chain(stream::pending());
        let mut body = Box::pin(AccessLogBody::new(StreamBody::new(frames), output, seed()));

        assert!(body.frame().await.unwrap().is_ok());
        drop(body);

        let contents = log.wait_for_line();
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains(" 200 2 "), "{contents}");
    }
}

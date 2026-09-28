//! Native dhttp client for QUIC and the TCP stream backend.
use std::{error::Error, io, time::Duration};

use bytes::Bytes;
use http_body::Frame;
use http_body_util::{BodyExt, Full, StreamBody};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

type Failure = Box<dyn Error + Send + Sync>;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("pishoo client: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Failure> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "echo".to_owned());
    let identity =
        std::env::var("PISHOO_CLIENT_IDENTITY").unwrap_or_else(|_| "demo.dhttp.net".to_owned());
    dhttp::DhttpNetwork::init().await?;
    let endpoint = dhttp::Endpoint::load(&identity).await?;
    match command.as_str() {
        "smoke" => {
            for (path, expected) in [
                ("/file/hello.txt", "static from demo\n"),
                ("/proxy/hello.txt", "static from upstream\n"),
                ("/exact", "static from upstream\n"),
                ("/api/info/info", "info Lib handled /info\n"),
            ] {
                let uri: http::Uri = format!("https://demo.dhttp.net{path}").parse()?;
                let response = endpoint.get(uri).await?;
                if response.version() != http::Version::HTTP_3
                    || response.status() != http::StatusCode::OK
                    || response.into_body().collect().await?.to_bytes() != expected
                {
                    return Err(io::Error::other(format!("GET {path} failed")).into());
                }
                println!("h3x/TCP GET {path}: 200");
            }
            let uri: http::Uri = "https://demo.dhttp.net/unlisted".parse()?;
            if endpoint.get(uri).await?.status() != http::StatusCode::FORBIDDEN {
                return Err(io::Error::other("unlisted demo API was not denied").into());
            }
            println!("h3x/TCP GET /unlisted: 403");
            let uri: http::Uri = "https://demo.dhttp.net/unlisted".parse()?;
            if endpoint
                .post(uri)
                .body(Full::new(Bytes::from_static(b"denied upload")))
                .await?
                .status()
                != http::StatusCode::FORBIDDEN
            {
                return Err(io::Error::other("unlisted upload was not denied").into());
            }
            println!("h3x/TCP POST /unlisted: 403");
            let concurrent = (0..8).map(|index| {
                let endpoint = endpoint.clone();
                async move {
                    let (path, expected) = if index % 2 == 0 {
                        ("/file/hello.txt", "static from demo\n")
                    } else {
                        ("/proxy/hello.txt", "static from upstream\n")
                    };
                    let uri: http::Uri = format!("https://demo.dhttp.net{path}").parse()?;
                    let response = endpoint.get(uri).await?;
                    let version = response.version();
                    let status = response.status();
                    let body = response.into_body().collect().await?.to_bytes();
                    Ok::<_, Failure>((version, status, body, expected))
                }
            });
            for result in futures::future::join_all(concurrent).await {
                let (version, status, body, expected) = result?;
                if version != http::Version::HTTP_3
                    || status != http::StatusCode::OK
                    || body != expected
                {
                    return Err(io::Error::other("concurrent H3 streams failed").into());
                }
            }
            println!("h3x/TCP eight concurrent static/proxy streams: 200");
            let uri: http::Uri = "https://demo.dhttp.net/api/echo/echo".parse()?;
            let response = endpoint
                .post(uri.clone())
                .body(Full::new(Bytes::from_static(b"hello h3x")))
                .await?;
            if response.version() != http::Version::HTTP_3
                || response.status() != http::StatusCode::OK
                || response.into_body().collect().await?.to_bytes() != "hello h3x"
            {
                return Err(io::Error::other("POST Echo failed").into());
            }
            println!("h3x/TCP POST /api/echo/echo: 200");

            let large = Bytes::from(vec![b'x'; 256 * 1024]);
            let uri: http::Uri = "https://demo.dhttp.net/api/echo/echo".parse()?;
            let response = endpoint
                .post(uri.clone())
                .body(Full::new(large.clone()))
                .await?;
            let echoed =
                tokio::time::timeout(Duration::from_secs(10), response.into_body().collect())
                    .await??
                    .to_bytes();
            if echoed != large {
                return Err(io::Error::other("large duplex Echo mismatch").into());
            }
            println!("h3x/TCP 256 KiB Echo with stream backpressure: 200");

            let mut request_trailers = http::HeaderMap::new();
            request_trailers.insert(
                "x-request-trailer",
                http::HeaderValue::from_static("preserved"),
            );
            let body = StreamBody::new(futures::stream::iter([
                Ok::<_, dhttp::BoxError>(Frame::data(Bytes::from_static(
                    b"hello-hello-hello-hello-hello-hello-hello-hello",
                ))),
                Ok(Frame::trailers(request_trailers)),
            ]))
            .boxed_unsync();
            let trailers_uri: http::Uri = "https://demo.dhttp.net/api/trailers/read".parse()?;
            let response = endpoint.post(trailers_uri).body(body).await?;
            if response.version() != http::Version::HTTP_3
                || response.status() != http::StatusCode::CREATED
            {
                return Err(io::Error::other("WASM trailer response status failed").into());
            }
            let collected = response.into_body().collect().await?;
            let trailers = collected
                .trailers()
                .ok_or_else(|| io::Error::other("WASM response trailers missing"))?;
            if trailers.get_all("x-response-trailer").iter().count() != 2
                || collected.to_bytes() != b"world-world-".repeat(64)
            {
                return Err(io::Error::other("WASM response body/trailers mismatch").into());
            }
            println!("h3x/TCP WASM upload and duplicate response trailers: 201");

            let (tx, rx) = tokio::sync::mpsc::channel(2);
            let frames = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|frame| (frame, rx))
            });
            let body = StreamBody::new(Box::pin(frames)).boxed_unsync();
            let mut response = endpoint.post(uri).body(body).await?;
            if response.version() != http::Version::HTTP_3
                || response.status() != http::StatusCode::OK
            {
                return Err(io::Error::other("streaming Echo status failed").into());
            }
            for chunk in [b"first\n".as_slice(), b"second\n".as_slice()] {
                tx.send(Ok::<_, dhttp::BoxError>(Frame::data(
                    Bytes::copy_from_slice(chunk),
                )))
                .await
                .map_err(|_| io::Error::other("upload stopped before Echo"))?;
                let mut echoed = Vec::new();
                while echoed.len() < chunk.len() {
                    let frame =
                        tokio::time::timeout(Duration::from_secs(5), response.body_mut().frame())
                            .await?
                            .ok_or_else(|| io::Error::other("Echo response ended early"))??;
                    echoed.extend_from_slice(
                        &frame
                            .into_data()
                            .map_err(|_| io::Error::other("Echo returned non-data frame"))?,
                    );
                }
                if echoed != chunk {
                    return Err(io::Error::other("Echo chunk mismatch").into());
                }
            }
            drop(tx);
            response.into_body().collect().await?;
            println!("h3x/TCP duplex Echo: each chunk arrived before upload EOF");

            let (tx, rx) = tokio::sync::mpsc::channel(2);
            let frames = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|frame| (frame, rx))
            });
            let body = StreamBody::new(Box::pin(frames)).boxed_unsync();
            let duplex_uri: http::Uri = "https://demo.dhttp.net/duplex".parse()?;
            let mut response =
                tokio::time::timeout(Duration::from_secs(5), endpoint.post(duplex_uri).body(body))
                    .await??;
            if response.version() != http::Version::HTTP_3
                || response.status() != http::StatusCode::OK
            {
                return Err(io::Error::other("local proxy duplex status failed").into());
            }
            for chunk in [b"proxy-first\n".as_slice(), b"proxy-second\n".as_slice()] {
                tx.send(Ok::<_, dhttp::BoxError>(Frame::data(
                    Bytes::copy_from_slice(chunk),
                )))
                .await
                .map_err(|_| io::Error::other("local proxy upload stopped early"))?;
                let mut echoed = Vec::new();
                while echoed.len() < chunk.len() {
                    let frame =
                        tokio::time::timeout(Duration::from_secs(5), response.body_mut().frame())
                            .await?
                            .ok_or_else(|| {
                                io::Error::other("local proxy response ended early")
                            })??;
                    echoed.extend_from_slice(
                        &frame.into_data().map_err(|_| {
                            io::Error::other("local proxy returned a non-data frame")
                        })?,
                    );
                }
                if echoed != chunk {
                    return Err(io::Error::other("local proxy duplex chunk mismatch").into());
                }
            }
            drop(tx);
            tokio::time::timeout(Duration::from_secs(5), response.into_body().collect()).await??;
            println!("h3x/TCP local proxy duplex: each chunk arrived before upload EOF");

            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let frames = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|frame| (frame, rx))
            });
            let body = StreamBody::new(Box::pin(frames)).boxed_unsync();
            let uri: http::Uri = "https://demo.dhttp.net/api/echo/echo".parse()?;
            let mut response = endpoint.post(uri).body(body).await?;
            tx.send(Ok::<_, dhttp::BoxError>(Frame::data(Bytes::from_static(
                b"abandoned\n",
            ))))
            .await
            .map_err(|_| io::Error::other("cancel test upload stopped"))?;
            let _ = tokio::time::timeout(Duration::from_secs(5), response.body_mut().frame())
                .await?
                .ok_or_else(|| io::Error::other("cancel test response ended early"))??;
            drop(response);
            drop(tx);
            let uri: http::Uri = "https://demo.dhttp.net/file/hello.txt".parse()?;
            let follow_up =
                tokio::time::timeout(Duration::from_secs(5), endpoint.get(uri)).await??;
            if follow_up.status() != http::StatusCode::OK
                || follow_up.into_body().collect().await?.to_bytes() != "static from demo\n"
            {
                return Err(io::Error::other("request after dropped Echo failed").into());
            }
            println!("h3x/TCP dropped Echo response: next request still succeeds");
        }
        "get" | "post" => {
            let target = args
                .next()
                .ok_or("usage: pishoo-client get|post URL [BODY]")?;
            let uri: http::Uri = if target.starts_with('/') {
                format!("https://{identity}{target}").parse()?
            } else {
                target.parse()?
            };
            let response = if command == "get" {
                endpoint.get(uri).await?
            } else {
                let body = args.next().unwrap_or_default();
                endpoint
                    .post(uri)
                    .body(Full::new(Bytes::from(body)))
                    .await?
            };
            eprintln!("{:?} {}", response.version(), response.status());
            let body = response.into_body().collect().await?.to_bytes();
            tokio::io::AsyncWriteExt::write_all(&mut tokio::io::stdout(), &body).await?;
        }
        "echo" => {
            let target = args
                .next()
                .unwrap_or_else(|| format!("https://{identity}/api/echo/echo"));
            let uri: http::Uri = if target.starts_with('/') {
                format!("https://{identity}{target}").parse()?
            } else {
                target.parse()?
            };
            let (tx, rx) = tokio::sync::mpsc::channel(2);
            let frames = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|frame| (frame, rx))
            });
            let body = StreamBody::new(Box::pin(frames)).boxed_unsync();
            let mut response = endpoint.post(uri).body(body).await?;
            if response.status() != http::StatusCode::OK {
                return Err(io::Error::other(format!("Echo: {}", response.status())).into());
            }
            eprintln!("Type a line and press Enter. Ctrl-D ends the stream; Ctrl-C exits.");
            let input = tokio::spawn(async move {
                let mut lines = BufReader::new(tokio::io::stdin()).lines();
                while let Some(line) = lines.next_line().await? {
                    tx.send(Ok::<_, dhttp::BoxError>(Frame::data(Bytes::from(format!(
                        "{line}\n"
                    )))))
                    .await
                    .map_err(|_| io::Error::other("Echo upload ended"))?;
                }
                Ok::<_, io::Error>(())
            });
            let mut stdout = tokio::io::stdout();
            let mut line_start = true;
            while let Some(frame) = response.body_mut().frame().await {
                if let Ok(data) = frame?.into_data() {
                    for chunk in data.split_inclusive(|byte| *byte == b'\n') {
                        if line_start {
                            stdout.write_all(b"echo> ").await?;
                        }
                        stdout.write_all(chunk).await?;
                        line_start = chunk.ends_with(b"\n");
                    }
                    stdout.flush().await?;
                }
            }
            if input.is_finished() {
                input.await??;
            } else {
                input.abort();
                let _ = input.await;
            }
        }
        _ => return Err("usage: pishoo-client [echo [URL]|get URL|post URL BODY|smoke]".into()),
    }
    Ok(())
}

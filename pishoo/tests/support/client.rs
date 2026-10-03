//! Native dhttp client for QUIC.
use std::{error::Error, io, time::Duration};

use bytes::Bytes;
use http_body_util::BodyExt;
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
                println!("HTTP/3 GET {path}: 200");
            }
            let uri: http::Uri = "https://demo.dhttp.net/unlisted".parse()?;
            if endpoint.get(uri).await?.status() != http::StatusCode::FORBIDDEN {
                return Err(io::Error::other("unlisted demo API was not denied").into());
            }
            println!("HTTP/3 GET /unlisted: 403");
            let uri: http::Uri = "https://demo.dhttp.net/unlisted".parse()?;
            if post_bytes(&endpoint, uri, Bytes::from_static(b"denied upload"))
                .await?
                .status()
                != http::StatusCode::FORBIDDEN
            {
                return Err(io::Error::other("unlisted upload was not denied").into());
            }
            println!("HTTP/3 POST /unlisted: 403");
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
            println!("HTTP/3 eight concurrent static/proxy streams: 200");
            let uri: http::Uri = "https://demo.dhttp.net/api/echo/echo".parse()?;
            let response =
                post_bytes(&endpoint, uri.clone(), Bytes::from_static(b"hello h3x")).await?;
            if response.version() != http::Version::HTTP_3
                || response.status() != http::StatusCode::OK
                || response.into_body().collect().await?.to_bytes() != "hello h3x"
            {
                return Err(io::Error::other("POST Echo failed").into());
            }
            println!("HTTP/3 POST /api/echo/echo: 200");

            let large = Bytes::from(vec![b'x'; 256 * 1024]);
            let uri: http::Uri = "https://demo.dhttp.net/api/echo/echo".parse()?;
            let response = post_bytes(&endpoint, uri.clone(), large.clone()).await?;
            let echoed =
                tokio::time::timeout(Duration::from_secs(10), response.into_body().collect())
                    .await??
                    .to_bytes();
            if echoed != large {
                return Err(io::Error::other("large duplex Echo mismatch").into());
            }
            println!("HTTP/3 256 KiB Echo with stream backpressure: 200");

            let trailers_uri: http::Uri = "https://demo.dhttp.net/api/trailers/read".parse()?;
            let response = post_bytes(
                &endpoint,
                trailers_uri,
                Bytes::from_static(b"hello-hello-hello-hello-hello-hello-hello-hello"),
            )
            .await?;
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
            println!("HTTP/3 WASM upload and duplicate response trailers: 201");

            let (mut upload, response) = endpoint.post(uri).await?;
            let mut response = response.await?;
            if response.version() != http::Version::HTTP_3
                || response.status() != http::StatusCode::OK
            {
                return Err(io::Error::other("streaming Echo status failed").into());
            }
            for chunk in [b"first\n".as_slice(), b"second\n".as_slice()] {
                upload.write_all(chunk).await?;
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
            upload.shutdown().await?;
            response.into_body().collect().await?;
            println!("HTTP/3 duplex Echo: each chunk arrived before upload EOF");

            let duplex_uri: http::Uri = "https://demo.dhttp.net/duplex".parse()?;
            let (mut upload, response) = endpoint.post(duplex_uri).await?;
            let mut response = tokio::time::timeout(Duration::from_secs(5), response).await??;
            if response.version() != http::Version::HTTP_3
                || response.status() != http::StatusCode::OK
            {
                return Err(io::Error::other("local proxy duplex status failed").into());
            }
            for chunk in [b"proxy-first\n".as_slice(), b"proxy-second\n".as_slice()] {
                upload.write_all(chunk).await?;
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
            upload.shutdown().await?;
            tokio::time::timeout(Duration::from_secs(5), response.into_body().collect()).await??;
            println!("HTTP/3 local proxy duplex: each chunk arrived before upload EOF");

            let uri: http::Uri = "https://demo.dhttp.net/api/echo/echo".parse()?;
            let (mut upload, response) = endpoint.post(uri).await?;
            let mut response = response.await?;
            upload.write_all(b"abandoned\n").await?;
            let _ = tokio::time::timeout(Duration::from_secs(5), response.body_mut().frame())
                .await?
                .ok_or_else(|| io::Error::other("cancel test response ended early"))??;
            drop(response);
            drop(upload);
            let uri: http::Uri = "https://demo.dhttp.net/file/hello.txt".parse()?;
            let follow_up =
                tokio::time::timeout(Duration::from_secs(5), endpoint.get(uri)).await??;
            if follow_up.status() != http::StatusCode::OK
                || follow_up.into_body().collect().await?.to_bytes() != "static from demo\n"
            {
                return Err(io::Error::other("request after dropped Echo failed").into());
            }
            println!("HTTP/3 dropped Echo response: next request still succeeds");
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
                post_bytes(&endpoint, uri, Bytes::from(body)).await?
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
            let (mut upload, response) = endpoint.post(uri).await?;
            let mut input = scopeguard::guard(
                tokio::spawn(async move {
                    let mut lines = BufReader::new(tokio::io::stdin()).lines();
                    while let Some(line) = lines.next_line().await? {
                        upload.write_all(format!("{line}\n").as_bytes()).await?;
                    }
                    upload.shutdown().await
                }),
                |task| task.abort(),
            );
            let mut response = response.await?;
            if response.status() != http::StatusCode::OK {
                return Err(io::Error::other(format!("Echo: {}", response.status())).into());
            }
            eprintln!("Type a line and press Enter. Ctrl-D ends the stream; Ctrl-C exits.");
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
                (&mut *input).await??;
            }
        }
        _ => return Err("usage: pishoo-client [echo [URL]|get URL|post URL BODY|smoke]".into()),
    }
    Ok(())
}

// Finish finite byte uploads independently so an early streaming response can
// be consumed while h3x is still sending the request.
async fn post_bytes(
    endpoint: &dhttp::Endpoint,
    uri: http::Uri,
    bytes: Bytes,
) -> Result<http::Response<dhttp::Body>, Failure> {
    let (mut writer, response) = endpoint
        .post(uri)
        .body(dhttp::WndBuf::with_initial(64 * 1024, bytes))
        .await?;
    let upload = scopeguard::guard(
        tokio::spawn(async move { writer.shutdown().await }),
        |task| task.abort(),
    );
    let response = response.await?;
    drop(scopeguard::ScopeGuard::into_inner(upload));
    Ok(response)
}

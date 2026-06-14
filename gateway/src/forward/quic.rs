use std::sync::Arc;

use dhttp::{
    endpoint::Endpoint,
    h3x::{connection::Connection, quic},
};
use http::{Request, Response, uri::Authority};
use http_body_util::BodyExt;
use hyper::{server::conn::http1, service::service_fn};
use snafu::{Report, ResultExt, Whatever};
use tracing::{Instrument, error, info};

use super::{BoxResponse, task_scope::ForwardTaskSpawner};
use crate::{
    error::BoxError,
    forward::{build_empty_response, build_error_response, tunnel_upgrade, validate_host},
};

/// 处理普通 HTTP 请求
#[tracing::instrument(level = "info", skip_all, fields(odcid = tracing::field::Empty))]
pub async fn proxy(
    mut req: Request<hyper::body::Incoming>,
    client: Arc<Endpoint>,
    task_spawner: ForwardTaskSpawner,
) -> Result<BoxResponse, hyper::Error> {
    // 验证主机合法性
    let host = match validate_host(&mut req) {
        Ok(host) => host,
        Err(error) => {
            error!(error = %Report::from_error(&error), "invalid host");
            return Ok(build_error_response(Report::from_error(&error).to_string()));
        }
    };
    // 创建 QUIC 连接
    let h3_conn = match connect(&client, &host).await {
        Ok(conn) => conn,
        Err(error) => {
            error!(error = %Report::from_error(&error), "failed to create quic connection");
            return Ok(build_error_response(Report::from_error(&error).to_string()));
        }
    };

    let request_upgrade = hyper::upgrade::on(&mut req);

    // 代理请求并返回响应
    match send(h3_conn, req).await {
        Ok(mut response) => {
            let response_upgrade = hyper::upgrade::on(&mut response);
            // Terminates when either end of the tunnel closes the connection.
            task_spawner.spawn(tunnel_upgrade(request_upgrade, response_upgrade).in_current_span());
            info!(?response, "request proxied successfully");
            Ok(response)
        }
        Err(error) => {
            error!(error = %Report::from_error(&error), "forward request failed");
            Ok(build_error_response(Report::from_error(&error).to_string()))
        }
    }
}

/// 处理 CONNECT 隧道请求
pub async fn connect_tunnel(
    req: Request<hyper::body::Incoming>,
    client: Arc<Endpoint>,
    task_spawner: ForwardTaskSpawner,
) -> Result<BoxResponse, hyper::Error> {
    let tunnel_spawner = task_spawner.clone();
    task_spawner.spawn(
        async move {
            // 升级连接并处理后续请求
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => {
                    info!("establishing tunnel to request uri");
                    let service_spawner = tunnel_spawner.clone();
                    let service = service_fn(move |req| {
                        let client = client.clone();
                        let request_spawner = service_spawner.clone();
                        async move { proxy(req, client, request_spawner).await }
                    });
                    if let Err(error) = http1::Builder::new()
                        .preserve_header_case(true)
                        .title_case_headers(true)
                        .serve_connection(upgraded, service)
                        .await
                    {
                        error!(error = %Report::from_error(&error), "connection handling failed");
                    }
                }
                Err(error) => {
                    error!(error = %Report::from_error(&error), "connection upgrade failed")
                }
            }
        }
        .in_current_span(),
    );

    Ok(build_empty_response())
}

/// 将请求通过 quic 转发到目标服务器
async fn send<Conn: quic::Connection>(
    h3_conn: Arc<Connection<Conn>>,
    req: Request<hyper::body::Incoming>,
) -> Result<BoxResponse, Whatever> {
    // 使用 h3x 的 execute_hyper_request 一步完成：打开流、发送请求、接收响应
    let response = h3_conn
        .execute_hyper_request(req)
        .await
        .whatever_context::<_, Whatever>("failed to execute quic request")?;

    // 将响应体转换为 BoxBody
    let (mut parts, body) = response.into_parts();
    parts.version = http::Version::HTTP_11;
    let body = BodyExt::boxed_unsync(body.map_err(BoxError::from));
    Ok(Response::from_parts(parts, body))
}

/// 通过 h3x 连接池获取连接
async fn connect(
    client: &Endpoint,
    host: &str,
) -> Result<Arc<Connection<<Endpoint as quic::Connect>::Connection>>, Whatever> {
    let authority: Authority = host
        .parse()
        .whatever_context(format!("invalid host: {host}"))?;
    let conn = client
        .connect(authority)
        .await
        .whatever_context(format!("connect to {host} failed"))?;
    Ok(conn)
}

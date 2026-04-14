use std::{
    env,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use super::*;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn create_temp_file(prefix: &str) -> PathBuf {
    let path = env::temp_dir().join(format!(
        "gateway_{prefix}_{}_{}.pem",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, "dummy").expect("Failed to create temp file");
    path
}

fn cleanup_temp_files(paths: &[&Path]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

fn build_proxy_conf(server_cert: &Path, server_key: &Path, location_body: &str) -> String {
    format!(
        r#"
pishoo {{
    server {{
        listen all 5378;
        server_name example.com;
        ssl_certificate {};
        ssl_certificate_key {};
        location /api {{
            {}
        }}
    }}
}}
"#,
        server_cert.display(),
        server_key.display(),
        location_body
    )
}

/// 测试：解析新的 server_id 配置格式
#[test]
fn test_parse_server_with_server_id() {
    let conf = r#"
pishoo {
    server {
        listen all 5378;
        server_name example.com;
        server_id 1;
        ssl_certificate /tmp/test_cert.pem;
        ssl_certificate_key /tmp/test_key.pem;
    }
}
"#;
    // 创建临时证书文件用于测试
    std::fs::write("/tmp/test_cert.pem", "dummy cert").expect("Failed to create test cert");
    std::fs::write("/tmp/test_key.pem", "dummy key").expect("Failed to create test key");

    let result = parse(conf.as_bytes(), None);

    // 清理临时文件
    let _ = std::fs::remove_file("/tmp/test_cert.pem");
    let _ = std::fs::remove_file("/tmp/test_key.pem");

    let root = result.expect("配置解析失败");
    let pishoo = root
        .get("pishoo")
        .and_then(|v| {
            if let Value::Nodes(n) = v {
                n.first().cloned()
            } else {
                None
            }
        })
        .expect("未找到 pishoo 块");

    let servers = match pishoo.get("server") {
        Some(Value::Nodes(nodes)) => nodes,
        _ => panic!("未找到 server 配置块"),
    };
    assert_eq!(servers.len(), 1);

    let server = &servers[0];

    // 验证 server_name
    match server.get("server_name") {
        Some(Value::ServerName(names)) => {
            assert_eq!(names.len(), 1);
            assert_eq!(names[0].name, "example.com");
        }
        _ => panic!("server_name 解析失败"),
    }

    // 验证 server_id
    match server.get("server_id") {
        Some(Value::ServerId(id)) => assert_eq!(*id, 1),
        _ => panic!("server_id 解析失败"),
    }
}

/// 测试：解析多个 server 配置，每个有不同的 server_id
#[test]
fn test_parse_multiple_servers_with_different_ids() {
    let conf = r#"
pishoo {
    server {
        listen all 5378;
        server_name main.example.com;
        server_id 0;
        ssl_certificate /tmp/test_cert1.pem;
        ssl_certificate_key /tmp/test_key1.pem;
    }
    server {
        listen all 5379;
        server_name backup.example.com;
        server_id 1;
        ssl_certificate /tmp/test_cert2.pem;
        ssl_certificate_key /tmp/test_key2.pem;
    }
}
"#;
    // 创建临时证书文件用于测试
    std::fs::write("/tmp/test_cert1.pem", "dummy cert 1").expect("Failed to create test cert 1");
    std::fs::write("/tmp/test_key1.pem", "dummy key 1").expect("Failed to create test key 1");
    std::fs::write("/tmp/test_cert2.pem", "dummy cert 2").expect("Failed to create test cert 2");
    std::fs::write("/tmp/test_key2.pem", "dummy key 2").expect("Failed to create test key 2");

    let result = parse(conf.as_bytes(), None);

    // 清理临时文件
    let _ = std::fs::remove_file("/tmp/test_cert1.pem");
    let _ = std::fs::remove_file("/tmp/test_key1.pem");
    let _ = std::fs::remove_file("/tmp/test_cert2.pem");
    let _ = std::fs::remove_file("/tmp/test_key2.pem");

    let root = result.expect("配置解析失败");
    let pishoo = root
        .get("pishoo")
        .and_then(|v| {
            if let Value::Nodes(n) = v {
                n.first().cloned()
            } else {
                None
            }
        })
        .expect("未找到 pishoo 块");

    let servers = match pishoo.get("server") {
        Some(Value::Nodes(nodes)) => nodes,
        _ => panic!("未找到 server 配置块"),
    };
    assert_eq!(servers.len(), 2);

    // 验证第一个 server
    let server1 = &servers[0];
    match server1.get("server_name") {
        Some(Value::ServerName(names)) => assert_eq!(names[0].name, "main.example.com"),
        _ => panic!("第一个 server 的 server_name 解析失败"),
    }
    match server1.get("server_id") {
        Some(Value::ServerId(id)) => assert_eq!(*id, 0),
        _ => panic!("第一个 server 的 server_id 解析失败"),
    }

    // 验证第二个 server
    let server2 = &servers[1];
    match server2.get("server_name") {
        Some(Value::ServerName(names)) => assert_eq!(names[0].name, "backup.example.com"),
        _ => panic!("第二个 server 的 server_name 解析失败"),
    }
    match server2.get("server_id") {
        Some(Value::ServerId(id)) => assert_eq!(*id, 1),
        _ => panic!("第二个 server 的 server_id 解析失败"),
    }
}

/// 测试：server 配置缺少 server_id 时的默认行为
#[test]
fn test_parse_server_without_server_id() {
    use std::env;
    let temp_dir = env::temp_dir();
    let cert_path = temp_dir.join("test_cert_no_id.pem");
    let key_path = temp_dir.join("test_key_no_id.pem");

    let conf = format!(
        r#"
pishoo {{
    server {{
        listen all 5378;
        server_name example.com;
        ssl_certificate {};
        ssl_certificate_key {};
    }}
}}
"#,
        cert_path.display(),
        key_path.display()
    );

    // 创建临时证书文件用于测试
    std::fs::write(&cert_path, "dummy cert").expect("Failed to create test cert");
    std::fs::write(&key_path, "dummy key").expect("Failed to create test key");

    let result = parse(conf.as_bytes(), None);

    // 清理临时文件
    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);

    let root = result.expect("配置解析失败");
    let pishoo = root
        .get("pishoo")
        .and_then(|v| {
            if let Value::Nodes(n) = v {
                n.first().cloned()
            } else {
                None
            }
        })
        .expect("未找到 pishoo 块");

    let servers = match pishoo.get("server") {
        Some(Value::Nodes(nodes)) => nodes,
        _ => panic!("未找到 server 配置块"),
    };
    assert_eq!(servers.len(), 1);

    let server = &servers[0];

    // 验证 server_name
    match server.get("server_name") {
        Some(Value::ServerName(names)) => assert_eq!(names[0].name, "example.com"),
        _ => panic!("server_name 解析失败"),
    }

    // 验证没有 server_id 时应该返回 None
    match server.get("server_id") {
        None => {} // 这是期望的行为
        Some(_) => panic!("不应该有 server_id 字段"),
    }
}

/// 测试：解析 DNS resolver 和 publisher 配置
#[test]
fn test_parse_dns_resolver_and_publisher() {
    use std::env;
    let temp_dir = env::temp_dir();
    let cert_path = temp_dir.join("test_cert_dns.pem");
    let key_path = temp_dir.join("test_key_dns.pem");

    let conf = format!(
        r#"
pishoo {{
    server {{
        listen all 5378;
        server_name example.com;
        dns h3 https://dns.example.com/dns-query;
        ssl_certificate {};
        ssl_certificate_key {};
    }}
}}
"#,
        cert_path.display(),
        key_path.display()
    );

    // 创建临时证书文件用于测试
    std::fs::write(&cert_path, "dummy cert").expect("Failed to create test cert");
    std::fs::write(&key_path, "dummy key").expect("Failed to create test key");

    let result = parse(conf.as_bytes(), None);

    // 清理临时文件
    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);

    let root = result.expect("配置解析失败");
    let pishoo = root
        .get("pishoo")
        .and_then(|v| {
            if let Value::Nodes(n) = v {
                n.first().cloned()
            } else {
                None
            }
        })
        .expect("未找到 pishoo 块");

    let servers = match pishoo.get("server") {
        Some(Value::Nodes(nodes)) => nodes,
        _ => panic!("未找到 server 配置块"),
    };
    assert_eq!(servers.len(), 1);

    let server = &servers[0];

    // 验证 dns 配置
    match server.get("dns") {
        Some(Value::Resolver(resolver)) => {
            assert_eq!(resolver.to_string(), "https://dns.example.com/dns-query");
        }
        _ => panic!("dns 解析失败"),
    }
}

#[test]
fn test_parse_proxy_pass_rejects_unsupported_scheme() {
    let server_cert = create_temp_file("server_cert_scheme");
    let server_key = create_temp_file("server_key_scheme");
    let conf = build_proxy_conf(
        &server_cert,
        &server_key,
        "proxy_pass ftp://backend.example.com;",
    );

    let result = parse(conf.as_bytes(), None);

    cleanup_temp_files(&[&server_cert, &server_key]);

    assert!(result.is_err(), "unsupported proxy_pass scheme should fail");
}

#[test]
fn test_parse_https_proxy_with_optional_trusted_certificate() {
    let server_cert = create_temp_file("server_cert_https");
    let server_key = create_temp_file("server_key_https");
    let trusted_ca = create_temp_file("trusted_ca_https");
    let conf = build_proxy_conf(
        &server_cert,
        &server_key,
        &format!(
            "proxy_pass https://backend.example.com; proxy_ssl_trusted_certificate {};",
            trusted_ca.display()
        ),
    );

    let result = parse(conf.as_bytes(), None);

    cleanup_temp_files(&[&server_cert, &server_key, &trusted_ca]);

    assert!(result.is_ok(), "https proxy with trusted CA should parse");
}

#[test]
fn test_parse_proxy_ssl_certificate_requires_matching_key() {
    let server_cert = create_temp_file("server_cert_pair");
    let server_key = create_temp_file("server_key_pair");
    let proxy_client_cert = create_temp_file("proxy_client_cert_pair");
    let conf = build_proxy_conf(
        &server_cert,
        &server_key,
        &format!(
            "proxy_pass https://backend.example.com; proxy_ssl_certificate {};",
            proxy_client_cert.display()
        ),
    );

    let result = parse(conf.as_bytes(), None);

    cleanup_temp_files(&[&server_cert, &server_key, &proxy_client_cert]);

    assert!(
        result.is_err(),
        "proxy_ssl_certificate without key should fail"
    );
}

#[test]
fn test_parse_http_proxy_allows_proxy_ssl_directives() {
    let server_cert = create_temp_file("server_cert_http");
    let server_key = create_temp_file("server_key_http");
    let proxy_client_cert = create_temp_file("proxy_client_cert_http");
    let proxy_client_key = create_temp_file("proxy_client_key_http");
    let trusted_ca = create_temp_file("trusted_ca_http");
    let conf = build_proxy_conf(
        &server_cert,
        &server_key,
        &format!(
            "proxy_pass http://backend.example.com; proxy_ssl_certificate {}; proxy_ssl_certificate_key {}; proxy_ssl_trusted_certificate {};",
            proxy_client_cert.display(),
            proxy_client_key.display(),
            trusted_ca.display()
        ),
    );

    let result = parse(conf.as_bytes(), None);

    cleanup_temp_files(&[
        &server_cert,
        &server_key,
        &proxy_client_cert,
        &proxy_client_key,
        &trusted_ca,
    ]);

    assert!(
        result.is_ok(),
        "http proxy should allow proxy_ssl directives to remain optional"
    );
}

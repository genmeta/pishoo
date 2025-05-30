use http::{HeaderValue, header, request::Parts};

pub(crate) fn search(parts: &Parts, value: HeaderValue) -> HeaderValue {
    let name_cloned = value.clone();
    let value = match value.to_str() {
        Ok(s) => s,
        Err(_) => return HeaderValue::from_static(""),
    };

    match value {
        "$host" => {
            let host = parts.headers.get(header::HOST);
            if let Some(host_value) = host {
                host_value.clone()
            } else {
                HeaderValue::from_static("")
            }
        }
        "$scheme" => HeaderValue::from_static("https"),
        "$remote_addr" => {
            let remote_addr = parts
                .extensions
                .get::<std::net::SocketAddr>()
                .map(|addr| addr.to_string())
                .unwrap_or_default();
            HeaderValue::from_str(&remote_addr).unwrap_or_else(|_| HeaderValue::from_static(""))
        }
        http if http.starts_with("$http_") => {
            let header_name = &http[6..];
            let header_name = header_name.replace("_", "-");
            let header = parts.headers.get(header_name.as_str());
            if let Some(header_value) = header {
                header_value.clone()
            } else {
                HeaderValue::from_static("")
            }
        }
        arg if arg.starts_with("$arg_") => {
            let arg_name = &arg[5..];
            if let Some(args) = parts.uri.query() {
                let query_pairs = form_urlencoded::parse(args.as_bytes());
                for (key, value) in query_pairs {
                    // 支持 user-id 与 user.id 形式的 key, 与配置 user_id 匹配
                    let key = key.replace("-", "_");
                    let key = key.replace(".", "_");
                    if key == arg_name {
                        return HeaderValue::from_str(&value)
                            .unwrap_or_else(|_| HeaderValue::from_static(""));
                    }
                }
            };
            HeaderValue::from_static("")
        }
        _ => name_cloned,
    }
}

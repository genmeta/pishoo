# gateway

## 启动反向代理

```sh
cargo run -p pishoo -- -c config/pishoo.conf
```

## 启动正向代理

```sh
cargo run -p gateway --example forward config/forward.conf
```

## 测试请求

```sh
curl -x http://127.0.0.1:5379 http://test2.genmeta.net/static/TODO.md
```

## HTTPS upstream 端到端验证

```sh
scripts/e2e_proxy_https_chain.sh
```

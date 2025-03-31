# 测试流程

## 启动反向代理
```sh
cargo run --example pishoo -- -c config/reverse.conf
```

## 启动正向代理
```sh
cargo run --example forward config/forward.conf
```

## 测试请求

```sh
curl -x http://127.0.0.1:5379 http://test2.genmeta.net/static/TODO.md
```
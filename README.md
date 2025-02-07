# 测试流程

## 启动反向代理
```sh
cargo run config/reverse.conf
```

## 启动正向代理
```sh
cargo run config/forward.conf
```

## 测试请求

```sh
curl -x http://192.168.2.142:5379 http://test1.genmeta.net/static/TODO.md
```
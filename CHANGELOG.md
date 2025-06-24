# Changelog
## [0.2.8]

### 修复

- MDNS 导致崩溃

### 更新

- 提升打洞效率

## [0.2.7]

### 更新

- 不汇报`test`和`user`域dns
- 更新依赖（traversal）
- 在ssh3 auth 提示中回显uri（不包括.genemta.net）

## [0.2.6]
### 修复
-   转发请求时, 删除了过多的 Header, 导致部分请求失败的问题

## [0.2.5]

### 更新
-   location 支持 `proxy_set_header` 配置
    -   当前支持变量:
        -   `$host`
        -   `$scheme` 变量
        -   以 `$http_` 开头的变量, 例如 `$http_user_agent`, 将匹配原始请求 `User-Agent` 头部
        -   以 `$arg_` 开头的变量, 例如 `$arg_user_id`, 将匹配query字符串中的请求参数 `user_id`/`user-id`/`user.id` 值
        -   `$remote_addr` 变量, 匹配客户端 IP 地址
    -   目前仅支持单独设置变量, 或者直接设置为常量值, 不支持变量拼接
-   proxy_pass 地址支持末尾斜杠
-   代理响应默认添加 CORS 相关头部
-   代理请求时, 默认将 `Host` 头部设置为目标地址, `Connection` 头部设置为 `close`, 去除其他 Header
-   支持 https dns

## [0.2.4]

### 修复

-   多 server 块支持绑定同一端口
-   支持 ~ 后缀
    -   使用 `http://test~` 可以访问 `https://test.genmeta.net`
-   mdns 支持
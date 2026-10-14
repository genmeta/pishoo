# Calendar WASI Lib

版本 `0.1.0`。Rust WASI HTTP Component，在自己的 `/db/calendar.db` 保存日程，每次请求直接持有 SQLite 连接。没有原生宿主路由、共享状态容器、其他 Lib 依赖或出站请求。

```sh
cargo test --locked --offline --manifest-path std/libs/calendar/Cargo.toml
WASI_SDK_PATH=/path/to/wasi-sdk sh std/libs/calendar/build.sh --publisher YOUR_NAME.dhttp.net
```

OpenAPI 3.1 见 [openapi.json](openapi.json)，宿主在内部路径前加 `/std`：

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/calendar/events?from=<ms>&to=<ms>` | 与范围相交的日程，排他上界 |
| POST | `/calendar/events` | 创建日程 |
| GET | `/calendar/event?id=<id>` | 读取日程 |
| PUT | `/calendar/event?id=<id>` | 全量编辑，JSON 必须包含原 updated_at |
| DELETE | `/calendar/event?id=<id>&updated_at=<ms>` | 按版本删除 |

每条日程包含 id/title/description/location/start_at/end_at/all_day/color/created_at/updated_at。时间使用 Unix 毫秒，范围为 1970–9999 年；结束时间排他且晚于开始时间。全天起止必须为 UTC 零点，表示固定日期而非当地时间瞬间。支持五种颜色。查询跨度最多 370 天，超过 10,000 条时返回 413，要求缩小范围，不静默截断。

读取与写入均要求 caller 与 local 的真实名称及 owner_hash 一致，并经过宿主普通 daccess；跨站修改拒绝。OpenAPI 声明 read/write 两组业务能力，安装不自动授予权限。没有私有授权表、审批接口、提醒后台作业或重复日程。

更新和删除使用 SQLite 即时事务核对 updated_at。更新取 max(当前毫秒, 原值+1)，同一毫秒内仍能区分修改；过期版本返回 409，不覆盖现有记录。新空库事务初始化 schema v1，未知/损坏库拒绝，不隐式迁移。错误使用 `application/problem+json` 与固定英文 type/title/status/detail，遵循 [RFC 9457](https://www.rfc-editor.org/rfc/rfc9457.html)，不暴露数据库路径或 SQL。响应使用 `private, no-store`。

运行测试使用隔离 SQLite，覆盖输入校验、范围相交、排他边界、全天日期、数据库版本和乐观并发；生产组件由 `wasm-tools validate` 验证。构建输出 `dist/lib.wasm` 和包构建输入；签名与前端打包见 [Calendar App](../../apps/calendar/README.md)。

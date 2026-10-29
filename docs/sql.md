# SQL 文件组织

生产 SQL 跟随所属模块：`src/package/sql/`、`src/contact/sql/`、`src/setup/sql/`。测试专用历史定义留在 `tests/fixtures/`；固定依赖和运行快照保持原样。

## 命名规则

- 文件名统一为小写 `snake_case`。
- `schema.sql`：新库的基础结构。最终数据库版本由调用方的初始化及迁移流程决定。
- `<purpose>.sql`：按用途命名的查询或视图，如 `registration_facts.sql`、`runtime_views.sql`。
- `migrations/v4_add_signing_evidence.sql`：`v<目标版本>_<具体变更>.sql`；版本不补零，变更名称使用动词和对象。迁移顺序由调用代码显式控制，不依赖文件名排序。
- `compat/v3/schema.sql`：生产仍依赖的历史定义，版本放在目录名中，同类文件保持相同名字。
- `tests/fixtures/<domain>/v1/schema.sql`：仅供测试的历史结构，同样使用 `v<版本>` 目录。

## 所有者与执行入口

| 路径 | 用途 |
| --- | --- |
| `src/setup/sql/schema.sql` | setting v3 的 setting、proxy_location 表；版本由 setup 写入 |
| `src/contact/sql/schema.sql` | 当前联系人投递库 v5 |
| `src/contact/sql/migrations/` | v1 → v5 的逐步迁移 |
| `src/package/sql/schema.sql` | 当前包登记 v4 与 ACL 能力表；要求附加 pkg_registry、acl 数据库 |
| `src/package/sql/compat/v3/schema.sql` | 仍被生产复用的 v3 基础定义与能力表修复；不添加 v4 签名列 |
| `src/package/sql/migrations/v4_add_signing_evidence.sql` | 已有包登记补充 v4 签名证据列 |
| `src/package/sql/registration_facts.sql` | v4 安装登记快照查询 |
| `src/package/sql/compat/v3/registration_facts.sql` | 仍由恢复路径使用的 v3 登记投影 |
| `src/package/sql/runtime_views.sql` | 每个配置连接的临时 Lib/App 视图 |

旧联系人 v1 与包登记 v2 的测试定义分别位于 `tests/fixtures/contact/v1/schema.sql`、`tests/fixtures/package/v2/schema.sql`。版本判断、事务边界、执行顺序和数据库内容不因文件位置或命名调整而变化。

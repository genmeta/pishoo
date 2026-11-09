# SQL 文件组织

生产 SQL 跟随所属模块。运行查询集中在所属模块的 `sql.rs`；建表、迁移和历史定义保留在 `sql/` 目录，如 `src/package/sql/`、`src/contact/sql/`、`src/setup/sql/`、`src/file/sql/`。测试专用历史定义留在 `tests/fixtures/`；固定依赖和运行快照保持原样。

## 集中运行查询

- 同一存储流程的查询、写入和连接配置集中在一个 `sql.rs`，以具名多行字符串常量引用，避免每条短语句一个文件。宿主和业务仓库遵循同一规则，Message 的 `delivery/sql.rs` 与宿主的 `file/sql.rs` 使用相同方式。
- 使用普通 `mod sql;` 和 `sql::CONSTANT`；常量只在所属模块范围可见，不引入自定义 SQL 分段、解析、加载或执行封装。
- `schema.sql`、结构变更脚本和必要的历史定义仍独立保存；完整的视图定义、登记快照等有独立用途的脚本可保留 `.sql` 文件。不能仅因语句不同就逐条拆文件。
- 整理只改变源码组织，保留 SQL 语义、参数位置、执行顺序和事务边界，不增加 schema 版本或兼容分支。

## 命名规则

- 文件名统一为小写 `snake_case`。
- `schema.sql`：新库的基础结构。最终数据库版本由调用方的初始化及迁移流程决定。
- `sql.rs`：同一存储流程的运行 SQL，使用 `UPPER_SNAKE_CASE` 常量名。
- `<purpose>.sql`：有独立用途的完整脚本，如 `registration_facts.sql`、`runtime_views.sql`；不用于逐条存放短查询。
- `migrations/v4_add_signing_evidence.sql`：`v<目标版本>_<具体变更>.sql`；版本不补零，变更名称使用动词和对象。迁移顺序由调用代码显式控制，不依赖文件名排序。
- `compat/v3/schema.sql`：生产仍依赖的历史定义，版本放在目录名中，同类文件保持相同名字。
- `tests/fixtures/<domain>/v1/schema.sql`：仅供测试的历史结构，同样使用 `v<版本>` 目录。

## 所有者与执行入口

| 路径 | 用途 |
| --- | --- |
| `src/file/sql.rs` | 文件索引读取、创建、偏移提交、删除、列检查及连接配置 |
| `src/file/sql/schema.sql` | 原生文件索引的基础结构 |
| `src/file/sql/add_storage_name.sql` | 旧文件索引补充实际落盘名；调用方检查列并控制事务，不增加版本 |
| `src/setup/sql/schema.sql` | setting v3 的 setting、proxy_location 表；版本由 setup 写入 |
| `src/contact/sql/schema.sql` | 当前联系人投递库 v5 |
| `src/contact/sql/migrations/` | v1 → v5 的逐步迁移 |
| `src/package/sql/schema.sql` | 当前包登记 v4 与 ACL 能力表；要求附加 pkg_registry、acl 数据库 |
| `src/package/sql/compat/v3/schema.sql` | 仍被生产复用的 v3 基础定义与能力表修复；不添加 v4 签名列 |
| `src/package/sql/migrations/v4_add_signing_evidence.sql` | 已有包登记补充 v4 签名证据列 |
| `src/package/sql/has_signing_evidence.sql` | 初始化与恢复共用的签名证据列检查 |
| `src/package/sql/registration_facts.sql` | v4 安装与恢复快照，包含完整签名证据 |
| `src/package/sql/compat/v3/registration_facts.sql` | 恢复旧 v3 数据库或旧日志时使用的登记投影 |
| `src/package/sql/runtime_views.sql` | 每个配置连接的临时 Lib/App 视图 |

旧联系人 v1 与包登记 v2 的测试定义分别位于 `tests/fixtures/contact/v1/schema.sql`、`tests/fixtures/package/v2/schema.sql`。版本判断、事务边界、执行顺序和数据库内容不因文件位置或命名调整而变化。

# SQL 文件组织

运行 SQL 紧邻实际执行数据库操作的 Rust 代码。短查询直接用于 prepare、query_row 和 execute，重复复用的查询可以在所属模块内定义常量。完整结构和必要的版本升级使用 SQL 文件。

## 包管理 SQL

- `src/package/files.rs` 负责版本登记、精确依赖、能力投影和安装恢复，查询位于对应操作中。
- `src/setup.rs` 负责连接附加、临时视图和升级顺序，相关 SQL 位于连接初始化流程中。
- `src/package/schema.sql` 保存完整的 v5 初始化结构。
- `src/package/migrations/` 保存 v4 签名证据升级和 v5 多版本升级。调用方显式选择并依序执行必要的升级，迁移文件记录各自版本的固定结构。
- 历史 v3 结构用于测试时从 `tests/fixtures/package/v3/schema.sql` 读取。旧安装日志的兼容投影由包登记快照操作生成。
- ACL 恢复流程执行明确的建表语句，安装与恢复复用实际存储操作。

## 命名规则

- 文件名统一为小写 `snake_case`。
- `schema.sql`：新库的基础结构。最终数据库版本由调用方的初始化及迁移流程决定。
- Rust 模块内复用的 SQL 常量使用 `UPPER_SNAKE_CASE` 命名。
- `<purpose>.sql`：有独立用途的完整脚本，如 `registration_facts.sql`、`runtime_views.sql`；不用于逐条存放短查询。
- `migrations/v4_add_signing_evidence.sql`：`v<目标版本>_<具体变更>.sql`；版本不补零，变更名称使用动词和对象。迁移顺序由调用代码显式控制，不依赖文件名排序。
- `compat/v3/schema.sql`：生产仍依赖的历史定义，版本放在目录名中，同类文件保持相同名字。
- `tests/fixtures/<domain>/v1/schema.sql`：仅供测试的历史结构，同样使用 `v<版本>` 目录。

## 所有者与执行入口

| 路径 | 用途 |
| --- | --- |
| `src/file/sql.rs` | 文件索引读取、创建、偏移提交、删除、旧索引导入及连接配置 |
| `src/file/sql/schema.sql` | 业务 database 中 native_files 表的基础结构 |
| `src/setup/sql/schema.sql` | setting v3 的 setting、proxy_location 表；版本由 setup 写入 |
| `src/contact/sql/schema.sql` | 当前联系人投递库 v5 |
| `src/contact/sql/migrations/` | v1 → v5 的逐步迁移 |
| `src/package/files.rs` | 包版本登记、精确依赖、能力投影、安装事务和历史日志快照操作 |
| `src/package/schema.sql` | 包登记 v5 的完整初始化结构，以及 ACL 能力表 |
| `src/package/migrations/` | v4 签名证据升级与 v5 多版本升级 |

旧联系人 v1 与包登记 v2 的测试定义分别位于 `tests/fixtures/contact/v1/schema.sql`、`tests/fixtures/package/v2/schema.sql`。版本判断、事务边界、执行顺序和数据库内容不因文件位置或命名调整而变化。

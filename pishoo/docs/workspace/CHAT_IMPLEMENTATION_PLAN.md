# Chat 实施计划（已归档）

本文件不再作为实施依据。当前唯一有效的计划是
[CHAT_MODULE_IMPLEMENTATION_PLAN.md](CHAT_MODULE_IMPLEMENTATION_PLAN.md)。

数据库按当前 schema 直接重建，不保留旧 Workspace Chat 表、权限集合表、兼容迁移、跨库
导入或 410 托底接口；新能力只能通过内置 descriptor 注册。

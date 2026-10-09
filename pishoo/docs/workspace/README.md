# pishoo Workspace 设计资料

本目录用于沉淀 pishoo Workspace 的产品边界、原始构想和实施计划。

## 文件索引

- [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md)：结合当前 pishoo/daccess 基座整理的实施方案。
- [VM_TESTING.md](VM_TESTING.md)：双 VM 真实环境、证书、浏览器入口和交互步骤；涵盖旧基线与当前集成分支的运行记录。
- [VM_ACCEPTANCE_2026-10-08.md](VM_ACCEPTANCE_2026-10-08.md)：Alice/Bob 双 VM 首轮实测、流程修复、截图及未解决的 HTTP/3 异常。
- [VM_EXTENDED_ACCEPTANCE_2026-10-08.md](VM_EXTENDED_ACCEPTANCE_2026-10-08.md)：后续补测结果；涵盖联系人生命周期、审批时效、资料、身份边界、并发聊天、大历史及完整 VM 重启。
- [VM_RULE_MANAGEMENT_2026-10-09.md](VM_RULE_MANAGEMENT_2026-10-09.md)：访问规则新增、编辑和删除的恢复原因，以及双 VM 实际权限效果验证。
- [VM_HOST_ACCEPTANCE_2026-10-09.md](VM_HOST_ACCEPTANCE_2026-10-09.md)：Mac 与 VM 共享网络、原有 Bob 好友申请恢复、双方聊天，以及环境启动和回滚步骤。
- [VM_INTEGRATION_ACCEPTANCE_2026-10-09.md](VM_INTEGRATION_ACCEPTANCE_2026-10-09.md)：迁入 `feat/pishoo-integration` 后重新部署双 VM，复验好友、双向授权、聊天、撤权恢复、离线重试、审批、规则管理及管理入口读取。
- [source/_workspace 畅想.md](source/_workspace%20畅想.md)：原始 Workspace 构想文档，原样归档。
- [source/图片和附件/img_v3_0215k_82b88544-5890-4d86-be5b-dc76c294aa8g.jpg](source/图片和附件/img_v3_0215k_82b88544-5890-4d86-be5b-dc76c294aa8g.jpg)：手绘 Workspace 导航与首页构想。
- [source/图片和附件/image.png](source/图片和附件/image.png)：联系人/聊天型双栏工作区参考图。
- [BRANCH_MIGRATION_2026-10-09.md](BRANCH_MIGRATION_2026-10-09.md)：当前目录原地切换到 `feat/pishoo-integration` 的迁移记录和历史验收范围。

验收报告随代码提交；截图、原始响应、日志和临时补丁仅保留在本地 `vm-evidence/`，已由 `.gitignore` 排除。

## 当前结论

此前实现把 Workspace 误收缩成了 daccess 管理控制台。正确边界是：

- Workspace 是 pishoo 面向 profile owner/管理员的统一个人工作台；
- daccess 只是 Workspace 中联系人、审批和访问控制能力的一个领域服务；
- 第一阶段优先实现联系人、审批请求和快捷设置；
- API 扩展与应用管理先建立稳定路由和侧边栏占位，后续再设计 Wasm、安装和生命周期协议。

原始资料只作为产品意图来源；原方案保留为分支历史资料。当前实现边界及冻结接口以 [design/README.md](../../../design/README.md) 和 [当前集成说明](../../DACCESS.md) 为准。

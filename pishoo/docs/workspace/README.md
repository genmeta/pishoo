# pishoo Workspace 设计资料

本目录用于沉淀 pishoo Workspace 的产品边界、原始构想和实施计划。

## 文件索引

- [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md)：结合当前 pishoo/daccess 基座整理的实施方案。
- [source/_workspace 畅想.md](source/_workspace%20畅想.md)：原始 Workspace 构想文档，原样归档。
- [source/图片和附件/img_v3_0215k_82b88544-5890-4d86-be5b-dc76c294aa8g.jpg](source/图片和附件/img_v3_0215k_82b88544-5890-4d86-be5b-dc76c294aa8g.jpg)：手绘 Workspace 导航与首页构想。
- [source/图片和附件/image.png](source/图片和附件/image.png)：联系人/聊天型双栏工作区参考图。

## 当前结论

此前实现把 Workspace 误收缩成了 daccess 管理控制台。正确边界是：

- Workspace 是 pishoo 面向 profile owner/管理员的统一个人工作台；
- daccess 只是 Workspace 中联系人、审批和访问控制能力的一个领域服务；
- 第一阶段优先实现联系人、审批请求和快捷设置；
- API 扩展与应用管理先建立稳定路由和侧边栏占位，后续再设计 Wasm、安装和生命周期协议。

原始资料只作为产品意图来源；具体安全边界、数据模型和实施顺序以实施方案为准。

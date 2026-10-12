# Pishoo Workspace

Workspace 位于 `/std/workspace/`，提供联系人、授权规则、审批、聊天、资料和 Lib 目录。
页面访问当前身份的本地管理 API；远端请求通过已验证的 DHTTP 身份发送。

- [前端开发与测试](../../workspace/README.md)
- [聊天投递协议](MESSAGE_PROTOCOL.md)
- [配置 API](../config-api.md)
- [管理命令与 Lib 安装](../management.md)
- [系统 HTTP 路径与升级](../system-paths.md)

联系人权限和审批由 daccess 提供；WASM 组件清单和目录本身不授予 API 访问权限。

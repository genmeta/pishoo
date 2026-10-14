# Pishoo Workspace

Workspace 位于 `/std/workspace/`，提供联系人、授权规则、审批、聊天、资料和 Lib 目录。
页面访问当前身份的本地管理 API；远端请求通过已验证的 DHTTP 身份发送。

- [前端开发与测试](../../../std/apps/workspace/README.md)
- [聊天投递协议](MESSAGE_PROTOCOL.md)
- [配置 API](../config-api.md)
- [管理命令与 Lib 安装](../management.md)
- [系统 HTTP 路径与升级](../system-paths.md)

联系人权限和审批由 daccess 提供；WASM 组件清单和目录本身不授予 API 访问权限。

我的信息页保留「分享我的介绍页」，提供 `/std/workspace/#/intro` 的预览与复制。介绍页读取 `GET /std/profile/context`，只显示公开资料；添加好友在新标签页进入访客自己的 Workspace，完整预填目标身份，匿名访客需先填写自己的身份名称。介绍上下文、头像和 App 静态资源需要主人显式配置普通 daccess ACL，不随安装自动授权。

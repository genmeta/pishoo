# Pishoo Workspace

Workspace 位于 `/app/workspace/#/`，提供联系人、授权规则、审批、聊天、资料和 Lib 目录。

公开个人介绍页位于 `/app/workspace/#/intro`，展示头像、显示名称和身份名称；展示身份时省略
`.dhttp.net` 后缀，链接和申请仍使用完整身份。可在“我的信息”中预览、复制介绍页链接。
主人允许普通公开 ACL 后，访客无需成为联系人即可查看，介绍页只读取 `/api/workspace/intro/context` 与公开头像，不读取管理 API。
已认证访客的按钮显示“以 {身份名称} 的身份添加”，点击后在新标签页打开自己的
`https://<访客身份>/app/workspace/#/contacts/new?target=<名片主人完整身份>`，目标自动预填，
确认后才发送申请，原介绍页保留。匿名访客先填写自己的身份名称，再在新标签页打开工作台；
这只决定跳转地址，不代替身份认证。
自己的介绍页提供编辑信息入口。介绍页上下文包含当前连接的访客身份，使用
`Cache-Control: private, no-store`；前端静态文件与上下文使用普通 daccess ACL，
Workspace 管理页面及 API 继续沿用现有授权。

Workspace 页面访问当前身份的本地管理 API；远端请求通过已验证的 DHTTP 身份发送。

- [前端开发与测试](../../workspace/README.md)
- [聊天投递协议](MESSAGE_PROTOCOL.md)
- [配置 API](../config-api.md)
- [管理命令与 Lib 安装](../management.md)
- [系统 HTTP 路径与升级](../system-paths.md)

联系人权限和审批由 daccess 提供；WASM 组件清单和目录本身不授予 API 访问权限。

# Profile 资料服务

Profile 是普通 WASI HTTP Lib，独立拥有展示资料与头像。入口为 `/std/profile`（GET/PATCH）、`/std/profile/avatar`（GET/PUT/DELETE）及 `/std/profile/context`（GET/HEAD）。介绍上下文返回真实握手的 viewer_name 和 is_owner，供 Workspace 介绍页使用；公开读取仍经过原生 ACL，写操作另外核对主人名称与 owner hash。

私有数据为 `db/<publisher>/profile/profile.db` 与该目录内 `profile/` 头像文件。Profile 不保存联系人、收藏、申请队列、消息或授权决定，不依赖 Message，不导出 CLI 投递任务。旧 Workspace 私有数据保留，不自动导入。

```sh
WASI_SDK_PATH=/path/to/wasi-sdk sh build.sh --publisher YOUR_NAME.dhttp.net
```

Workspace App 精确依赖同发布者 Profile 0.1.0 与 Message 0.1.0。原生 Contact 的收藏与出站申请分别使用 `/std/contact/local/saved`、`/std/contact/local/requests`；联系人和 ACL 继续由 daccess 管理。首次安装 Profile 前须显式移除占用 `/std/profile` 的旧 Workspace Lib；旧 App 的依赖保护要求先卸载旧 App。卸载各包保留数据。

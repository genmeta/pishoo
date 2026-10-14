# Pishoo 标准应用与业务库

标准发行的 App/Lib 源码集中在此目录，独立于 `pishoo/src` 宿主实现。当前 Workspace App、Profile Lib 和 Message Lib 均为初始版本 `0.1.0`。

| 目录 | 职责 |
| --- | --- |
| [apps/workspace](apps/workspace/README.md) | Workspace 前端 App |
| [libs/profile](libs/profile/README.md) | 资料与头像 Lib |
| [libs/message](libs/message/README.md) | 1v1 消息 Lib |

它们使用普通 App/Lib 的打包、安装、卸载及授权机制。原生 ACL、Contact、Package 和其他宿主基础设施仍由 Pishoo 实现；`std` 源码目录本身不授予特权。Journal 示例保留在 `pishoo/examples/journal`。

从仓库根目录构建三份签名制品：

```sh
WASI_SDK_PATH=/path/to/wasi-sdk sh std/apps/workspace/package.sh --publisher YOUR_NAME.dhttp.net
```

输出仍为 `target/workspace-dist/default-artifacts/{message-lib,profile-lib,workspace-app}.webc`。仅构建前端可在 `std/apps/workspace` 中运行 `bun run build`。

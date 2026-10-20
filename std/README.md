# Pishoo 标准应用与业务库

标准发行的 App/Lib 源码集中在此目录，独立于 `pishoo/src` 宿主实现。当前 Workspace App 为 `0.1.3`；Profile Lib、Message Lib 及 Calendar App/Lib 保持 `0.1.0`。

| 目录 | 职责 |
| --- | --- |
| [apps/workspace](apps/workspace/README.md) | Workspace 前端 App |
| [libs/profile](libs/profile/README.md) | 资料与头像 Lib |
| [libs/message](libs/message/README.md) | 1v1 消息 Lib |
| [apps/calendar](apps/calendar/README.md) | 中英文日历前端 App |
| [libs/calendar](libs/calendar/README.md) | 私有日程存储与 CRUD Lib |

它们使用普通 App/Lib 的打包、安装、卸载及授权机制。原生 ACL、Contact、Package 和其他宿主基础设施仍由 Pishoo 实现；`std` 源码目录本身不授予特权。Journal 示例保留在 `pishoo/examples/journal`。

业务制品独立发布到商店，不随 Pishoo 宿主安装包交付，也不在首次启动自动安装。安装 Pishoo 与提供 `genmeta package install` 的新版 gmutils，配置身份并启动 Pishoo 后，由身份所属账户执行：

```sh
genmeta package install std.dhttp.net/workspace --id YOUR_IDENTITY
```

宿主会下载、验签并递归安装 App 所声明的精确 Profile、Message 依赖；所需版本须已发布到商店。两个宿主软件包安装顺序不限，Pishoo 不依赖 gmutils 启动。

从仓库根目录构建三份签名制品：

```sh
WASI_SDK_PATH=/path/to/wasi-sdk sh std/apps/workspace/package.sh --publisher YOUR_NAME.dhttp.net
```

从源码构建并启动 Pishoo、安装 Workspace 及其依赖，可运行 `WASI_SDK_PATH=/path/to/wasi-sdk python3 std/apps/workspace/dev.py --id YOUR_IDENTITY`。此开发命令每次都会卸载并重装这三个包，清除它们的业务数据、对应 ACL 和审批记录。前置条件、清理范围和发布者选项见 [Workspace 开发说明](apps/workspace/README.md)。

输出为 `target/workspace-dist/{message-lib,profile-lib,workspace-app}.webc`，供独立上传商店。仅构建前端可在 `std/apps/workspace` 中运行 `bun run build`。

Calendar 独立发布前后端，App 精确依赖同发布者的 Calendar Lib `0.1.0`：

```sh
WASI_SDK_PATH=/path/to/wasi-sdk sh std/apps/calendar/package.sh --publisher YOUR_NAME.dhttp.net
```

输出 `target/calendar-dist/{calendar-lib,calendar-app}.webc`。开发与接口见上表的 Calendar 文档。

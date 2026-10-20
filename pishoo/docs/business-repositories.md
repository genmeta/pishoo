# 宿主与独立产品仓库

Pishoo 只维护原生服务、预留 std API、通用 App/Lib 安装与运行机制、WIT/SDK、客户端示例和宿主测试夹具。业务源码不通过子模块、符号链接或复制源码重新引入宿主。标准发布者的 Lib 仍是独立安装的业务制品；`/std` 挂载不表示业务实现属于宿主。

本地工作目录按同级独立 Git 仓库组织：

| 仓库 | 内容 | 原路径 |
| --- | --- | --- |
| [workspace](../../../workspace/README.md) | Workspace App 与前端测试 | std/apps/workspace |
| [profile](../../../profile/README.md) | Profile Lib、OpenAPI 与 API 文档 | std/libs/profile |
| [message](../../../message/README.md) | Message Lib、OpenAPI、协议与 API 文档 | std/libs/message |
| [calendar](../../../calendar/README.md) | app/ 前端和 lib/ 后端，分别发布 | std/apps/calendar、std/libs/calendar |
| [journal](../../../journal/README.md) | Journal App/Lib，分别发布 | pishoo/examples/journal |

各仓库的 MIGRATION.md 记录来源版本。仓库独立并不改变签名包名、版本、精确依赖或对外 HTTP 接口。业务 Lib 依赖未发布的 Rust SDK 时，默认引用同级 Pishoo 的 wit/；也可指向 SDK 发行包中的 crate，不依赖宿主业务源码。

宿主构建、普通测试及 SDK 打包无需检出业务仓库。原生与预留接口见 [std API](std-api.md)。需要真实业务组件的既有忽略验收测试仍在宿主侧维护，通过显式环境变量读取外部构建输入：

```sh
export PISHOO_PROFILE_REPO=/absolute/path/to/profile
export PISHOO_MESSAGE_REPO=/absolute/path/to/message
export PISHOO_JOURNAL_REPO=/absolute/path/to/journal
```

先在对应产品仓库运行 build.sh，再按测试的环境要求单独运行相关忽略测试。路径未配置会明确失败；不会自动检出、构建或导入业务制品。测试夹具中的 host-client、lib-http-probe 和小型 WASI 测试组件仅验证宿主协议，不是业务发行包。

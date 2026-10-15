> 2026-10-15：统一命令与构建流程见 [genmeta package](genmeta-package.md)。下文的打包细节由 pishoo-pack 承担；genmeta package build 先执行本地构建配置。

# genmeta package install 实现设计与 Agent 任务书

日期：2026-10-14。状态：待实现。配套任务：[pack](genmeta-pack-design.md)、[upload](genmeta-upload-design.md)。本任务实现从商店安装的CLI，以及用户要求的源码仓库链接展示。

## 1. 目标和已确认边界

```sh
genmeta package install workspace
```

默认从固定商店解析App，选择最新已发布版本，为当前默认身份安装；pishoo负责可信下载、验签、递归精确Lib依赖、登记和即时加载。

- 不支持本地文件、目录、URL安装；没有 source/local/offline/store 选项。
- 不提供Github下载、克隆或远程构建。
- 不复制宿主安装器、写identity下的app/lib文件或config.db；在线调用已有安装API。
- 不自动启动/重启服务、提权、改变listen/proxy配置或创建默认身份。
- 不改ACL、授予建议权限、增加信任名单或安装账本。
- 现有pishoo离线开发命令及前端文件入口不由本任务删除或扩展。

实施前阅读本仓 `AGENTS.md`、`design/README.md`、`design/pishoo-interfaces.md`、`design/workspace-chat-interfaces.md`、[包规范](package-spec.md)、[管理说明](management.md)，以及商店当前契约/WEBC协议。禁止扩展底层接口和Server/Sandbox成员。

## 2. 命令契约

```sh
genmeta package install TARGET [-i NAME | --id NAME] [--version MAJOR.MINOR.PATCH] [--type app|lib]

genmeta package install workspace
genmeta package install dev.dhttp.net/workspace
genmeta package install workspace --version 0.2.0
genmeta package install workspace --id alice.smith
genmeta package install dev.dhttp.net/chat --type lib
```

| 输入 | 规则 |
| --- | --- |
| TARGET | 必填：合法短名，或规范化完整publisher/name；不接受 `name@version` 新语法 |
| kind | 默认app；显式lib用于单独后端安装，不自动跨类型回退 |
| version | 可选无前导零的三段数字；省略时读取商店最新已发布版本并固定为精确值 |
| id | 安装到哪个本机身份；省略时用现有默认身份，不是包publisher |

立即拒绝明显本地路径、.webc、URL、非法名称/版本/类型以及多余参数。完整publisher/name本身包含一个 `/`，不能因为有斜杠而错误判定为文件。不存在的合法商店名称报商店未找到，不能尝试读取同名文件。

身份选择复用dhttp-home的正常名称解析、HomeScope::User及默认profile解析；支持DHTTP_HOME。没有默认身份或凭据不完整时明确提示设置身份/使用--id，不选择第一个目录，不退匿名。

## 3. 固定商店、短名及版本解析

固定商店 `store.dhttp.net`、owner_hash：

```text
ffc3e50e1dfb7baca1cc07a7cbfcf8cb34a9bf5a9c25024fcf9a9786bb230746
```

解析列表/详情的请求也必须在发送前使用现有expect_remote_owner_hash，响应再核验名称及owner_hash；不信任未钉住的浏览代理结果。复用现有DNS/Network初始化，规范化Uri/Header类型，不跟随重定向。

短名解析请求所选集合 `/store/v1/apps?q=<name>` 或 `/store/v1/libs?q=<name>`。现有q是模糊查询，必须遍历实际分页后仅保留 `name == TARGET`、对应kind且published的结果，再按完整ID去重；不能从第一页第一项或标题匹配安装。

| 精确短名结果 | 行为 |
| --- | --- |
| 0项 | 未找到/无可用版本，失败 |
| 1个完整ID | 使用该publisher/name |
| 多个完整ID | 失败并列出完整名称，要求重新指定；无交互默认选择、官方别名或硬编码workspace发布者 |

分页每页最多100；校验page/page_size/total及数据条目，设置一次解析总期限与有限响应字节/条目预算。建议商店解析总期限120秒、每次JSON2 MiB、最多10000个候选；如果预算耗尽、分页失败或目录在查询期间明显不一致，报解析未完成，不能把已看到的唯一候选当全局唯一。无须新增商店exact-name API。本方案不承诺目录分页快照；得到唯一候选后仍用其完整ID读取详情。

完整ID直接读 `/store/v1/<collection>/<publisher>/<name>`；省略version从这份可信详情取得最新published精确版本。显式version读取对应 `/versions/<version>`，绝不能目标下架后换版本。

核对详情的id、publisher、name、kind、version、status、合法大小/摘要及固定download_path。App不要求OpenAPI。商店latest依现有三段数值排序；CLI不按字符串、上传时间或本机比较推断最新。

## 4. 在线安装请求和身份

使用所选本机身份的真实Endpoint向该身份自身服务发请求，核验响应RemoteAuthority名称及owner_hash与本地凭据一致。必须遵守当前严格握手/OCSP要求，不禁用TLS校验、不伪造身份头。只连接成功不等于已调用正确Pishoo服务。

```text
POST https://<selected-identity>/std/pishoo/store-install
Content-Type: application/json
Accept: application/json
```

```json
{
  "kind": "app",
  "publisher": "dev.dhttp.net",
  "name": "workspace",
  "version": "0.2.0"
}
```

kind始终显式发送：既有服务省略kind默认lib，而CLI默认app。不要扩大请求JSON或把repository/download URL/owner_hash从CLI塞进安装请求。

宿主重新请求固定商店、核验精确发布证明、签名、摘要和大小；CLI的解析结果不代替宿主验证。目录版本选择与安装之间版本下架应失败，不能静默替换。CLI不承担另一套递归下载、编译或登记逻辑。

宿主按现有规则最多解析100个Lib，拒绝循环、版本冲突和同类短名冲突；精确匹配包核验复用，按依赖顺序单制品提交。父失败保留已成功依赖；CLI错误说明该规则，不声称全局回滚。安装不自动修改ACL。

网络截止时间：商店解析总期限120秒；安装为有界同步请求，建议CLI总等待15分钟以容纳递归下载/编译。使用现有timeout与结果，不增加Server deadline/report、后台作业或202状态API。截止时间不是取消服务端安装的保证。

## 5. 成功、错误和未知结果

只有收到可信成功响应且其id/kind/version与选定包一致，才报告安装成功；在线变更即时生效，App入口从短名及已保存entry派生为 `https://<identity>/app/<name>/`。

stdout成功输出JSON：`id,kind,version,identity`，App额外输出`entry_url`。stderr输出选定完整包和版本、目标身份及进度；错误包括同名多发布者、版本下架、冲突、依赖失败和服务不可达。

运行服务不可达、身份被禁用或服务路由不存在时，提示启动/启用正确身份服务；不改为离线写盘。保留服务端daccess当前拒绝/审批响应，不添加用户不存在的状态查询。

请求中断或超时显示“安装结果未知，服务端可能仍在安装”。可以有界只读查询现有已登记/已加载apps/libs确认实际状态，但仅同版本条目存在不能证明本次请求执行完成或内容一致，不能据此报告整个操作成功。不自动重复写请求、不回滚已装依赖。用户可检查目录/日志并再次明确执行命令。

## 6. repository展示

pack agent负责宿主从已签`package.wapm.repository`派生可选`repository`；upload agent负责商店兼容和透传。install agent负责Workspace前端显示：

- 在已安装App/Lib详情以及商店App/Lib详情显示“源码仓库”链接。
- 使用相应已加载/磁盘/商店条目自身值，不能用商店新版填补本地旧版元数据。
- 原生summary→前端详情的映射保留repository；支持无字段条目。
- 链接只作展示；限有效HTTPS URL，禁止脚本/非网页协议，使用普通链接、新标签页及`rel="noopener noreferrer"`，不嵌入或执行远程内容。
- 不克隆、不下载、不在打包/安装中访问该URL，不产生新页面、导航项或持久状态。

具体TypeScript成员提案仅为既有 `LibSummary.repository?: string` 与 `StoreSummary.repository?: string`，StoreDetail继承；不新增运行时结构。依本仓架构契约，实施前把这两项差异列入对用户的具体成员核对；不能借此修改其他冻结字段。本文件不自行写入冻结清单。

## 7. 实现归属和共同前置条件

本agent唯一负责 gmutils `genmeta/src/install.rs`及安装专属测试；Pishoo `workspace/src/pages/AppsPage.tsx`、`workspace/src/api/types.ts`、必要的i18n/展示样式及前端测试。避免修改宿主安装函数，确有bug则给出证据和方法体级修复范围再协调归属。

```rust
pub(super) async fn run(target: String, id: Option<String>, version: Option<String>, kind: String)
    -> Result<(), snafu::Whatever>;
```

参数解析和launcher装配由pack agent唯一编辑main、共享Cargo和Cargo.lock；所需依赖/调用点交给该agent。自己的网络helper保持module-local/stateless，不新增State/Context、Guard、自定义包身份或通用helper API层。

商店新manifest规范尚待适配，是upload/install共同联调前置条件。store-server当前CONTRACT明确要求openapi_json持久化冲突的具体SQL/字段差异先经用户批准。install agent不修改商店，不回退旧OpenAPI包元数据，按真实协议错误报告未兼容。

默认在临时本机身份/兼容临时商店完成验证；不得启动/关闭用户生产服务、改其凭据/ACL、发布线上包或部署商店。固定信任锚的生产校验必须保留；测试可通过模块内请求级依赖注入或私有测试fixture建立可信连接，不能新增生产transport开关或替换锚。

## 8. 验收与交付

- 短名唯一、跨分页同名多发布者、模糊相似标题、非法分页/预算耗尽都正确处理。
- 完整ID、指定version、默认kind=app及显式lib请求正确；kind永不省略。
- 本地路径、.webc、Github URL、@version及不存在的合法名称均不会触发文件安装。
- 默认身份缺失、显式身份、商店owner错误和目标服务owner错误正确拒绝。
- 兼容临时商店中Workspace App递归安装Workspace/Chat Lib，安装入口可查询；匹配依赖复用，版本冲突拒绝，父失败保留已成功依赖。
- 不可达/禁用服务不离线落盘；超时显示未知，不自动重试写入或伪报全部成功。
- 仓库链接对商店/本地App和Lib都显示；无字段隐藏，非法URL不成为可点击危险链接，不加载远程源码。
- 回归现有genmeta命令帮助/版本与Workspace详情行为；完成适当Rust check/test、前端类型/构建及交互验证。

交付包含安装模块、前端展示、所需共享依赖列表、测试结果及商店联调前置条件状态。pack agent完成共享入口后联合验收pack→upload→install；任何未获批准的冻结成员变化须停在具体差异提案，而不是用替代容器绕过。

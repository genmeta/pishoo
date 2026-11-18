# Pishoo 原生系统管理

2026-11-17 本轮已实现：`/std/sys/listen`、`/std/sys/mount`、`/std/sys/pkg`、`/std/sys/pkg/list`、`/std/sys/pkg/install` 统一按真实调用者的现有路径与方法 ACL 授权，移除接口内部额外的身份本人限制。新建本机身份不再自动创建设备管理员联系人，也不复制所有方法的 `allow /`；设备管理员资格与个人身份授权分别维护。已有联系人和 ACL 保留，后续仍通过正常联系人与 ACL 接口显式管理。

## 最新设计约定（待实现）

2026-11-17。首位管理员采用首次初始化时有效的默认身份，绑定规范化名字与证书 owner hash 后持久登记一次，不再由身份扫描或成功监听顺序选择。没有有效默认身份时等待设置；登记后更改默认身份不转移管理员资格，管理员名单清空也不重新触发自动登记。

`init` 是香橙派镜像内置的服务及名字，普通 Pishoo 安装不依赖它。镜像对接方案是让 `init` 与 `genmeta` 共用 Pishoo 的通用本机身份、默认身份及服务配置能力；首次登记不要求预先通过 `/sys` 获得管理员授权，也不根据 `init` 这个名字赋权。镜像引导服务自身不应设为用户默认身份，避免占用首位管理员登记。

Pishoo 负责身份与管理员事实的核验、持久保存和服务配置生效；镜像负责引导页面、初始化访问策略、热点及 `init` 的启停。管理员登记持久成功且正常管理入口可用后，再由镜像关闭引导入口和热点。下面记录当前实现，首次监听登记等行为尚待按这些约定调整；通用本机对接能力的具体接口仍待设计。

## 当前实现

2026-11-17。管理员实现已从独立 worktree 合入当前分支。身份创建按用户确认采用已有证书与私钥导入。系统管理接口由 Pishoo 原生内置模块实现。首个成功启用的本机 Pishoo 服务身份登记为管理员。

## 管理员

设备支持多个管理员。每项授权绑定规范化 DHTTP 名字与证书的 owner hash。管理员可以只存在于另一台设备。设备级数据库位于 `<DhttpHome>/.system/database`，Linux 系统服务的默认位置为 `/etc/dhttp/.system/database`。普通用户运行沿用该进程的用户 DhttpHome。

管理员库 v2 的 `administrator` 以名字为主键，仅保存 `name` 和 `subject_id`。v1 升级在同一事务中保留管理员主体绑定、首次登记及身份生命周期记录。每位管理员均拥有整套 `/sys` 权限。后续成员通过管理员 API 显式添加，设备管理员名单不向个人身份复制联系人和 ACL。

`/sys` 由 Pishoo 当前加载的管理员身份提供。每次请求依据真实握手核验承载身份和来访身份的名字与 owner hash，并分别检查管理员名单。

权限集合目录 `GET /std/acl/permissions` 为管理员附加“系统管理 / System administration”组，包含全部内置 `/sys` 操作。组只在承载身份和真实来访身份均在设备管理员名单中时返回；匿名、非管理员及主体不符的同名身份均不显示。每次读取核验当前名单，移除后再次读取即隐藏。目录原有路径 ACL 继续生效。

`?details=1` 下该组额外带有 `management="administrator"` 和规范化名字数组 `members`，同时保留双语 `title` 与 `endpoints`。普通目录仍返回路径到方法数组的对象。Workspace 在权限集合页显示该组、接口与实际管理员成员，标注由设备管理员名单管理。该组不参与普通 ACL 授权、联系人权限申请或授予；成员通过 `/sys/admin` 管理。

## 首位管理员

首次登记由 Pishoo 的身份启用流程完成。身份具有有效凭据、已有服务监听配置，并成功加载和注册监听后，才参与首位管理员选择。管理员名单为空时，首个符合条件的身份登记为管理员。

已有多个服务身份时，沿现有串行发现和成功启用顺序选择首个身份。管理员名单已有记录时采用现有授权。

设备数据库的单行 `device` 记录保存 `first_administrator`，与首次授权在同一事务提交。后续启用的身份沿用普通身份权限。管理员的变更以当前管理员表为准，首次登记记录用于保持这项选择的一次性语义。

后续使用 `POST /sys/user` 创建本机身份时，沿用普通身份初始化的默认权限，管理员资格通过 `POST /sys/admin` 单独设置。新身份完成发布、启用后，才具有服务监听能力。

## 内置 API

| 路径 | 方法 | 用途 |
| --- | --- | --- |
| `/sys/version` | GET | Pishoo 版本 |
| `/sys/info` | GET | 版本、OS、架构、承载身份 |
| `/sys/admin` | GET、POST、DELETE | 管理员清单、登记和移除 |
| `/sys/user` | GET、POST、DELETE | 本机身份清单、导入和删除 |
| `/sys/reload` | POST | 重新加载运行身份的 Lib，随后扫描身份和配置 |
| `/sys/restart` | POST | 收尾当前服务并重新执行 Pishoo 进程 |

响应使用 `Cache-Control: no-store`。POST JSON 的总请求体上限为 1 MiB。读取 API 不接受 query 或正文。DELETE 恰好接受一个 `name` query。未知方法返回带 Allow 的 405。控制 API 使用空请求体，接受后返回 202，并在响应正文交付运输层后提交进程控制命令。

管理员登记请求如下。调用者须已具备设备管理员资格。

```json
{"name":"admin.name","subject_id":"64位小写十六进制owner hash"}
```

名字按 dhttp-home 的短名、`~` 与全名规则归一化。同名同主体幂等更新，同名不同主体返回 409。管理员移除使用 `DELETE /sys/admin?name=NAME`。

身份导入请求如下。

```json
{
  "name":"new.name",
  "certificate":"-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----\n",
  "private_key":"-----BEGIN PRIVATE KEY-----\n...\n-----END PRIVATE KEY-----\n",
  "ocsp":"可选的base64编码OCSP"
}
```

宿主验证 DHTTP 固定根下的证书链、名字、有效期、ClientAndServer 用途、私钥公钥匹配和 OCSP。缺省 OCSP 复用宿主已有获取及缓存算法。成功创建的身份默认使用 `all` 监听，之后可通过现有身份配置接口调整。同名、同主体的成功重试返回 `created=false`，并核对已发布目录的主体。

删除作用于设备发现的本机身份，包含其本机凭据、配置和业务数据。Linux 系统服务沿用现有多账户身份发现，删除恢复日志保存实际身份路径。管理员名单通过 `/sys/admin` 管理。删除已不存在的本机身份返回 `deleted=false`。

## 新身份的权限

身份创建不额外生成管理员联系人，也不向管理员授予该身份的根路径权限。身份本人保留普通初始化的默认权限，其他联系人按路径和方法显式授权。联系人分类不构成额外的系统管理角色。

设备管理员表负责每个 `/sys` 请求的设备权限；个人身份的联系人与 ACL 管理接口负责该身份的授权。已有联系人及其权限不会因为这次移除自动继承逻辑而被批量删除。

## 持久化与恢复

`managed_user` 保存名字、主体、实际身份目录路径与 `pending`、`active`、`deleting`、`removed` 阶段。凭据与宿主数据库先写入 `.system` 内的私有暂存身份，完整同步后进入 `.system/pending/`。设备数据库提交 pending 行，随后以禁止覆盖的原子 rename 发布至正常身份目录。发布完成后保存 active 阶段。服务成功启用后，执行一次性的首位管理员登记。

启动和每轮扫描恢复已提交操作。恢复在发布与删除前核对真实目录及其证书主体。删除先持久化意图，再将旧目录移到该身份所属 DhttpHome 的 `.system/trash/`，保存 removed 阶段后清理。removed 恢复只清理旧目录，这使随后出现的同名目录得到保留。数据库、目录及凭据写入失败均保留可检查的已提交恢复事实。

## 原生内置模块

`/sys/version`、`/sys/info`、`/sys/admin`、`/sys/user`、`/sys/reload`、`/sys/restart` 由宿主 Rust 模块直接处理。管理员数据库、真实调用者授权、身份管理及进程控制构成通用核心，使用通用本机身份承载管理接口。

`/sys` 为原生命名空间。未知系统路径返回 404。通用系统管理接口不需要 WASM 运行时参数。

根路径 `/sys` 保留命名空间，不提供读取入口；管理员请求该根路径返回 404。版本与承载身份信息分别由 `/sys/version`、`/sys/info` 提供。

## 实现边界与验证

`Server` 新增实际路由资源 `system_router: Router`。`run` 局部持有设备 SQLite 连接和有界控制通道，身份扫描通过 `reconcile_device` 装配系统路由并登记首个成功启用的本机管理员。`system` 按 HTTP 边界、管理员存储、身份生命周期拆分。`reconcile_profiles` 增加设备管理员数据库参数，在每个身份监听成功后执行登记，使用真实启用顺序。现有 `fetch_ocsp`、`cache_ocsp` 扩大到 crate 内复用。移除自动联系人继承后，`access_for_system` 仅作为测试重导出保留。

回归测试位于 `tests/unit/system/`，并通过 Server 的正常 Router 验证设备权限。凭据测试复用 OpenSSL 3 夹具。实际进程重启需在独立服务进程中联调。

本地回归包括设备管理员授权、首位管理员的一次性登记、后续身份权限隔离、身份导入与删除恢复、身份管理接口的显式 ACL 授权，以及独立执行的真实 QUIC/TLS/H3 管理员验收。

```sh
cargo test --offline --lib -- --test-threads=1
cargo test --offline --lib server::tests::network::system::device_admin_registration_and_authorization_over_h3 -- --ignored --exact
```

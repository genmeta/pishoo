# Pishoo App / Lib 软件包规范

日期：2026-10-14。状态：本仓宿主、打包器与 Workspace/Message 已按本规范实现；独立商店服务适配及线上部署须另行完成。本文是包元数据、依赖、文件布局和安装行为的当前规范；替代此前把这些信息放入 OpenAPI、为静态文件生成 OpenAPI 操作及迁移旧测试包的约定。WEBC 容器版本仍为 v3，包级签名算法不变，正式构建入口为 `genmeta package build [PATH] [-o FILE.webc] [--id PUBLISHER]`，执行本地 `[build]` 配置后签名；已有产物使用 `genmeta package build --no-build`。打包实现归 gmutils，宿主发行包不再附带打包命令；宿主兼容性与包签名校验调用运行中的 Pishoo 校验 API。

## 1. 职责和包类型

借鉴 Debian/RPM，将软件包分成包元数据和实际内容。App、Lib 使用相同的 WEBC v3 容器和包级签名，各自发布、版本管理和安装。

| 项目 | App | Lib |
| --- | --- | --- |
| 用途 | 浏览器前端 | WASI HTTP 后端 |
| 制品名 | `app.webc` | `lib.webc` |
| atoms | 零个 | 恰好一个 `backend`，必须是 Component，不能是 core module |
| 必需文件 | `/public/index.html` | `/metadata/openapi.json` |
| 文件映射 | 恰好一个本包 `/public` 映射 | 恰好一个本包 `/metadata` 映射 |
| OpenAPI | 不使用，不生成静态文件操作 | 只描述后端业务接口及 API 访问声明 |
| 安装后访问 | `/std/<name>/` | `/std/<业务路径>` |
| 运行依赖 | 零个或多个 Lib | 零个或多个其他 Lib |

App 不包含后端或 `/metadata/openapi.json`；Lib 不包含 public 资源或 static-file 操作。WEBC manifest 的 atoms 声明必须与实际内容一致。禁止 `use` 依赖、commands、WEBC 命令 entrypoint 和 bindings；Pishoo 运行依赖不参与 WEBC 模块链接。一个 Component 仍是一个 Lib，不新增 App 运行时结构。

## 2. 唯一包清单

发布包内的 WEBC manifest 是包元数据的唯一权威。开发时可由 `wasmer.toml` 等构建输入生成，但不增加另一份具有同等权威的 `manifest.json`，也不从 OpenAPI 补齐缺失包字段。

沿用标准 `package.wapm` 和 `package.fs` 注解，新增具体用途的 `package.pishoo` 注解。下列 JSON 只是 WEBC manifest 注解的可读表示，实际仍以标准 WEBC manifest CBOR 编码保存，不是新的文件格式。

| 位置 | 字段 | 要求 |
| --- | --- | --- |
| `package.wapm` | `name` | 必填，完整 `<publisher>/<name>`；不再另存 publisher 和短名副本 |
| `package.wapm` | `version` | 必填，固定 `MAJOR.MINOR.PATCH` |
| `package.wapm` | `description`、`license` | 必填，非空包说明和许可证标识 |
| `package.wapm` | `repository` | 可选，非空 HTTPS URL，须有主机且不能包含用户名密码；原字符串参与签名 |
| `package.pishoo` | `kind` | 必填，`app` 或 `lib`，必须与实际内容和上传集合一致 |
| `package.pishoo` | `title` | 必填，非空展示名称，不承担包身份 |
| `package.pishoo` | `dependencies` | 两种包均必填，数组；无依赖显式写 `[]` |
| `package.pishoo` | `changelog` | 必填，非空本版本变化和数据兼容性说明 |
| `package.pishoo` | `entry` | 仅 App 必填，固定 `/`；Lib 禁止声明 |
| `package.pishoo` | `host` | 仅 Lib 必填，能力用途对象；无能力写 `{}`；App 禁止声明 |
| `package.fs` | 文件映射 | 按包类型映射本包卷，不允许 `from` 指向外部包 |

`[package].repository` 原样保留在签名 manifest 的 `package.wapm.repository`，由宿主 `package_metadata` 派生可选 `repository` 展示字段。省略时不生成空字符串或 null。链接只用于展示，不联网核验、下载或执行；不在 `package.pishoo` 存副本，也不从 OpenAPI 或商店未签名外链推测。

`package.pishoo` 只接受表内字段，不保留扩展袋或重复身份字段。`host` 沿用已支持的 `private-storage`、`request-identity`、`signatures` 非空用途及真实 imports 核对规则；不产生授权或新增宿主能力。

App 的包注解示例：

```json
{
  "package": {
    "wapm": {
      "name": "alice.smith.dhttp.net/journal",
      "version": "1.0.0",
      "description": "手记前端",
      "license": "MIT"
    },
    "pishoo": {
      "kind": "app",
      "title": "手记",
      "entry": "/",
      "changelog": "首次发布前端。",
      "dependencies": [
        {"publisher": "alice.smith.dhttp.net", "name": "journal", "version": "1.0.0"}
      ]
    },
    "fs": [{"volume_name": "public", "mount_path": "/public"}]
  }
}
```

Lib 使用同样的 `wapm`；`pishoo.kind="lib"`，以 `host` 替换 `entry`，声明自己的 dependencies，fs 改为 `/metadata`，并加入实际 `backend` atom 声明及其标准摘要。上例省略 atoms 等容器字段，不能作为完整可发布包。

## 3. 身份、版本和签名

publisher 是规范化完整 DHTTP 名称；短名以小写字母开头，只含小写字母、数字和连字符，最长63字节。版本三段均无前导零，不接受 prerelease、build metadata、版本范围或 `latest`。包身份为 `(kind, publisher, name)`；同一包版本不可覆盖，App/Lib 可以同名同版本。

上传 URL、manifest 包身份、实际签名者及已认证握手身份必须一致。publisher 首次成功发布时绑定 owner_hash；证书轮换不能更换包归属。宿主安装仍验证固定商店身份、可信发布证明、包签名、大小和摘要。

包级签名继续覆盖原始 manifest、atoms 和除签名卷外的全部内容卷；依赖或元数据变化也必须递增自身版本并重新签名。签名卷、摘要算法和身份校验详见 [WEBC 商店协议](../../../store-server/docs/webc-protocol.md#包级签名)。不改变既有签名信封版本或签名域。

## 4. 依赖

`package.pishoo.dependencies` 最多100项，每项只允许 `{publisher,name,version}`，均为必填字符串。publisher、name、version 遵循上一节规范；同一完整 Lib ID 不重复，不支持版本范围、备选依赖、可选依赖或虚拟包。App 和 Lib 使用完全相同的格式；直接自依赖拒绝。

这里只声明运行依赖。Rust/Cargo、前端 npm/Bun 等构建依赖留在构建系统，不进入安装依赖。

商店在新发布版本的登记事务中核对每项依赖是已发布的精确 Lib 版本；同名 App 不能满足依赖，缺失或下架拒绝发布。依赖后来下架不改写历史版本或联动下架使用方；相同内容的历史重试保持既有幂等结果。

宿主递归解析整棵依赖图，拒绝循环、同一完整 ID 的版本冲突及本机同类短名冲突。已安装且匹配的精确版本复用；缺失依赖由商店安装补装，不静默覆盖已有不同版本。本地文件安装不联网补装，要求依赖已安装并满足当前加载要求。

依赖声明不授予调用权限，也不是 WASM 静态链接关系。本机 Lib 调用沿用标准 WASI HTTP 和既有 daccess；不新增传输接口。

## 5. OpenAPI 和前端资源

Lib 的唯一业务 API 描述是映射后的 `/metadata/openapi.json`，必须是 OpenAPI 3.1。`info.version` 必须与包版本一致，但不作为包身份来源；`info.title/description/license` 是 API 文档信息，商店展示包信息时从 manifest 读取。

OpenAPI 中的 `x-pishoo` 仅保留已有 API `access-groups`、操作 `access-group` 及 `initial-access` 建议。不再允许 `kind`、`publisher`、`name`、`dependencies`、`entry`、`host`、`changelog` 或 `static-file`。后端任何嵌套层都不得嵌入另一份 `pishoo:openapi` 或 `pishoo:signature`。

业务路径仍为固定组件内部路径，不能包含宿主挂载前缀、query、路径模板或系统保留路径；每个操作有唯一 operationId 和非空 summary。访问组成员须与实际操作一致且不交叠。所有授权建议仅供审阅，不自动写入 daccess。

App 从实际 `/public` 文件树派生静态路由：`index.html` 对应 `/`，其他文件对应相对路径；每个文件原生支持 GET/HEAD，无需生成 OpenAPI、operationId 或 static-file 注解。不存在的文件404，存在文件的其他方法405，HEAD 不发送正文。无隐式目录索引或 SPA fallback；SPA 使用 hash 路由。

静态路径拒绝空段、`.`、`..`、百分号、反斜杠、空白及路径歧义；禁止符号链接和重复条目。前端资源使用相对 URL，业务请求使用声明依赖对应的 `/std/<业务路径>`。public 不表示匿名公开，所有前端文件和业务请求都经过现有 daccess。

## 6. 安装、更新、卸载和数据

安装入口继续按 `kind` 区分 app/lib；省略 kind 的既有原生请求仍按 lib 处理。在线安装/更新/卸载即时生效，在途请求保留既有资源；离线 CLI 修改在下次加载生效。

本机管理 API 仅使用短名：`GET/PUT/DELETE /std/sys/package/libs/<name>` 和 `/std/sys/package/apps/<name>`。GET/DELETE 从同类安装登记解析完整 ID，PUT 从已验签包取得 publisher/name 并核对短名与 URL；不接受带发布者的旧管理路径。响应仍返回完整包 ID，更新继续核对发布者 owner_hash。商店路径和精确依赖继续使用完整包身份。

WEBC 是发布、下载及安装格式，安装后展开保存，不再保存完整 WEBC 文件。普通制品位于 `apps/<name>/`、`libs/<name>/`，已验签发布者为 `std.dhttp.net` 的制品位于 `apps/std/<name>/`、`libs/std/<name>/`；不创建发布者目录。`std` 为制品目录保留名，App、Lib 各自按短名唯一，同类跨发布者同名不能共存；两类名称空间独立。

```text
<身份>/
├── libs/
│   ├── std/
│   │   ├── message/
│   │   │   ├── message.wasm
│   │   │   ├── openapi.json
│   │   │   ├── manifest.json
│   │   │   ├── signature.json
│   │   │   └── .webc-proof
│   │   └── profile/ ...
│   └── <其他Lib>/ ...
├── apps/
│   ├── std/
│   │   └── workspace/
│   │       ├── manifest.json
│   │       ├── signature.json
│   │       ├── .webc-proof
│   │       └── public/
│   │           ├── index.html
│   │           └── assets/
│   └── <其他App>/ ...
└── ssl/
    ├── fullchain.crt
    ├── privkey.pem
    └── stapling.ocsp
```

完整 publisher/name 仍保存在已签 manifest 和 config.db 登记，用于身份、依赖与能力校验；后端私有数据仍为 `db/<publisher>/<name>/`。标准目录不增加授权或执行特权；只按已签发布者分类，安装来源与短名不会使其他发布者进入 std。

现有包签名绑定原始 WEBC 段，不能只对展开后的 JSON 重新序列化验签。`.webc-proof` 保存原始头部、文件位置和非文件字节，WASM/资源/签名 payload 只存于各自展开文件；加载时有界还原原包，复用原验签并核对规范文件位置，拒绝文件缺失、篡改和符号链接。`manifest.json` 是原签名 manifest 的可读投影，加载核对一致性；文件摘要仍位于签名覆盖的原始头部中，不增加另一份独立权威。其他 Lib 元数据放入 `metadata/`；签名覆盖但未映射的原始卷文件保存在隐藏 `.volumes/`，不对外提供。

更新先在同一文件系统准备完整展开目录，经现有身份/制品文件锁和 SQLite 事务整体发布；提交失败恢复旧目录。运行资源仍使用已验签的内存快照，在途请求保留旧版本。已登记的旧 WEBC 制品可读兼容；显式重装会写入新展开路径并核对原发布者 owner_hash，旧制品与数据保留。启动不自动迁移或删除。

安装事实以现有 `config.db` 登记为准，登记依赖必须与已验签包清单一致。未登记文件忽略，已登记文件缺失或不一致时报错，不扫描导入。更新不得破坏已登记 App/Lib 的依赖约束；`/std` 全部保留且不回退代理；旧 `/api`、`/app` 正常代理，无兼容别名。

卸载 App 只移除前端制品、登记及其依赖关联，保留 Lib。任何仍被已安装 App/Lib 依赖的 Lib 不允许单独卸载或替换为不满足依赖的版本。显式卸载无引用 Lib 也保留私有数据，安装/更新/卸载均不自动改写 daccess。

不新增级联卸载、autoremove、purge、手动/自动安装标记或引用计数。依赖关系从已登记事实查询，不维护另一套可矛盾的计数。单个制品原子提交，不承诺跨制品事务；上层包保存失败时已安装的有效依赖保留。

## 7. 商店响应、限额和兼容边界

上传、下载和安装路径保持现有 apps/libs API。目录、版本详情和 updates 的 `kind/dependencies/title/description/changelog/host` 从已验签 manifest 派生；App 不返回伪造或空壳 OpenAPI，Lib 详情才包含真实 OpenAPI。商店索引可缓存已验证包元数据，但不能成为第二套开发者声明或覆盖签名内容。

继续使用完整包64 MiB、manifest/index 各1 MiB、Lib OpenAPI 1 MiB、单文件16 MiB、4096项、目录深度32、路径1024 UTF-8字节、16卷和既有签名/请求上限，详见商店协议。

按用户要求不迁移旧测试包：新格式不从旧 OpenAPI 元数据回退，不接受旧合包，不自动拆包、改签或重写数据库；开发者从源码重建发布。需要清理测试发布时另行执行明确的数据操作，本规范修订不删除线上或本地数据。

本仓实施经用户逐项批准的接口差异见 design 清单：统一 manifest 读取、CLI 复用整包验签、前端展示字段以及独立 Message 数据归属和能力登记。宿主安装关系仍复用现有 config.db 表，不新增状态容器或安装账本。独立商店服务适配与发布部署不由本次源码变更代替。

## 参考

- [Debian 包控制字段](https://www.debian.org/doc/debian-policy/ch-controlfields.html#debian-binary-package-control-files-debian-control)：包身份和 Depends 等关系属于包元数据。
- [RPM 依赖字段](https://rpm.org/docs/4.20.x/manual/spec.html#dependencies)：区分 Requires 与 BuildRequires。
- [APT 手册](https://manpages.debian.org/bookworm/apt/apt-get.8.en.html)：remove、purge 和 autoremove 是不同操作；本阶段仅保留已有安装/卸载范围。

## 标准服务与能力登记

`/std` 是协议命名空间，ACL、Contact、Package 和现有宿主基础设施原生实现，不作为 WASM 能力。Lib 声明 `/message`、`/profile`、`/workspace/context` 等业务路径，宿主仅加 `/std`；guest 收到去掉该前缀的业务路径与原 query。App 挂载 `/std/<name>/`，同名 App/Lib 可共存，按实际文件 GET/HEAD 和声明操作分派。不同 Lib 不共享路径，原生路径和静态操作冲突拒绝安装、更新；不存在路径404，未声明方法405。

访问组来自验签 OpenAPI 的 access-groups/access-group，与包版本一起登记 config.db 的 lib_capabilities/lib_capability_operations；无组写零行。GET `/std/acl/capabilities` 返回 `<完整Lib ID>#<组>`、title、所属包版本和实际端点，经 daccess 授权，不能代表联系人的授权或激活状态。普通安装事务更新能力并恢复失败发布；启动按登记装配已安装资源，不重复安装校验，也不扫描或隐式导入旧安装。卸载级联删除能力并保留数据和 ACL，换提供者前须明确处理旧路径规则。路径及服务归属见 [HTTP 路径](system-paths.md)。

# genmeta upload 实现设计与 Agent 任务书

日期：2026-10-14。状态：待实现。配套任务：[pack](genmeta-pack-design.md)、[install](genmeta-install-design.md)。本文件描述单包发布 CLI，不表示当前独立商店已适配新格式或已部署。

## 1. 目标和边界

`genmeta upload` 将一个本地签名 WEBC App 或 Lib 发布到固定商店。名称、类型、版本、依赖、源码仓库链接来自已签 manifest；不要求用户重复输入，不允许上传参数改写。

- 一次一个本地普通文件，不接受目录、GitHub URL、远程包或通配批量发布。
- 不打包、不修改签名文件、不自动重签、不自动发布依赖、不安装。
- 不新增作业、轮询状态 API、发布队列、信任列表或安装账本。
- 未知上传结果通过现有精确版本只读 API 核实；不恢复 HTTP 202/status。

实施前阅读本仓 `AGENTS.md`、`design/README.md`、`design/pishoo-interfaces.md`、[包规范](package-spec.md)，以及 `store-server/AGENTS.md`、`CONTRACT.md`、`docs/store-server-proposal.md`、`docs/webc-protocol.md`。遵守各仓冻结结构与签名约束，不修改底层传输接口。

## 2. 命令契约

```sh
genmeta upload FILE.webc [-i PUBLISHER | --id PUBLISHER]

genmeta upload ./dist/chat-0.2.0-lib.webc
genmeta upload ./dist/workspace-0.2.0-app.webc --id dev.dhttp.net
```

FILE 必填；没有 kind/name/version/store/source/force 选项。帮助与版本行为沿用 genmeta。`--id` 是实际签名发布身份的核对项，不是商店名称，也不是最终安装用户身份。

省略 id 时从 manifest publisher 选择本机凭据，不能使用无关的默认身份或匿名身份。显式 id 经正常名称解析后必须与包 publisher 一致。home 沿用 `DHTTP_HOME` 和用户 home，不增设新参数。

## 3. 本地验证与真实发布身份

1. 有界读取普通文件，完整包不超过既有64 MiB限制。保留单次读取的字节快照。
2. 解析 manifest 得到 kind 和完整 publisher/name/version，只用于选择已有校验路径，不视作已经可信。
3. 对该字节快照使用 `pishoo app check` 或 `pishoo lib check` 的现有整包校验。若调用外部命令，写入私有暂存文件并校验该文件；发送仍使用同一份已验证字节，不能重新读取可能已变化的原路径。捕获 check JSON，不泄漏到最终 stdout。
4. 校验结果与解析的身份/类型/版本一致；取得包中签名证书链与内容摘要。不要实现第二套包验签器。
5. 加载 publisher 的真实 DHTTP Endpoint。包签名证书链须与实际出站 LocalAuthority 证书链逐字节一致，名称及 owner_hash 也匹配。否则在上传前失败，提示用当前凭据重新 pack。

读取 webc manifest、签名信封及原始 section 来取得字段/已有摘要是必要的解析，不等于另建校验实现；无效包只能通过宿主 check 失败处理。密钥仅经现有身份库使用，不读取进日志或打进包。

验证需要包含 App 零 atom/仅 public、Lib 唯一 backend/仅 metadata、manifest唯一权威、精确依赖和 repository 格式；与 [pack 设计](genmeta-pack-design.md)保持一致。

## 4. 商店连接和请求

固定目标：`store.dhttp.net`。既有 owner_hash：

```text
ffc3e50e1dfb7baca1cc07a7cbfcf8cb34a9bf5a9c25024fcf9a9786bb230746
```

来源是已冻结信任锚与独立商店 DEPLOYMENT.md；变更必须另行取得用户授权。连接使用既有 DHTTP DNS/Network 初始化与真实 Endpoint。请求发送前通过 `expect_remote_owner_hash` 核验目标，响应再核验 RemoteAuthority 名称和 owner_hash。禁止跟随重定向或将包内容发往任意 URL。

```text
PUT https://store.dhttp.net/store/v1/apps/<publisher>/<name>/versions/<version>
PUT https://store.dhttp.net/store/v1/libs/<publisher>/<name>/versions/<version>
Content-Type: application/webc
Accept: application/json
```

URL 完全由已验证字段和固定集合构造；使用 `http::Uri`、`HeaderValue` 等现有验证类型。使用现有 Body/写入接口并正确结束请求 body，不新增 Body 包装器或传输额度。以一个有界请求完成发布，CLI 总等待须覆盖商店120秒输入及响应处理；建议单次上传总期限180秒，元数据读取30秒，总输入输出均有现有限额。各网络操作超时不能重置无限循环。

## 5. 发布语义、失败及未知结果

商店负责发布者权限、owner_hash归属、包签名、编译兼容性、版本不可变及依赖可用性。客户端不把可选的预查询当提交授权或原子保证；第一版直接上传即可。

| 结果 | CLI 行为 |
| --- | --- |
| 首次发布成功 | 核验响应身份、类型、版本、状态、大小和完整包摘要，输出成功 |
| 同版本同 payload 已存在 | 现有幂等成功；保存的完整包/signature可能与此次重签文件不同，核验商店保存包的 payload 后输出 `already-published` |
| 同版本不同 payload | 冲突；提示提升版本，不覆盖、不自动修改版本 |
| 依赖缺失或下架 | 显示具体完整 Lib ID/精确版本；提示先发布依赖 |
| 认证、审批或授权拒绝 | 展示当前 daccess/商店实际响应，不生成后台等待器或自创轮询 API |
| 同内容重试但既有版本下架 | 显示仍下架；不能宣称已重新发布或可安装 |
| 网络中断/响应丢失/5xx导致结果不明 | 执行下述有界只读核实，不盲目重复 PUT |

未知结果核实：GET精确版本；若可信响应证明已发布且 package_sha256与上传文件相同，即可确认保存；若完整包摘要不同但可能是同内容重签，下载可信响应中约定的固定下载路径，验证保存包并比较既有协议的 payload_sha256。只有匹配且 status=published 才能确认。不存在、下架、内容不同或查询也失败时输出明确结果；无法证明则显示“发布结果未知，可稍后重新执行同一命令”，非零退出。

协议不保证元数据响应直接包含 payload_sha256，不能假设有该字段。客户端不得用仅版本号相同作为成功证据。手动重试继续服从服务端同版本同内容幂等规则，旧 signer、包和发布时间由服务端保留。

## 6. 依赖发布顺序

Workspace 按以下顺序发布：

```sh
genmeta upload ./dist/chat-lib.webc
genmeta upload ./dist/profile-lib.webc
genmeta upload ./dist/workspace-app.webc
```

每次只发布一个包。上游失败时已成功发布的依赖保留；没有跨包事务或级联回滚。App 和 Lib 可以同 publisher/name/version，各自位于独立集合。相同名字的 App 不能满足 Lib 依赖。

## 7. repository与商店兼容前置条件

repository来自 `package.wapm.repository`，仅为可选展示信息；命令不访问该 URL。商店目录和版本详情从已验签 manifest 派生可选 `repository` 字段，由 install agent 在前端展示。没有链接的旧有效条目仍可展示，不补造链接。

截至设计日，独立商店源码仍按旧 OpenAPI 包元数据工作；CONTRACT明确指出 `versions.openapi_json` 无法表达无OpenAPI App，并要求持久字段变化单独批准。线上也不能假设已经完成适配。

本任务允许完成上传客户端及新协议测试素材，但不隐含批准商店 SQL/schema/Registry/跨模块接口变更。upload agent 先列出商店实际差异、需要的具体列名/约束/读取写入修改及已批准上游依赖同步来源，提交用户批准后才能实施。不能往 openapi_json 塞伪造OpenAPI、通用扩展袋，或回退旧合包协议。也不能擅自改商店 vendor运行时来源。

如果其他工作已完成商店适配，核验具体代码和契约再联调；没有适配时 CLI应准确显示真实拒绝，不宣称端到端完成。部署、真实上传、删除/迁移线上测试包均不属于此任务授权。

## 8. 实现归属与输出

本 agent 唯一拥有 gmutils `genmeta/src/upload.rs` 及上传专属测试；实现约定：

```rust
pub(super) async fn run(file: std::path::PathBuf, id: Option<String>)
    -> Result<(), snafu::Whatever>;
```

CLI只存参数，不新增运行状态结构、Guard或公共库API。必要helper保持module-local、stateless；无需建立跨工具共享网络Context。共享 launcher、Cargo、Cargo.lock、README由pack agent统一编辑，所需依赖和调用点提前通知pack agent。商店默认只读评估；得到具体额外授权后的商店修改由upload agent独占，其他agent不碰。

stdout成功输出JSON，包含 `result: published|already-published`、`id,kind,version,status,package_sha256,detail_url`。未知结果非零退出并在stderr明确说明；不要把服务端出错时任意JSON当成功。成功响应/核实均要检查固定下载路径、响应身份、大小/摘要及现有JSON限额。

## 9. 验收与交付

- App与Lib各完成临时身份/临时兼容商店的真实发布路径验证，包及URL身份完全一致。
- 缺失签名、篡改包、错误publisher、证书链不匹配、错误商店owner均在适当阶段拒绝；传输前身份拒绝不能泄露包内容。
- 缺失/下架精确依赖拒绝；先发布依赖后父包成功；不自动发布其他包。
- 同内容重试、同内容重签、不同内容409、下架重试不恢复均有验证。
- 响应丢失后只读核实能区分已保存、未确认及不同内容；不盲目重发，不只比较version。
- 文件变化竞态仍只发送已校验快照；超限、重定向、非法响应、trailer或截止时间失败正确处理。
- repository保留并可从可信商店详情读取；本地CLI不联网获取源码。
- 发布CLI本身不需要运行本机Pishoo服务，但依赖已安装宿主check工具；缺工具时明确报错。

交付包含上传模块、协议测试、所需依赖列表、商店兼容/成员差异报告及上线前未完成项。真实外部发布必须由用户另行明确指定，不能用生产商店试发占用命名空间。

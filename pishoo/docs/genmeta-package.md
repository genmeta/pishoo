# genmeta 软件包命令

本文说明当前 gmutils 的命令行为。职责、HTTP 接口、数据库和安装生命周期见[包管理参考](package-management.md)；WEBC 清单、签名和内容要求见[软件包规范](package-spec.md)。

本文“源包”指原始已签 WEBC 发布文件，不指源码工程。“作者”指已验签发布者的规范化 DHTTP 身份。发布和传输使用已签名的 WEBC 源包；安装后的 WASM、静态文件和数据库登记属于运行资源，不是可直接重新发布的原包。

## 命令与参数

```text
genmeta build [PATH] [-o FILE.webc] [-i AUTHOR] [--no-build]
genmeta publish FILE.webc [-i AUTHOR]
genmeta install TARGET [-i IDENTITY] [--app | --lib] [--version VERSION]
genmeta update TARGET [-i IDENTITY] [--app | --lib] [--version VERSION]
genmeta uninstall TARGET [-i IDENTITY] [--app | --lib]
genmeta list [-i IDENTITY] [--app | --lib]
genmeta info TARGET [-i IDENTITY] [--app | --lib]
```

`-i` 等同于 `--id`，`-o` 等同于 `--output`。`--app`、`--lib` 互斥；install、update、uninstall、info 默认 App，list 默认同时列出 App 和 Lib。build、publish 的类型来自源包清单，不接受类型开关。

`VERSION` 必须是没有前导零的三个非负整数 `MAJOR.MINOR.PATCH`，各段不超过 u64；不支持预发布或构建后缀。版本选择参数只用于 install、update；build 的包版本来自清单。

旧 `genmeta package …`、独立 `check` 命令、`list --loaded` 及安装参数 `--file` 已删除。安装和更新只选择商店发布版本；本地 WEBC 文件仅用于 build/publish 流程。

## 名称与身份

| 输入 | 当前解析方式 |
|---|---|
| `genmeta install workspace` | 从商店选择 `std.dhttp.net/workspace` 的 App |
| `genmeta install alice/calendar` | 将作者简写规范化为 `alice.dhttp.net` |
| `genmeta update calendar` | 从所选宿主身份的安装登记取得原作者，再查询其商店版本 |
| `genmeta info message --lib` | 按已安装 Lib 短名读取数据库详情 |
| `genmeta uninstall alice/calendar` | 先核对登记完整身份，再删除 |

推荐使用 `作者/名称`，不用 `@` 前缀。名称必须以小写字母开头，其余为小写字母、数字或连字符，最多 63 字符。同一身份内同类名称唯一，App 和 Lib 各自独立。TARGET 不接受 URL、本地路径、WEBC 文件路径或 `name@version`。

`--id` 在不同操作中的含义如下：

| 操作 | 指定及省略 `--id` 的行为 |
|---|---|
| build | 断言它与清单作者相同；省略时直接使用清单作者及其本地签名凭据 |
| publish | 断言它与源包作者相同；省略时直接使用源包作者的本地凭据 |
| install / update / uninstall / list / info | 选择承载软件包的宿主身份；省略时使用当前账户配置的默认身份 |

管理命令从当前账户的 DHTTP home 加载凭据。默认身份可用 `genmeta identity default NAME` 设置；它与软件包作者不是同一概念。目标身份必须已由 Pishoo 加载且服务可达，CLI 不自动安装、启动或启用 Pishoo。

## 构建与签名

`PATH` 默认当前目录，可指项目目录或本地 TOML 清单。通常使用项目中的 `wasmer.toml`：

```toml
[build]
command = ["sh", "build.sh"]
manifest = "dist/wasmer.toml"
```

每次普通 build 都先执行 `command`，工作目录为输入清单所在目录。它是参数数组；只有显式选择 `sh` 等程序时才运行 shell。失败立即停止，不生成签名包。可选 `manifest` 指构建产出的相对清单路径，解析后必须仍在项目内；省略时使用原清单。`[build]` 不进入已签元数据。

```sh
# 假设项目清单的作者为 alice.dhttp.net
genmeta build ./my-app -o ./calendar.webc --id alice
genmeta build --no-build ./my-app/dist/wasmer.toml -o ./calendar-repacked.webc
```

`--no-build` 跳过源码构建，仍按清单打包并签名；输入不是现成 WEBC。省略 `-o` 时，输出为最终清单所在目录的 `dist/<name>-<version>-<kind>.webc`；显式相对输出路径相对于命令当前目录。已有输出文件不会被覆盖。

签名使用清单作者的本地证书和私钥，不刷新 OCSP，不连接 Pishoo，也不自动发布。源码构建脚本本身仍可按项目需求访问网络。Lib 的 OpenAPI 在打包签名前嵌入 WASM。构建日志写 stderr，成功结果 JSON 写 stdout。

## 发布到商店

```sh
genmeta publish ./calendar.webc --id alice
```

publish 读取一份有大小上限的源包字节，在本地检查容器及签名封装，并使用作者身份连接固定商店。源包的签名证书链必须与该作者当前本地凭据一致；凭据变化后，旧文件可能需要重新 build。`--id` 不会改写包作者或改签文件。此操作不依赖运行中的 Pishoo。

商店负责验证发布输入；CLI 核对返回的完整身份、类型、版本、作者 owner hash、摘要及大小。已有版本不会被覆盖：相同发布内容可确认已发布，不同内容须递增版本；已撤下版本不会因重传恢复。精确依赖应先发布到商店。

上传失败或收到服务端 5xx 时，CLI 会对同一精确版本进行一次只读核实，不会自动重传 PUT。核实失败意味着发布结果未知，应确认商店记录后再显式重试。源文件完成读取后，后续校验和上传使用同一份内存字节，不重新按路径打开文件。

## 商店安装、更新和卸载

```sh
genmeta install workspace --id sky.lee
genmeta install alice/calendar --version 0.1.0 --id sky.lee
genmeta install alice/message --lib --version 0.1.0 --id sky.lee
genmeta update calendar --version 0.2.0 --id sky.lee
genmeta list --id sky.lee
genmeta info message --lib --id sky.lee
genmeta uninstall calendar --id sky.lee
```

商店 install、update 先读取目标身份的安装登记，再解析商店发布版本。省略 `--version` 时选择最新已发布版本；显式版本固定本次目标，不承诺只能升级到更高版本。

- install 和 update 遇到相同完整身份和版本均返回 `unchanged`，不提交安装请求；install 遇到其他已装版本则要求使用 update。此结果仅表示 ID、版本相同，不代表重新校验了本地内容，也不执行同版本修复。
- update 要求已安装，并沿用原作者。不能据此更新没有作者的旧短名登记。
- 同类短名已被另一作者占用时拒绝，须先显式卸载。宿主仍独立核对本次下载输入的签名、商店发布证明、精确依赖及路由约束，不保存作者密钥指纹。
- 商店安装由宿主递归安装精确 Lib 依赖。每个包单独提交；父包失败时，已成功安装的依赖保留。
- uninstall 先 GET 详情并核对 `Package-Id`，再用 `Expected-Package-Id` 提交删除。被其他 App/Lib 依赖的 Lib 不能删除，错误会列出依赖者。

卸载保留业务数据、授权规则和依赖包，不自动清理未使用依赖；卸载后不会自动恢复。list、info、uninstall 不查询商店，也不扫描本地制品文件，但仍须通过网络访问所选身份的 Pishoo。清单和详情反映数据库登记，不能据此断言运行文件存在或服务已经加载。

## 安装请求协议

install 和 update 使用相同的宿主写入协议。CLI 已确定精确商店版本后，发送：

```http
PUT /std/sys/lib?name=message
Content-Type: application/json
Accept: application/json

{"publisher":"alice.dhttp.net","version":"0.1.0"}
```

App 使用 `/std/sys/app?name=NAME`。URI 中只有所选类型与短名；JSON 恰好包含规范化完整 `publisher` 和精确 `version`，不再包含 `name`、`kind`、源包字节或下载 URL。由宿主连接固定商店，取得、验证并安装源包及其精确 Lib 依赖。

CLI 不向宿主上传本地 WEBC，也不接受本地安装路径、文件或任意下载地址。`genmeta build` 和 `genmeta publish FILE.webc` 继续处理本地构建/发布制品，这不构成本地安装入口。list/info 的 GET 和 uninstall 的 DELETE 语义保持独立，详见[包管理参考](package-management.md)。

## 并发、失败及结果确认

CLI 预检和实际安装是不同请求，存在检查与写入之间的时间窗口（TOCTOU）。安装接口执行单包 upsert，不提供“仍未安装”或“仍为某版本”的比较交换；卸载前置条件也仅锁定完整作者/名称，不锁定版本或内容。

管理请求会核对所选服务的真实 DHTTP 身份与 owner hash；商店连接另核对固定商店身份。发现身份不符、重定向、异常状态或不一致的响应时停止。成功结果写 stdout JSON，进度及诊断写 stderr。

商店安装等待上限为 15 分钟；中断、超时或响应丢失后，服务端仍可能完成写入。HTTP 202 代表审批响应，不代表安装成功；404/503 应检查所选身份、服务启用、路由及访问权限。出现“结果未知”时先查询 list/info 和宿主日志，再显式重试；没有自动重试、强制卸载或跨包事务。

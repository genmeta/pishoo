# Codex 控制 AnySee 的测试经验与崩溃排查

整理日期：2026-10-15。适用于 macOS、AnySee、Node.js 与 Playwright 的网页测试。命令示例已按当前 `std/apps/workspace` 目录调整；文中的测试结果保留原验证日期。

我们已经通过 Codex 执行脚本、使用 Playwright 经 CDP 控制 AnySee，完成介绍页测试、真实身份访问、截图和新标签页验证。本机没有复现“一连接就崩溃”。同事遇到的退出需要结合其具体二进制、启动参数和日志定位，不能只凭窗口消失或 `Target closed` 判定原因。

实际控制链路为：

```text
Codex 执行本地脚本
  → Playwright chromium.connectOverCDP()
  → AnySee 的本机调试端口
  → 测试标签页
```

这里使用 AnySee 自己的 Chromium 内核和 DHTTP 能力；普通 Chrome 的测试结果不能替代 AnySee 的身份访问验证。

## 1. 已验证的环境与结果

| 项目 | 实测信息 |
| --- | --- |
| 平台 | macOS |
| Node.js | `v22.19.0` |
| Playwright | `1.63.0`，当前依赖位于 `std/apps/workspace` |
| 2026-10-15 测试程序 | `/Applications/AnySee.app/Contents/MacOS/AnySee` |
| CDP 返回的 product | `Chrome/138.0.7204.306`；这是 AnySee 返回的协议标识 |
| CDP 返回的 revision | `@6f7f3f6432ce263e6d6ae0b3152a43d5e4290dbd` |
| 调试连接 | `127.0.0.1` 上的动态端口 |
| 测试配置 | 独立临时 `--user-data-dir` |

2026-10-15 的最小复现使用本机临时 HTTP 页面，没有启动 Pishoo，也没有访问真实账号 API：

- 在 `1440×900`、`768×1024`、`375×812` 三种尺寸下完成输入、点击、打开新标签页和截图。
- 关闭脚本创建的 context 后，默认 context 中用于核验的页面仍存在。
- 断开 CDP 后，`/json/version` 仍可访问；重新连接三轮，原页面仍存在。
- 没有收到测试页面的 `crash` 事件。最后由脚本明确发出 `Browser.close`，正常结束自己启动的临时浏览器。

此前的介绍页验证也覆盖过真实 Spike → Alice 身份访问、触屏与减少动态效果设置，以及三种尺寸下的新标签页跳转。卡片改版阶段的介绍页 E2E 记录为 `27/27`；后续新标签页行为另做了三种尺寸验证。历史用例通过不等于所有 AnySee 版本都不会崩溃。

同样的版本字符串可能对应不同构建。排查时同时记录可执行文件路径、CDP revision 和 Playwright 版本，不只记录“Chrome 138”。

本文验证的是 Node.js + Playwright 直接连接 CDP。若同事使用浏览器 MCP、插件或其他控制工具，还需要记录该工具的版本、启动参数和退出清理方式；不能把一种接入路径的结果直接推广到所有 Codex 浏览器工具。

## 2. 先选择测试实例

| 目的 | 使用方式 | 清理范围 |
| --- | --- | --- |
| 布局、交互、截图、模拟 API | 启动独立 AnySee，使用全新的临时配置 | 关闭自己创建的页面、context；最后可退出这个临时实例 |
| 验证日常 AnySee 的登录或原生 DHTTP 身份 | 连接已有用户实例，在其默认 context 中新建测试标签页 | 只关闭自己新建的标签页、移除自己的拦截，再断开 CDP |

复用用户实例时，`browser.contexts()[0]` 通常是已有默认 context，不能把它当作脚本创建的测试 context 来统一清理。也不要遍历所有页面并逐一关闭。

`browser.newContext()` 可以隔离 Cookie、localStorage 和路由拦截，适合模拟测试；它不会继承已有 context 的网页登录状态。DHTTP 证书选择还涉及 AnySee 的原生实现，应以实际身份接口响应为准。

两种实例都通过 `connectOverCDP()` 接入。不要因为 Playwright 提示缺少内置 Chromium，就先下载普通 Chrome 来替代 AnySee；本项目已有 `@playwright/test`，可以直接连接 AnySee。

## 3. 建议先跑的最小复现

从独立临时配置开始。不要用同一个用户数据目录同时启动两个 AnySee；已有实例可能接收启动请求，使第二个进程退出，或产生目录锁冲突。新命令里的调试参数也可能没有应用到已有进程。

以下命令在仓库根目录的同一个终端中执行。`ANYSEE_BIN` 应改为同事机器上实际要测试的程序；不要使用本例之外的个人构建路径。

```sh
export ANYSEE_BIN='/Applications/AnySee.app/Contents/MacOS/AnySee'
export ANYSEE_TEST_RUN="$(mktemp -d "${TMPDIR:-/tmp}/anysee-cdp.XXXXXX")"

"$ANYSEE_BIN" \
  --user-data-dir="$ANYSEE_TEST_RUN/profile" \
  --remote-debugging-address=127.0.0.1 \
  --remote-debugging-port=0 \
  --no-first-run \
  --no-default-browser-check \
  about:blank > "$ANYSEE_TEST_RUN/browser.log" 2>&1 &

printf '%s\n' "$!" > "$ANYSEE_TEST_RUN/browser.pid"
printf '本轮输出：%s\n' "$ANYSEE_TEST_RUN"
cd std/apps/workspace
node -p "require('@playwright/test/package.json').version"
```

接着执行下面的脚本。它等待当前配置的 `DevToolsActivePort`，核验监听端口归属，在独立 context 里操作无外部依赖的页面，然后断开连接并确认浏览器仍存活。

```sh
node <<'JS'
const { chromium, expect } = require('@playwright/test');
const fs = require('node:fs/promises');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const { setTimeout: delay } = require('node:timers/promises');

async function main() {
  const run = process.env.ANYSEE_TEST_RUN;
  if (!run) throw new Error('缺少 ANYSEE_TEST_RUN');
  const pid = Number((await fs.readFile(path.join(run, 'browser.pid'), 'utf8')).trim());
  let port;
  for (let attempt = 0; attempt < 100; attempt++) {
    process.kill(pid, 0); // 只检查本轮启动的进程是否存在。
    try {
      const text = await fs.readFile(path.join(run, 'profile/DevToolsActivePort'), 'utf8');
      const candidate = Number(text.split('\n')[0]);
      if (Number.isInteger(candidate) && candidate > 0 && candidate < 65536) {
        port = candidate;
        break;
      }
    } catch (error) {
      if (error.code !== 'ENOENT') throw error;
    }
    await delay(200);
  }
  if (!port) throw new Error('20 秒内没有取得 CDP 端口，请检查 browser.log');

  const listeners = execFileSync('lsof', [
    '-nP', '-a', '-p', String(pid), '-iTCP', '-sTCP:LISTEN',
  ], { encoding: 'utf8' });
  if (!new RegExp(`(?:127\\.0\\.0\\.1|\\[::1\\]):${port}\\s`).test(listeners)) {
    throw new Error('调试端口不属于本轮 AnySee 的回环监听');
  }
  const endpoint = `http://127.0.0.1:${port}`;
  console.log({ pid, endpoint });
  const browser = await chromium.connectOverCDP(endpoint, {
    timeout: 15000,
    noDefaults: true,
  });
  browser.on('disconnected', () => console.log('CDP disconnected'));
  let context;
  try {
    const session = await browser.newBrowserCDPSession();
    try {
      console.log(await session.send('Browser.getVersion'));
    } finally {
      await session.detach();
    }
    context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    const page = await context.newPage();
    page.setDefaultTimeout(10000);
    page.on('crash', () => console.error('PAGE_CRASH'));
    page.on('close', () => console.log('Test page closed'));
    await page.setContent(`
      <h1>AnySee CDP smoke</h1>
      <label>Name <input></label>
      <button onclick="document.querySelector('output').textContent = document.querySelector('input').value">Echo</button>
      <output></output>
    `);
    await page.getByRole('heading', { name: 'AnySee CDP smoke' }).waitFor();
    await page.getByLabel('Name').fill('Codex');
    await page.getByRole('button', { name: 'Echo' }).click();
    await expect(page.locator('output')).toHaveText('Codex');
    await page.screenshot({ path: path.join(run, 'smoke.png'), fullPage: true });
  } finally {
    // 只清理自己创建的 context；不要换成 browser.contexts()[0].close()。
    try {
      if (context) await context.close();
    } finally {
      // 在本文验证的 1.63.0 + connectOverCDP 组合中，此处仅断开连接。
      await browser.close();
    }
  }
  const response = await fetch(`${endpoint}/json/version`, {
    signal: AbortSignal.timeout(3000),
  });
  if (!response.ok) throw new Error('断开后浏览器健康检查失败');
  console.log('PASS：交互和截图通过，断开后 AnySee 仍运行');
}

main().catch(error => { console.error(error); process.exitCode = 1; });
JS
```

这段脚本会保留临时浏览器，便于检查窗口和日志。检查完成后，只正常退出本轮临时实例；不要使用 `pkill AnySee` 或 `killall AnySee` 清理所有实例。测试目录中的 `browser.log`、`browser.pid`、`smoke.png` 可一起用于定位问题。

整理文档时已提取上面的原样 Node.js 代码，使用另一份全新配置实际运行通过，确认截图生成、CDP 断开后健康检查成功，随后才由启动者退出临时实例。

`noDefaults: true` 是本文验证的 Playwright 版本支持的选项，用来减少连接时对已有默认 context 的下载、焦点和媒体仿真设置的覆盖；它不会关闭全部 CDP 初始化，也不是防崩溃开关。旧版本应先核对 API 支持情况。

## 4. 关闭与断开：最容易混淆的地方

| 操作 | 含义与使用边界 |
| --- | --- |
| `page.close()` | 关闭该页面；只能对自己创建、可以丢弃的标签页执行 |
| 自建 `context.close()` | 关闭这个 context 及其中页面、弹出页；适合隔离测试收尾 |
| 默认 context 的 `close()` | 会进入浏览器级清理流程，不能当成测试 context 使用 |
| `browser.close()`，对象来自 `connectOverCDP()` | 本文的 Playwright `1.63.0` 实测只断开 CDP；AnySee 进程和已有页面保留 |
| `browser.close()`，对象来自 Playwright 的 `launch()` | 属于另一条生命周期路径，会关闭其启动的浏览器 |
| `session.send('Browser.close')` | CDP 原生命令，主动请求浏览器退出；与上一行的 JavaScript 方法不能混为一谈 |
| 正常退出、`SIGTERM`、`SIGKILL` | 都是进程退出来源；若清理脚本主动触发，不能据此认定连接导致崩溃 |

因此，“绝对不能调用 `browser.close()`，否则 AnySee 必崩”的说法不准确。本文既检查了本机 Playwright 的 CDP transport 实现，也用独立实例验证了断开、健康检查和重新连接。换 Playwright 版本、连接插件或启动方式后，应重新跑最小复现。

单次 Node 脚本结束或使用 `process.exit()` 也不能单独证明浏览器崩溃。强制结束脚本会跳过未完成的异步清理与证据写入，优先使用 `try/finally` 完成收尾。

## 5. 连接已有用户实例时的做法

1. 先用 `ps` 检查真实 AnySee 主进程、程序路径、完整启动参数和用户数据目录。不要只按进程名猜测使用的是安装版还是开发构建。
2. 已启用 CDP 时直接连接；从对应配置目录的 `DevToolsActivePort` 获取动态端口，或核验已有固定端口。再用 `lsof -nP -a -p <PID> -iTCP -sTCP:LISTEN` 确认监听归属。
3. `/json/version` 能响应后，再使用 `connectOverCDP(endpoint, { timeout: 15000, noDefaults: true })`。不要复用其他实例遗留的端口文件。
4. 在用户默认 context 中新建专用测试页，避免覆盖用户正在填写的表单、修改其视口或刷新工作页面。mock、视口和触屏回归优先放到自建 context。
5. 如已有实例没有启用 CDP，额外启动命令不保证能给它补上参数。需要正常退出并重新启动时，先保存用户工作并明确说明影响；不要强杀日常浏览器或删除其配置目录。
6. 结束时关闭自己的测试页、弹出页，移除自己注册的路由及事件；只断开连接，保留用户实例。

同一实例在同一时间只安排一个会操作页面的测试执行器。避免录制、页面测试、其他 agent 和用户同时抢焦点、切换标签页或重载页面。发现超时先检查页面是否被另一方导航，不要自动重启浏览器。

## 6. Pishoo / DHTTP 测试中的额外经验

- **核验接口就绪，而非只等页面加载。** `domcontentloaded` 后等待实际标题、表格或接口结果。持续 WebSocket 或轮询可能使 `networkidle` 长时间不成立；固定等待几秒也不能保证页面可操作。
- **检查实际身份。** 当前介绍页读取 `/std/profile/context` 并核对 `viewer_name`；工作台从 `/std/profile` 的 `identity_name` 获取身份，另行合并联系人和审批统计。不要只看地址栏或页面头像判断登录身份。
- **区分网页隔离与原生身份。** 新配置目录、新 context、DHTTP 原生身份不是同一层。不要为迁移测试随意复制日常证书或私钥，也不要用伪造请求头替代握手认证。
- **区分解析失败与崩溃。** 这次 VM 测试曾因 AnySee 对 macOS `bridge100` 的网络接口处理而无法解析 Alice 原生域名。浏览器仍在运行；通过使用真实 Spike 凭据的本机 HTTP bridge 完成了访问。这是网络链路问题，bridge 也只是该测试环境的入口，不是通用浏览器修复。
- **区分普通 HTTPS 与 DHTTP。** AnySee 中完整 `.dhttp.net` 主机可能呈现为 `~` 简写。断言 URL 时允许客户端实际表示，但仍应检查完整目标身份、路径和查询参数。
- **在点击前监听新标签页。** `target="_blank" rel="noopener"` 下，新页面可能没有可供网页访问的 `window.opener`。使用 `context.waitForEvent('page')`，并把可能影响首个请求的 mock 注册在自建 context 上；不要只在原 page 上注册。

```js
const opened = context.waitForEvent('page', { timeout: 10000 });
await page.getByRole('link', { name: '以 spike.liu 的身份添加' }).click();
const tab = await opened;
await expect(tab).toHaveURL(expectedUrl);
await expect(page).toHaveURL(originalIntroductionUrl);
// 只关闭这个用例创建的新标签页。
await tab.close();
```

真实服务验收先做只读访问。好友申请、聊天、资料修改和审批属于实际写操作，测试应使用隔离数据或明确授权；超时后先查结果，不自动重试提交。这次介绍页新标签页验收拦截了目标工作台页面，没有发送真实申请。

页面截图可直接使用 `page.screenshot()`，不必为此开启系统级录屏权限。它记录网页内容；要录制地址栏、标签栏等原生窗口区域，需要另行处理系统捕获权限与窗口捕获方式。

## 7. 同事说“崩溃”时，先按现象分层

| 现象 | 优先核对 |
| --- | --- |
| 新启动的进程马上结束，原窗口还在 | 是否复用同一个配置目录；是否触发 Chromium 单实例转交；`browser.log` 是否有目录锁信息 |
| `ECONNREFUSED` 或连接超时 | 端口是否陈旧、是否属于该 PID、CDP 是否真正开启；不要直接归因于网页 |
| `Target page, context or browser has been closed` | 哪个对象被关闭；`finally`、测试 fixture、MCP 或启动器是否进行了清理 |
| `disconnected`，但主进程和 `/json/version` 仍正常 | CDP transport 断开；检查是谁主动断开及 WebSocket 日志 |
| 只有一个标签页报错，主进程仍在 | 记录 `page.on('crash')`；区分 renderer 崩溃、JS 异常和普通网络错误 |
| 主进程消失 | 对照退出码、信号、主动关闭命令和 macOS 诊断报告；进程消失本身不能证明原因 |
| 页面显示域名解析或请求失败 | 检查 DHTTP 发现、网络接口、目标服务和身份；这不等于 AnySee 进程崩溃 |

建议在脚本中记录 `page.on('crash')`、`page.on('close')`、`browser.on('disconnected')`，并记录脚本主动关闭的时间和对象。`pageerror` 是页面 JavaScript 错误，不能当作 renderer 崩溃信号。

macOS 崩溃报告通常位于 `~/Library/Logs/DiagnosticReports/`，也可从 Console.app 检查。保留主进程和 renderer/GPU 子进程报告；没有报告不能反向证明没有崩溃。不要仅凭一条 WebSocket 异常给 AnySee 下结论。

逐层缩小复现：独立配置手动打开 → CDP 只读连接 → 新建页面 → 输入、点击 → 截图 → 加回 DHTTP 和产品页面 → 加回录制、trace 或插件。每次只增加一个变量，记录首次失败的步骤。GPU 相关参数只有在日志或调用栈指向该方向时，才在隔离实例中做单变量对比；不把 `--disable-gpu`、`--no-sandbox`、`--disable-web-security` 当作默认启动配方。

## 8. 发给排查人员的材料

保留一份简短复现报告，包含：

- 操作系统、AnySee 实际可执行文件路径和构建信息、Playwright/控制插件版本。
- 完整启动参数、是否临时配置、是否连接已有实例、主进程 PID 和端口归属。
- `Browser.getVersion` 或 `/json/version` 中的 product、revision、protocolVersion。
- 最小脚本、首次失败步骤、发生时间、完整异常、浏览器 stdout/stderr。
- 当时主进程是否仍在、`/json/version` 是否可达、是否收到页面 `crash` 事件、是否有诊断报告。
- 是否在独立配置、`about:blank` / 本地静态页面、关闭其他控制器后仍可复现。

分享前去掉真实网页内容、账号令牌和私钥；无需发送整个日常用户配置目录。动态端口、PID 和输出目录只描述该次运行，不应当作后续测试的固定配置。

## 9. 可以直接交给 Codex 的任务说明

> 请使用本机 AnySee 和已有 Playwright 依赖，通过 CDP 做测试。先确认实际二进制、版本、PID、配置目录和调试端口归属。先在全新临时配置上跑最小页面复现，再增加产品页面和 DHTTP 身份验证。连接使用明确超时；支持时设置 noDefaults。把默认用户 context 与自建测试 context 分清，只清理自己创建的页面和 context，不强杀日常浏览器、不删除其配置。记录页面 crash、页面 close、CDP disconnected、主动清理及进程退出，先用日志区分断开、正常退出和崩溃。真实写操作不要自动重试；无需重新编译 AnySee 或 Pishoo。

参考 API：[connectOverCDP](https://playwright.dev/docs/api/class-browsertype#browser-type-connect-over-cdp)、[Browser.close](https://playwright.dev/docs/api/class-browser#browser-close)、[BrowserContext](https://playwright.dev/docs/api/class-browsercontext)。本文的生命周期结论以所列版本的本机源码检查和实际复现为依据。

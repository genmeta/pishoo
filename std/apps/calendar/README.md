# Calendar 前端 App

独立的 Solid + TypeScript 日历，挂载 `/std/calendar/`，版本 `0.1.0`。参考 Workspace 的黄色侧栏、粗边框和蓝色按钮；支持中英文完整页面、月视图、周视图、日程列表、日期导航、当前视图搜索、颜色筛选、日程增删改、全天与跨天日程。移动端采用紧凑月历与下方日期详情。

前端仅调用依赖 Calendar Lib 的 `/std/calendar/events` 和 `/std/calendar/event`，没有 App OpenAPI 或后端 atom。业务数据以 Lib 的 SQLite 为准。语言默认遵循浏览器，可切换或指定 `?lang=en` / `?lang=zh-CN`；日期、时间、无障碍标签、表单、确认和错误都随语言变化。

```sh
cd std/apps/calendar
bun install --frozen-lockfile
PISHOO_CALENDAR_BACKEND=http://127.0.0.1:3000 bun run dev
bun run typecheck
bun run build
```

开发服务器只代理两条精确业务路径；部署时静态资源相对当前 App 地址加载。普通 daccess 保护前端和 API，Lib 额外核对真实主人身份，安装不自动授权。

从仓库根目录生成两份独立签名 WEBC：

```sh
WASI_SDK_PATH=/path/to/wasi-sdk sh std/apps/calendar/package.sh --publisher YOUR_NAME.dhttp.net
```

生成 `target/calendar-dist/calendar-lib.webc` 与 `calendar-app.webc`，App 精确依赖同一发布者的 `calendar@0.1.0`。需要本机发布者签名凭据；`PISHOO_PACKAGE_WEBC` 可指定兼容的现有打包器。构建不上传或安装，不修改源码发布者。正式标准发行使用 `std.dhttp.net` 签名；源码位于 std 本身不赋予特权。

定时日程按浏览器本地时区展示，页脚标明时区。全天日程保持日历日期，编辑器中的「最后一天」是包含边界，API 保存排他结束日期。月视图包含相邻月份，周视图按周一开始；列表与搜索只覆盖当前显示时段。并发版本冲突保留当前编辑内容，需重新打开日程后编辑。

后端错误采用 [RFC 9457 Problem Details](https://www.rfc-editor.org/rfc/rfc9457.html)。前端依据真实 HTTP 状态展示完整本地化文案，包括授权拒绝、等待审批、版本冲突、容量和连接错误。删除需要应用内确认，编辑对话框支持键盘焦点约束、Esc 关闭与焦点恢复。

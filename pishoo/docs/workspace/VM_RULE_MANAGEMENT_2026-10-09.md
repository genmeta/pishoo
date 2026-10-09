# 2026-10-09 访问规则管理恢复与双 VM 实测

原始验收证据仅保留在本地 `vm-evidence/2026-10-09/`，不纳入版本管理。下文证据文件名均相对此目录。

已恢复「快捷设置 → 访问规则」中的**添加规则、编辑、删除**，并部署到 Alice、Bob 两台 VM。
双方分别在真实页面操作，另一身份使用原生 DHTTP/HTTP/3 请求验证规则效果。

## 入口消失的原因

`cd495e324e492bfbb5e1d7ec08a9a134ec47cf02`（2026-09-30，`feat(workspace): add contacts, approvals, settings and chat views`）
将规则页面改为只读：删除 `PoliciesPage.tsx` 的添加按钮、编辑表单、编辑/删除操作以及
`api/client.ts` 的 `setRule`、`deleteRule`。提交说明明确写了 `Keep access rules read-only in the UI.`。
后端 daccess 的规则写入和删除接口仍然存在。

10 月 8 日的[补充验收](VM_EXTENDED_ACCEPTANCE_2026-10-08.md)只检查了规则查看及视图切换，
没有识别原有手动管理功能的缺失；本次补齐这项验收。

## 恢复的行为

- 页面右上角提供「添加规则」，填写 API 路径、HTTP 方法、效果及主体。
  支持允许、审批、拒绝，普通方法及 `*`，完整联系人名称、分组和 `**`、`?`、`*?` 选择器。
- 按 API 查看时预填当前 API；按主体查看时预填完整主体名称。
- 每行恢复编辑和删除。编辑固定 API、方法、主体，更新规则效果，避免修改规则标识后留下原规则。
- 写入复用 `PATCH /acl/allow/{name}`，删除复用 `DELETE /acl/allow/{name}`。
  API 路径放在 JSON 中，因此根路径 `/` 也能添加，无需依赖非空通配路径路由。
- 保存后刷新列表并定位到该规则所属页；提交期间禁止重复操作。
  后端拒绝保存时，表单保留内容并显示错误。删除需要确认，后端的管理权限和管理员保护仍生效。
- 恢复中英文文案，并更新原先断言界面只读的 E2E 场景。

## 双 VM 实际结果

沿用[双 VM 环境](VM_TESTING.md)，基线为 `feat/rebase-daccess` / `c57c08d1e00272e5d0a8ccd73c7460e65c84a12a`，
加当前未提交的修复。没有伪造身份、mock 响应或直接写 SQL 创建测试规则。
所有被验收的规则均通过真实 Workspace 页面保存。

| 场景 | Alice 与 Bob 交换角色后的实际行为 | 结果 |
| --- | --- | --- |
| 主动添加允许规则 | 页面保存 204，对方原生请求返回 200，确实到达该 VM 的业务上游 | 通过 |
| 编辑为拒绝 | 页面保存 204，对方请求返回 403 | 通过 |
| 编辑为审批 | 页面保存 204，对方请求返回 202，生成审批记录；拒绝该审批后重试 403 | 通过 |
| 按主体删除 | 页面删除 204，规则从列表和后端规则树消失 | 通过 |
| 主体上下文 | 按主体点击添加，表单预填完整 DHTTP 名称 | 通过 |
| 根路径与匿名选择器 | 页面添加 `OPTIONS /`、主体 `?`、拒绝，后端返回 204 并准确保存；随后页面删除 | 通过 |
| 无效主体 | 不存在的联系人返回 400，错误在表单内显示，填写内容保留 | 通过 |
| 非 owner 写规则 | 对方真实证书调用规则写入接口返回 403 | 通过 |
| 中英文 | 中文和英文都能打开规则表单并显示对应字段 | 通过 |
| 响应式 | 两端在 1440、768、375 像素宽度下表单无横向溢出，保存按钮可见 | 通过 |
| 最终恢复 | 清理测试规则后规则树与本轮操作前一致，双方 Chat 收发能力仍齐备，待审批数量为零 | 通过 |

关键响应见 Alice（`02-alice-manual-rule-real-effects.json`） 和
Bob（`02-bob-manual-rule-real-effects.json`）。
最终规则页见桌面截图（`04-alice-rules-management-final.png`），
表单见手机截图（`01-alice-add-dialog-375.png`），
错误提示见截图（`03-alice-inline-save-error.png`）。
最终权限及运行状态见 Alice（`04-alice-final.json`） 和
Bob（`04-bob-final.json`）。

复验前，隔离 VM 中的 OCSP 响应已经过期；从证书声明的 OCSP 服务取得新响应，验证签名及
`good` 状态后更新缓存并重启测试服务。两端证书和私钥未替换，未关闭 TLS/OCSP 校验。
新响应的 `Next Update` 为 2026-10-16 03:35 UTC。

只在 Alice VM 局部构建运行程序后部署两端。本机 `bun run typecheck`、VM 中包含 TypeScript
检查的前端构建及 `git diff --check` 均通过；未运行整套 E2E 或 cargo tests。
构建记录（`build-access-rules.log`）与
验收改动（`tested-rule-management.patch`）已保存，后者含 App 的此前聊天布局改动。

| 程序 | 两端相同的 SHA-256 |
| --- | --- |
| `pishoo` | `a359f2624431801d679c0eded3b020e347970c638fec4cbf3bda40aa43baceec` |
| `pishoo-client` | `073ee94e6e49158243680f43d3ceb2e1b8c8f6090a833ce14aa841aa7528375d` |

两端 Pishoo、浏览器入口和业务上游服务保持运行。代码及文档改动未暂存、未提交；
HTTP/3 稳定性、完整证书轮换及长期运行仍按此前报告的范围处理。

# 当前目录迁入 feat/pishoo-integration

2026-10-09，按用户确认的方案，在原目录切换分支，再恢复已有修改。

| 项目 | 切换前 | 切换后 |
| --- | --- | --- |
| 工作目录 | `/Users/x/code/genmeta/gateway/gateway` | `/Users/x/code/genmeta/gateway/gateway` |
| 分支 | `feat/rebase-daccess` | `feat/pishoo-integration` |
| HEAD | `c57c08d1e00272e5d0a8ccd73c7460e65c84a12a` | `4856e259a7c818a5c93bc8e10173f4fed4bd2f5c` |
| 远程跟踪 | `origin/feat/rebase-daccess` | `origin/feat/pishoo-integration` |

新分支是在原 HEAD 之后追加的四个提交，没有分叉。先在独立 worktree 中预览合并，
随后完成原目录切换；没有改名、移动或替换原目录及其 `.git`。

## 迁移操作

1. 在仓库外备份修改补丁、13 个已修改文件、272 个未跟踪文件和本地协作规则，记录文件摘要。
2. 将临时 `gateway-pishoo-integration` worktree 转为 detached HEAD，释放目标分支；其中的修改继续保留。
3. 在当前目录用普通 `git apply --reverse` 临时撤回已备份修改。
4. 执行 `git switch --no-overwrite-ignore feat/pishoo-integration`，保护忽略文件。
5. 用普通 `git apply` 恢复修改，再逐一核对代码与合并预览。

没有使用 stash、`git add`、`git apply --index/--3way` 或创建提交。分支切换正常更新
Git 的基线，恢复后的修改保持未暂存，暂存差异为空。

## 保留内容与核验

13 个原有修改文件全部迁入，包括聊天消息随机 ID、联系人深链接 fallback、主动刷新
申请状态、聊天轮询和完整分页、聊天高度修复、访问规则新增/编辑/删除及相关 E2E 断言。
双方都修改的六个文件实际修改位置不同，补丁全部成功应用。

合并结果保留了新分支的日志改动、Apps 页面、Lib API、双语文案、WASM 样式及导航测试 fixture。
迁入文件已逐一与仓库外的合并预览核对，`git diff --check` 通过；随后更新本迁移记录及文档索引。

原有的 272 个未跟踪文件均已核对 SHA256，包含 VM 文档、截图、日志和响应证据，以及
`anysee-demo/` 和 `anysee-demo.zip` 中的未跟踪文件。原目录、`.git`、`anysee-demo/`、
`target/`、前端 `node_modules/` 和 `dist/` 的 inode 与切换前一致。
这些本地目录未搬迁，运行中的服务、数据库、证书和 VM 磁盘没有被替换。

临时 worktree 路径仍为 `/Users/x/code/genmeta/gateway/gateway-pishoo-integration`，
HEAD 为 `4856e25`，处于 detached HEAD，仅作为此前合并结果的保留副本。
后续开发继续使用原目录 `/Users/x/code/genmeta/gateway/gateway`。

## 验收范围

迁移操作本身没有编译或启动新程序；随后按用户要求，于 15:46–15:57 在两台 VM
部署当前 HEAD 和迁入修改的 Pishoo、客户端及浏览器桥，完成真实交互回归。
好友申请、双向授权、访问审批、聊天、撤权恢复、离线重试、访问规则管理、深链接和聊天布局通过，
管理入口的读取也通过，详见[集成分支双 VM 报告](VM_INTEGRATION_ACCEPTANCE_2026-10-09.md)。

[旧双 VM 报告](VM_ACCEPTANCE_2026-10-08.md)和
[本机与 VM 报告](VM_HOST_ACCEPTANCE_2026-10-09.md)继续保留各自的历史基线。
本轮 VM 没有安装 Lib；Note、Lib 安装移除、加载后的代理组合、WebSocket 及公网功能
仍需各自专项实测，不能据核心 Workspace 回归通过认定这些功能也已完整通过。

本次原地切换的备份和核验清单位于：

```text
/Users/x/Library/Caches/pishoo-branch-migration/20261009-143808/in-place-20261009-151559/
  tracked-changes.patch
  before-workspace/
  target-preview/
  state.json
  result.json
```

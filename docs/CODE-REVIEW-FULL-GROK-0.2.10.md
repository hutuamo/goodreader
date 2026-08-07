# GoodReader 全面代码检视报告

| 字段 | 内容 |
| --- | --- |
| **检视时间** | 2026-08-07 11:24 CST |
| **检视者** | Grok 4.5（xAI）— 只读全面检视，未修改业务代码 |
| **称呼** | 官家 |
| **基线提交** | `38c4fb8`（`feat: deepen agent integration and PDF generation`） |
| **分支 / 工作区** | `ds/codereivew-0.2.10` + **大量未提交 WIP 修复**（见 §0） |
| **产品版本** | `0.2.10` · macOS 13+ · Apple Silicon · Tauri 2 + Rust + TypeScript |
| **方法** | 四路并行全文模块审查（Agent / 制书 / 数据·HTTP / 前端·脚本）+ 主检视员对高严重项源码复核 + npm 全绿 + `cargo test` 在检视时点失败 |
| **结论** | 架构清晰、安全基线与契约校验扎实；工作区已修复多轮历史 P1，但 **当前 WIP 无法通过 `cargo test` 编译**。仍开放：**入架事务窗口**、**native 停止协作取消不完备**、**paused 卡死问答**、**书籍与 API 同源架构风险**、若干前端可用性缺陷。P0 以“数据/正确性可造成不可察觉损坏”为准；当前无远程 RCE 级 P0。 |

---

## 0. 检视范围与覆盖声明

### 0.1 覆盖程度（诚实声明）

| 模块 | 路径 | 覆盖 |
| --- | --- | --- |
| Agent 编排 | `agent.rs` ~2106 行 | **全文**（子代理 + 关键路径复核） |
| Agent 会话 | `agent_session.rs` ~1425 行 | **全文** |
| 制书 | `generation.rs` ~5084 行 | **系统性全文扫读**（按函数/阶段） |
| PDF 排版 | `pdf_composer.rs` ~504 行 | **全文** |
| HTML 导入 | `importer.rs` ~1222 行 | **主路径全文 + 测试入口** |
| 数据库 | `db.rs` ~1248 行 | **全文** |
| 书库契约 | `library.rs` ~575 行 | **全文** |
| HTTP | `server.rs` ~1647 行 | **全文** |
| 模型 / 入口 | `models.rs` / `lib.rs` / `main.rs` | **全文** |
| 书架 UI | `frontend/src/main.ts` ~1789 行 | **全文** |
| 阅读器 | `frontend/src/reader.ts` ~2159 行 | **全文** |
| 设置纯函数 | `settings.ts` | **全文** |
| CSS | `app.css` / `reader.css` | **交互与安全相关通读**（非审美逐行） |
| 脚本 | `scripts/check-*.mjs` / `test-*.mjs` | **全部** |
| 配置 | `package.json` / `Cargo.toml` / vite / tsconfig | 简要 |
| 文档 | README / CONTEXT / V1-SPEC / ADR 索引 / Agent·Import 设计 / 既有审查 | 通读关键章节 |
| **未做** | 真实 Codex/Claude/OpenCode 联调；真实长 PDF 任务；浏览器 E2E；渗透式 CSP 绕过实验 | |

**合计应用源码约 1.9 万行**（不含 `node_modules` / `target` / 样书资源）。
本报告是「全面模块级检视 + 高危项亲验」，不是「每一行都有独立注释式审计」。

### 0.2 工作区 WIP 状态（检视时）

相对 `38c4fb8` 已改（未提交）主要意图：

| 主题 | 状态（源码意图） | 验证 |
| --- | --- | --- |
| Legacy 停止键 + cancel 订阅 | 已改 | 有测例；需整库编译通过 |
| 同书单活跃任务事务内检查 | 已改 | 有单测意图 |
| 备份未来版本预检 | 已改 | 有单测意图 |
| PDF 翻译硬拒绝 | 已改 | 前后端均有 |
| removable 启发式收紧 | 已改 | 有单测 |
| 空 blocks 拒绝 | 已改 | 有单测 |
| 封面像素上限（`image` crate） | 已改 | **编译曾反复失败** |
| 单实例 `fd-lock` | 已改 | 逻辑清晰 |
| 字号 `null` 解析 | 已改 | `test:reader-settings` 8 pass |
| 遗留 running 问答恢复 | WIP 中 | **`recover_stale_question_tasks` 当前编译错误**（`connection` 未 `mut`） |
| dispose 先 cancel | 已改 | 仍受 cancel 语义限制 |

**检视时点验证：**

| 命令 | 结果 |
| --- | --- |
| 9 个 `npm run test:*`（含 `test:reader-settings`） | **全部通过** |
| `cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1` | **失败**：`db.rs` `recover_stale_question_tasks` — `connection` 需 `mut`（此前还曾出现 `cover_dimensions_ok` / image API 相关编译错误，说明 WIP 仍在变动） |

> **发布门禁**：在 WIP 合并前必须 `cargo test` 全绿。当前分支**不能**当作可发布状态。

---

## 一、项目理解

**GoodReader** 是 macOS 本地「书架 + 统一阅读器」：

1. 将 PDF / 本地 HTML / 公开 URL 转为 **契约化静态 HTML 书籍包**（`book.json` 权威、正文不可变）；
2. 阅读进度、高亮、笔记、书签与 AI 历史外置于 **SQLite + Application Support**；
3. AI 复用本机 **Codex / Claude / OpenCode / Cursor / 自定义 CLI**，**不存模型 API Key**；
4. Tauri 2 内嵌 WebView，业务 API 走 **127.0.0.1 随机端口 + 一次性会话 cookie**。

核心不变量（设计）：

- 书籍内容不可变；标注与进度不写回书籍包；
- 入架前契约/安全校验；书籍包不执行自带脚本；
- Agent 产物须过账本/校验，不能直接改正式书架。

---

## 二、架构优点（确认正确的路径）

### 2.1 安全与信任边界

- Loopback 绑定、64 位会话 token、`HttpOnly; SameSite=Strict`、`secure_eq` 常数时间比较；
- 写接口 Origin 精确匹配 + `application/json`；
- 书籍页 CSP `script-src 'nonce-…'`，仅注入 GoodReader `reader.js`；
- `resolve_package_file`：禁 `..`/绝对路径，canonicalize + `starts_with(root)`；
- 删除书仅允许 `Books/` 直接子目录；
- 前端用户串普遍 `escapeHtml`；AI Markdown `html: false` + 引用白名单；
- 外链：前端协议过滤 + `/api/open-external` 二次 scheme 校验。

### 2.2 数据与任务状态机

- WAL + foreign_keys + busy_timeout；
- `complete_agent_execution` / `pause_agent_execution` 条件 UPDATE → **stopped 不被迟到完成覆盖**；
- WIP：`create_question_task` 事务内 COUNT 活跃任务，堵住 HTTP 层 TOCTOU；
- WIP：备份 `user_version` 预检；单实例锁；遗留 running 问答恢复（意图正确，编译未过）。

### 2.3 制书与 PDF 行账本

- 行账本闭合（全覆盖、不重复、不可移除行不可 omit）逻辑扎实；
- OCR 页在 Auto 下 start 拒绝；PDF 强制 Agent；
- 空 blocks 已改为一律拒绝；
- removable 已从「前/后 3 行 + ≥3 页」收紧为「首/末行 + ≤40 字 + ≥80% 页」；
- importer：staging → validate → rename；禁书库内外互包含；
- 翻译账本键一致 + `{{GRn}}` 占位多重集。

### 2.4 Agent 编排

- 任务与 runtime 解耦；共享 `ai_messages`；Provider 会话按 `(book_id, runtime_id)`；
- Legacy 停止键与 `session_workspace` 对齐（WIP）；
- dispose_book 先 cancel 再 dispose（方向正确）；
- `ExecutionControl` 现用 `control.clone()` 写入 slot（**control move 问题已不在当前磁盘**）。

---

## 三、问题清单（按严重级别）

说明：级别以**当前磁盘代码**为准，并标注 WIP 是否已覆盖历史问题。

### P0 — 数据正确性 / 入架一致性

#### P0-1 书籍入架 rename 与任务 `completed` 非原子

- **位置**：`generation.rs` `execute_task`（约 684–717）
- **证据**：`fs::rename(candidate → books_dir)` 后才 `update_summary(..., "completed")` 与清理；书架扫描看到 rename 结果。
- **失败场景**：崩溃夹在中间 → 书已上架，任务被 `recover_interrupted_tasks` 标 `paused`；`progress≥42` 时可能整段重转 → **重复 UUID 新书**或幽灵状态。
- **修复**：发布意图文件 + 恢复时若目标已合法则标 completed；或 rename 后立刻原子写 completed 并在 recover 识别「已发布」。

#### P0-2（产品/设计偏差）自动入架，无「用户确认」闸门

- **位置**：同上 publishing 路径；前端仅展示 quality，无 accept API
- **证据**：`BOOK-IMPORT-DESIGN` 要求校验通过并经用户确认后入架；实现为自动 rename。
- **影响**：带 warnings 的书（TextLayer 稀疏页、缺图占位等）可在用户未明确接受时上架。
- **修复**：`awaiting_review` 状态 + accept/reject；仅 accept 后 rename。

> 若产品明确「校验通过即可自动入架」，可将 P0-2 降为文档债；P0-1 事务窗口仍在。

---

### P1 — 严重正确性 / 安全边界 / 可用性

#### P1-1 Native Agent 停止依赖协作 cancel，存在「订前已 cancel / 握手无取消」窗口

- **位置**：`agent_session.rs` Codex/Claude/OpenCode 循环 `cancel.changed()`；`read_rpc_response` / ensure 阶段无 cancel；`ExecutionControl.pid` 只写不读；`stop_question` 仅对 legacy `active_processes` 强杀
- **证据**：
  - `watch::Receiver::changed()` 不感知订阅前已为 `true`；
  - Native 不登记 `active_processes`；
  - 握手/RPC 读阻塞时 stop 可能无效直至超时（最长约 30 分钟）。
- **影响**：UI/DB 显示 stopped，CLI 仍可能继续跑并写会话状态。
- **修复**：循环前检查 `*rx.borrow()`；cancel 时 `pid` 强杀；RPC/ensure 纳入 cancel；native 也登记可杀句柄。

#### P1-2 `paused` 问答任务无法 stop，又占用「活跃」槽位

- **位置**：`db.rs` `stop_agent_task`（仅 `queued|running`）；`create_question_task` / `active_agent_tasks`（`NOT IN (completed,stopped)` 含 paused）
- **影响**：失败后用户可能：**不能新问、不能停、只能重试或清空 AI 历史**。
- **修复**：允许 stop paused；或 active 排除 paused 并提供 dismiss。

#### P1-3 `forget_book` / 封面路径未强制 UUID 形态

- **位置**：`server.rs` `forget_book`（不经 `book_package`）；`cover_override_path` / `remove_cover_override` / `save_cover_override` 用 `root.join(format!("{book_id}.{ext}"))`
- **证据**：`Path::join` 在绝对路径 `book_id` 时丢弃 root；含 `..` 可逃出 CoverOverrides。
- **影响**：在已持有会话 cookie 的前提下，可能删除库外匹配扩展名的文件（本地威胁模型，但仍破坏边界）。
- **修复**：所有 `book_id` 强制 `Uuid::parse_str`；封面文件名禁止路径分隔符。

#### P1-4 书籍 HTML 与 App API 同源

- **位置**：`server.rs` 同一 router 挂 `/books/*` 与 `/api/*`；Cookie `Path=/`
- **影响**：架构级：CSP/导入消毒任一失手 → 书籍页脚本可调用完整本地 API。
- **修复（中长期）**：书籍资源独立 origin/端口或不透明隔离；短期 serve 时再校验 + 内容哈希。

#### P1-5 动态渲染 / URL 抓取失败被静默降级

- **位置**：`generation.rs` `prepare_html_source` / `fetch_url`（渲染失败仍用静态/curl 结果）
- **影响**：任务可 `completed`，正文残缺却无硬错误（与设计「无法稳定抓取应停止」冲突）。
- **修复**：声明需要动态渲染时失败即 `bail!`。

#### P1-6 译文校验允许「空源 → 非空译文」

- **位置**：`validate_translation_map`
- **影响**：账本键 1:1 仍可通过，语义上可注入无来源正文。
- **修复**：源 trim 为空则译文必须为空或恒等。

#### P1-7 导入完成「开始阅读」与书库刷新竞态（前端）

- **位置**：`main.ts` `applyImportTaskUpdate` / 打开按钮
- **影响**：用户立即点「开始阅读」可能找不到书、无反馈。
- **修复**：等待 bootstrap 或用 `task.imported` 直接打开。

#### P1-8 目录侧栏遮罩不可点击关闭

- **位置**：`reader.css` `.gr-sidebar-overlay`：`display: none` 且 `pointer-events: none !important`；`.open` 未设 `display` 且 `pointer-events: auto` 被 `!important` 压过
- **影响**：点遮罩关目录失效（Escape/按钮仍可用）。
- **修复**：`.open { display: block; pointer-events: auto !important; }` 并理顺基态。

#### P1-9 内联标注重叠时 `surroundContents` 无防护

- **位置**：`reader.ts` `wrapTextRange` / `renderInlineAnnotations`
- **影响**：无 CSS Highlight 时重叠区间可抛错，部分高亮不渲染。
- **修复**：try/catch 分片包装或统一走 Highlight API。

#### P1-10 全局仅允许一个未完成导入任务（与队列 UI/设计不一致）

- **位置**：`generation.rs` `start` 拒绝第二个 unfinished
- **影响**：paused/failed 堵死新导入；`move_queued` 形同虚设。
- **修复**：文档改为单任务，或实现真队列（仅重型阶段 Semaphore(1)）。

#### P1-11 WIP 编译失败（发布阻断）

- **位置**：`db.rs` `recover_stale_question_tasks`（约 622）：`connection` 非 `mut` 却 `transaction()`
- **影响**：整库测试/发布不可用。
- **修复**：`let mut connection = ...`。

---

### P2 — 中等

| ID | 问题 | 位置 |
| --- | --- | --- |
| P2-1 | SSE broadcast 256 + Lagged 静默丢增量 | `agent.rs` / `server.rs` stream |
| P2-2 | `book_cover` 默认路径未走 `resolve_package_file`（symlink 读包外） | `server.rs` |
| P2-3 | 书库顶层目录 symlink 可被收录 | `library.rs` scan |
| P2-4 | Schema 无步进迁移，仅 CREATE IF NOT EXISTS + 强行 v3 | `db.rs` |
| P2-5 | 恢复备份后进程内 catalog/Agent 内存未 resync | `restore_backup` + server |
| P2-6 | 未来版本拒绝前仍写 `before-restore` 占备份配额 | `db.rs` |
| P2-7 | 删除书籍副本不 dispose Agent、不清理封面 | `delete_book_package` |
| P2-8 | curl `--location` 重定向 SSRF 面（公网→内网） | `fetch_url` / download |
| P2-9 | `pdfimages` 失败吞掉 → requires_figure 失效 | `generation.rs` |
| P2-10 | Agent `caption` 字段丢弃（`_caption`） | `pdf_composer.rs` |
| P2-11 | 翻译含 `pre` 账本但不写回 | 翻译 apply 路径 |
| P2-12 | 章节页码区间可重叠 | PDF 章节选择 |
| P2-13 | removable 收紧后仍可能把页首重复短栏头当页眉 | `repeated_pdf_lines` |
| P2-14 | 旋转页裁区尺寸未校验 | `render_pdf_region` |
| P2-15 | AI 流式全量 `innerHTML` 重绘打断交互 | `reader.ts` |
| P2-16 | `closeBookMenu` 误清任意 `aria-expanded` | `main.ts` |
| P2-17 | 进度保存失败 toast 可能刷屏 | `reader.ts` |
| P2-18 | 设置写入错误提示不一致 | `reader.ts` |
| P2-19 | 多数 `check-*.mjs` 仍是字符串存在性检查 | `scripts/` |
| P2-20 | OpenCode text 增量 vs 全量假设未验证 | `agent_session.rs` |
| P2-21 | 切换 runtime 后旧 session CLI 常驻 | `AgentSessionHost` |
| P2-22 | 导入 HTML 外链黑名单不完整（CSP 兜底） | `library.rs` |
| P2-23 | 封面边界：测试注释「>8192」与 `<=8192` 实现需对齐 | `server.rs` tests |

---

### P3 — 轻微 / 技术债

- 备份文件名精确到秒可能覆盖；备份持全局 DB 锁卡 UI
- 标注/进度不校验 block 是否真实存在
- AI 耗时用 `task.updated_at - created_at`（含排队/重试）
- `live_tasks.partial_output` / stderr 无界增长
- PDF 书无 `data-goodreader-block`（产品边界，应 UI 明示）
- 章节等权进度近似
- 检查点未绑定 runtime/指令版本
- 清除 AI 不删磁盘 `Sessions/`
- 自定义 CLI = 用户授权任意绝对路径执行（需 UI 强提示）
- Mutex poison 用 `expect`
- Debug `GOODREADER_E2E_SESSION`
- 死脚本：`convert-rust-for-dummies.mjs` 等

---

## 四、历史问题闭合对照（相对前轮审查）

| 前轮问题 | 当前状态 |
| --- | --- |
| Legacy stop 键错误 | **WIP 已修意图** + 测例；待编译绿 |
| 同书并发 TOCTOU | **WIP 事务内检查** |
| removable 过宽 | **WIP 已收紧** + 测例 |
| 字号 null→80% | **已修** + 行为测绿 |
| PDF+翻译静默 | **前后端拒绝** |
| 空 blocks 空白页 | **已拒绝** |
| 备份未来版本覆盖 | **预检** + 测例意图 |
| 无单实例 | **fd-lock 已加** |
| 封面像素炸弹 | **WIP 尺寸检查**；编译曾不稳 |
| control move | **当前已用 clone**（不成立为 P0） |

---

## 五、验证记录

```text
npm: test:import-ui / ai-task-status / ai-markdown / ai-stop / cover-replacement
     / reader-selection-ai / reader-display-settings / reader-typography
     / reader-settings
→ 全部通过（注意多数为字符串契约检查，非行为测）

cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1
→ 失败：db.rs recover_stale_question_tasks 中 connection 需 mut
```

---

## 六、建议处理顺序

1. **立刻（发布阻断）**
   - 修 `recover_stale_question_tasks` 编译错误；稳住 `cover_dimensions_ok`；全量 `cargo test` 绿。

2. **P0 入架**
   - 发布事务/恢复幂等（P0-1）；产品确认是否需要 awaiting_review（P0-2）。

3. **P1 Agent 与状态机**
   - cancel 初始态 + pid 强杀（P1-1）；paused 可 stop（P1-2）。

4. **P1 安全硬化**
   - book_id UUID（P1-3）；同源风险文档化/隔离路线图（P1-4）。

5. **P1 制书正确性**
   - 动态渲染硬失败（P1-5）；空源译文（P1-6）。

6. **P1 前端**
   - 遮罩 CSS（P1-8）；导入完成打开书（P1-7）；surroundContents（P1-9）。

7. **P2 按性价比**
   - SSE Snapshot 自愈、cover resolve、备份 resync、测试行为化。

---

## 七、检视声明

- **身份**：Grok 4.5（xAI）
- **时间**：2026-08-07 11:24 CST
- **动作**：只读全面检视；产出本报告文件；**未修改**业务实现以「修 bug」为目的（工作区中已有他人/并行 WIP 修复，与本检视并行存在）。
- **局限**：未联调真实 Agent CLI；未跑真实多页 PDF 长任务；WIP 在检视过程中仍在变化，以「报告时间点的磁盘内容」为准。
- **与前作关系**：替代性「全面版」结论；`CODE-REVIEW-REPORT-GROK-0.2.10.md` 为早期浅扫；本文件为全量模块覆盖后的权威汇总。

---

*报告结束。*

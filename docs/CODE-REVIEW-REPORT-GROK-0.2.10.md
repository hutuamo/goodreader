# GoodReader 代码检视报告（Grok）

| 字段 | 内容 |
| --- | --- |
| **检视时间** | 2026-08-07 11:13 CST |
| **检视者** | Grok 4.5（xAI）— 只读检视，未修改业务代码；本文件为检视产出 |
| **称呼约定** | 面向官家（仓库协作约定） |
| **基线提交** | `38c4fb8`（`feat: deepen agent integration and PDF generation`） |
| **分支** | `ds/codereivew-0.2.10` |
| **产品版本** | `0.2.10` · macOS 13+ · Apple Silicon · Tauri 2 + Rust + TypeScript |
| **检视对象** | 已提交代码 + 当前未提交工作区改动（见下文 §0） |
| **方法** | 通读 README / CONTEXT / V1-SPEC / ADR 索引 / AGENT-INTEGRATION-DESIGN；精读 `agent.rs` 停止与执行路径、`agent_session.rs` 会话模型、`db.rs` 任务终态、`server.rs` 鉴权与 AI API、`generation.rs`/`pdf_composer.rs` PDF 行账本、`reader.ts`/`main.ts`/`settings.ts` 显示设置；对照既有 `CODE-REVIEW-REPORT-0.2.10.md`（codewhale）做独立复核与状态更新 |
| **结论摘要** | 架构与安全基线扎实；P0 无。工作区已开始修复前轮 P1-1 / P2-1 / P2-2 / 部分测试门禁，但 **WIP 回归测试当前无法编译**。仍开放：**P1 并发问答错配**、**P1 PDF 可移除行启发式过宽**，以及备份恢复、SSE 丢包、单实例、封面解码等 P2。 |

---

## 0. 工作区状态（检视时）

相对 `38c4fb8` 的未提交改动：

| 路径 | 意图（从 diff 解读） |
| --- | --- |
| `src-tauri/src/agent.rs` | 抽取 `session_workspace`；`stop_question` 改用正确进程键；legacy `run_runtime` 订阅 `ExecutionControl`；新增 `stop_question_terminates_a_legacy_runtime_process` 测试 |
| `src-tauri/src/agent_session.rs` | `ExecutionControl::subscribe` 可见性放宽为 `pub(crate)` |
| `frontend/src/settings.ts`（新增） | `parseClampedSetting` / `clampNumber` 纯函数 |
| `frontend/src/reader.ts` | 字号/侧栏宽度改走 `parseClampedSetting`，`null` 不再变 0 |
| `frontend/src/main.ts` | PDF 制书时禁用「翻译为简体中文」 |
| `scripts/test-reader-settings.mjs` + `package.json` | 真实行为测试 `test:reader-settings` |
| `scripts/check-reader-selection-ai.mjs` | 用正则提取函数体，避免顺序切片导致负向断言恒真 |

**本环境验证：**

- `node --test scripts/test-reader-settings.mjs` → **8 pass**
- `node scripts/check-reader-selection-ai.mjs` → 通过
- `cargo test --lib stop_question_terminates` → **编译失败**

失败原因：新测试对 `AgentCoordinator` 值调用 `start_question`，而该方法签名为 `self: &Arc<Self>`，需 `Arc::new(coordinator)`（或同等包装）。另有 `BookManifest` 在非 test 导入路径上的 unused import 警告。

> 以下问题分级中：**「WIP 已修 / 部分修」** 表示意图正确但需以可编译、可绿测为准；未合并前仍视为风险未闭环。

---

## 一、项目理解

### 1.1 产品是什么

**GoodReader** 是 macOS 本地「书架 + 统一阅读器」：

1. 把 **PDF / 本地 HTML 目录 / 公开网页** 转为符合接入契约的 **静态 HTML 书籍包**；
2. 在单一 Tauri WebView 中提供书架、阅读、进度、高亮、笔记、书签；
3. 复用本机 **Codex / Claude Code / Cursor / OpenCode / 自定义 CLI** 做翻译、PDF 排版与书籍问答，**应用内不保存模型 API Key**。

核心承诺（来自 README / ADR / CONTEXT）：

- **书籍正文不可变**；阅读状态与 AI 历史外置于 SQLite 与数据目录；
- **`book.json` 是权威清单**；稳定 UUID 书籍身份 + 稳定 block 身份锚定标注；
- **书籍包不跑自带脚本**；阅读脚本由 GoodReader 注入并受 nonce CSP 约束；
- Agent 只做语义侧工作，**正式入架前必须过契约/质量校验**。

### 1.2 运行时架构

```text
┌─────────────────────────────────────────────────────────────┐
│  Tauri 2 WebView（书架 main.ts / 阅读器 reader.ts）          │
└───────────────────────────┬─────────────────────────────────┘
                            │ loopback HTTP + 一次性会话 cookie
┌───────────────────────────▼─────────────────────────────────┐
│  Rust：server.rs（axum）+ library 扫描校验 + generation 制书 │
│        agent.rs / agent_session.rs（CLI 会话与任务）          │
│        db.rs（SQLite：进度/标注/AI/任务）                      │
└───────────────────────────┬─────────────────────────────────┘
                            │ 本机 CLI stdio / stream-json
┌───────────────────────────▼─────────────────────────────────┐
│  Codex app-server · Claude · OpenCode · Cursor/自定义       │
└─────────────────────────────────────────────────────────────┘

数据目录：~/Library/Application Support/GoodReader/
  Books/ · Backups/ · CoverOverrides/ · AgentTasks/ · goodreader.sqlite3
```

代码体量（约）：Rust ~1.4 万行（`generation.rs` 约 5k 为制书重心）+ 前端 TS ~4k 行。文档体系完整（V1-SPEC、28 条 ADR、导入/Agent 设计、BACKLOG）。

### 1.3 0.2.10 功能重心

- 原生 Agent 持久会话（Codex / Claude / OpenCode）+ SSE 流式问答 + 停止/重试；
- PDF 逐页 Agent 排版（行账本 + 裁图 + 检查点），预检三模式 auto / text-layer / ocr；
- 选区「问 AI」、阅读字号与侧栏宽度、封面覆盖、AI 侧栏工作区。

---

## 二、做得好的地方

这些路径经代码复核，**当前结论为设计正确或风险可控**：

1. **会话与写接口边界**
   `127.0.0.1` 随机端口、随机会话 token、HttpOnly/SameSite、非 GET Origin 校验、`secure_eq` 常数时间比较；不开放 CORS。符合 ADR-0016 思路。

2. **书籍 CSP 与脚本注入**
   `book_csp` 使用 `script-src 'nonce-…'`，仅注入 GoodReader 版本化 `reader.js`；包内脚本契约校验 + CSP 双线。

3. **路径穿越**
   `resolve_package_file`：禁止 `..` / 绝对路径，canonicalize 后 `starts_with(root)`；book id 走 UUID 解析（调用侧）。

4. **问答终态守卫**
   `complete_agent_execution` / `pause_agent_execution` 对 `status` 有条件 UPDATE；迟到完成不会覆盖 `stopped`，也不会写入 assistant 消息（`db.rs` 事务内 `changed == 0` 直接返回）。

5. **PDF 行账本核心校验**
   `validate_layout`：未知字段严格解析、lineId 全覆盖、禁止重复引用、figure crop 边界、`requires_figure` 强制——方向正确，单测覆盖主要拒绝路径。

6. **Agent 任务与运行时解耦（设计）**
   共享 `ai_messages` + 按 `(book_id, runtime_id)` 的 Provider 会话，符合 AGENT-INTEGRATION-DESIGN。

7. **WIP 修复方向正确**
   进程停止键统一、设置解析排除 `null`、PDF 翻译 UI 禁用、设置行为测试从「字符串存在」升级为真实断言——与前轮报告优先级一致。

---

## 三、问题清单

### P1 — 严重（应在发布前闭环）

#### P1-1 Legacy「停止请求」杀不到进程（WIP 修复，未闭环）

- **位置**：`agent.rs` `stop_question` / `session_workspace` / `run_runtime`；测试 `stop_question_terminates_a_legacy_runtime_process`
- **基线问题（`38c4fb8`）**：`stop_question` 用 `tasks_dir.join(task_id)` 查 `active_processes`，而 Cursor/自定义 CLI 登记键是 `Sessions/<hex(book_id)>/workspace`；legacy 循环也不订阅 cancel。
- **WIP**：键已对齐，cancel 已传入 `execute_command_with_limits`。
- **阻塞**：新增集成风格测试 **当前无法通过编译**（`start_question` 需 `Arc`）。在测试转绿之前，不应宣称 P1-1 已修。
- **建议**：`let coordinator = Arc::new(AgentCoordinator::new(...)?);` 后调用；清理 `BookManifest` 导入警告；绿测后再合并。

#### P1-2 同书并发问答：检查与创建非原子 + 共享工作区竞态（仍开放）

- **位置**：`server.rs` `create_book_question`（先 `active_agent_tasks` 再 `create_question_task`）；`db.rs` `create_question_task` 无「同书仅一条活跃」约束；`agent.rs` `prepare_workspace` 覆盖写同一 `Sessions/<book>/workspace`
- **场景**：双 POST 竞态，或「停止后立刻发新问」时旧 turn 尚未退出。后写的 `context/current.md` 可能被旧执行读到；跨 runtime 时无会话槽串行化更易错配。
- **影响**：`ai_messages` 问题/回答静默错配——比 UI 闪错更难察觉。
- **建议**：DB 层部分唯一索引或事务内 `SELECT … FOR` 式串行（SQLite 可用「同 book 活跃任务计数 + 唯一应用锁」）；执行路径对 `book_id` 持异步互斥，覆盖工作区与 Provider turn。补并发 HTTP/集成测试。

#### P1-3 PDF `removable` 启发式过宽，正文可被合法省略（仍开放）

- **位置**：`generation.rs` `repeated_pdf_lines`（每页前/后 3 行、归一化后 ≥3 页出现即 removable）；`pdf_composer.rs` `validate_layout` 仅校验「omitted 必须带 removable」，不校验语义
- **场景**：跨页重复表头、栏目名、题号等进入 `omittedLineIds` → 校验通过 → **静默丢正文**，违背「正文不可变 / 行账本完整性」产品承诺。
- **建议**：收紧规则（更短、更靠边缘、更高页占比）；对「removable 且被省略」记入任务事件供审计；单测构造「3 页顶部同一正文行」断言不可 removable。

---

### P2 — 中等

#### P2-1 默认字号被钳到 80%（WIP 已修，前端行为测已绿）

- **基线**：`Number(null) === 0` → clamp 到 80。
- **WIP**：`parseClampedSetting(null, …, fallback)` 正确；`test:reader-settings` 覆盖 null/空串/非法/限幅。
- **剩余**：确认服务端 `loadPreference` 与前端路径都走同一语义；合并前跑一遍阅读器启动路径人工确认。

#### P2-2 PDF + 翻译组合（前端已禁用，后端未硬拒绝）

- **WIP**：`main.ts` 在 `needsLayoutAgent` 时禁用翻译复选框与文案说明。
- **缺口**：`generation.rs` `start_import` 仍允许 `request.translate && kind == Pdf`（仅要求 runtime）。任意客户端可绕过 UI。
- **建议**：`validate_start_request` / `start_import` 对 PDF 明确 `bail!("PDF 制书当前不支持翻译")`。

#### P2-3 零文本页空 blocks 可通过校验

- `pdf_composer.rs`：`blocks.is_empty() && !source.lines.is_empty()` 才拒绝；无文本层纯图页可生成空白 `pdf-page`。
- 建议：空 blocks 一律拒绝，或上游跳过空白页并记入 quality 报告。

#### P2-4 恢复更高 schema 备份：先覆盖活动库再失败

- `db.rs` `restore_backup`：backup 进活动连接后 `initialize()` 才因 `user_version` 失败。
- 建议：覆盖前只读检查源备份 `user_version <= SCHEMA_VERSION`。

#### P2-5 无单实例保护

- `lib.rs` 无 single-instance；双开可并发写同一 SQLite。
- 建议：`tauri-plugin-single-instance` 或 flock。

#### P2-6 封面仅魔数 + 32MB 大小，无像素上限

- `save_cover_override` / `cover_image_format`：不解压校验尺寸，存在 WebView 解码内存尖峰风险。
- 建议：解码并限制例如 ≤8192×8192。

#### P2-7 清除 AI 工作区 / dispose 在 turn 中可能长时间阻塞

- `agent_session.rs` `dispose_book` 等待 slot 锁；turn 最长约 30 分钟。
- 建议：dispose 前对该书 active control `cancel()`，并设超时。

#### P2-8 SSE `Lagged` 增量被静默丢弃

- `server.rs` stream：`BroadcastStream` 错误在 filter 中丢掉；capacity 256。
- 建议：Lagged 时补发 Snapshot（前端 sequence 去重可自愈）。

#### P2-9 旋转页裁区与裁图产物尺寸未校验

- `render_pdf_region` 仅查退出码与文件存在；`/Rotate` 页坐标语义依赖 poppler。
- 建议：读输出 PNG 尺寸断言；补旋转页 fixture。

#### P2-10 测试门禁仍以字符串检查为主（部分改善）

- 多数 `scripts/check-*.mjs` 仍是 `includes` 扫描。
- 已改善：`test-reader-settings.mjs`、选区问 AI 函数体提取。
- 建议：停止/并发/设置等关键路径优先「行为测试」而非 marker 列表。

#### P2-11 删除书籍副本不清理封面覆盖

- `forget_book` 会 `remove_cover_override`；`delete_book_package` 仅移到废纸篓并刷新目录，**不清理** `CoverOverrides`。
- 与 handoff「删除副本时同步清理」表述不一致，易残留孤儿封面。

---

### P3 — 轻微 / 技术债（择要）

| 项 | 说明 |
| --- | --- |
| 崩溃后 `running` 问答任务 | 导入侧有中断恢复，问答任务无对称恢复，UI 可能长期「处理中」 |
| 清除/忘记不删磁盘工作区 | `Sessions/<hex>`、`AgentTasks/` 残留 |
| 备份同步持全局 DB 锁 | 大库时 UI 卡顿 |
| 封面临时文件名固定 `{book_id}.tmp` | 连续替换潜在竞态 |
| schema 迁移 | 主要为 `CREATE IF NOT EXISTS`，缺少分步版本迁移框架 |
| `pdfimages` 失败吞掉 | 漏图检测可能失效 |
| PDF 书无 `data-goodreader-block` | 块级标注/对照对 PDF 书不可用（产品边界，建议 UI 明示） |
| 流式 UI 整段 `innerHTML` | 停止按钮点击可能被重绘打断 |
| 检查点未绑定 runtime/指令版本 | 换 Agent 可能复用旧页排版 |
| 死脚本 | `convert-rust-for-dummies.mjs` 等无调用方（非阻塞） |

---

## 四、安全与信任边界（总评）

| 面 | 评价 |
| --- | --- |
| 本地 HTTP 会话 | 良好：loopback + 短会话 + Origin/Content-Type |
| 书籍内容 XSS | 良好：CSP nonce + 契约去脚本；AI Markdown `html:false` + 转义/链接校验 |
| 路径与书库 | 良好：canonicalize 边界 |
| Agent 进程 | 中等：依赖 CLI 自带权限模型；自定义 CLI 可写共享工作区，停止/并发问题放大影响面 |
| 多实例 / 备份 | 偏弱：见 P2-4、P2-5 |
| 封面与资源 | 中等：类型魔数有，像素炸弹无 |

**总体**：对「本地可信用户 + 用户自备 Agent」威胁模型匹配；不宣称对抗恶意本地进程或供应链攻击。当前最大实质风险是 **AI 历史正确性（P1-2）** 与 **PDF 正文静默丢失（P1-3）**，而非远程 RCE。

---

## 五、与前轮 codewhale 报告的关系

仓库内已有 `docs/CODE-REVIEW-REPORT-0.2.10.md`（2026-08-07，codewhale）。本报告在独立复核后：

- **同意**其 P1-1 / P1-2 / P1-3 与多数 P2 的根因判断与证据方向；
- **更新状态**：工作区已对 P1-1、P2-1、P2-2（前端）、P2-10（局部）动手，但 P1-1 测试未绿、P2-2 后端未封；
- **补充**：WIP 编译失败、删除副本与封面清理不一致（P2-11）、后端 PDF 翻译仍可绕过。

不重复展开其已验证「无问题」列表；本报告 §二 给出独立肯定结论。

---

## 六、建议处理顺序

1. **立刻**：修好 WIP `stop_question` 测试编译并跑绿；合并进程键 + cancel 订阅。
2. **发布前**：P1-2（同书串行）、P1-3（removable 收紧）、P2-2 后端拒绝、P2-1 确认合并。
3. **紧随**：P2-4 备份版本检查、P2-8 SSE Snapshot 自愈、P2-3 空白页。
4. **近期**：P2-5 单实例、P2-6 封面解码、P2-7 dispose cancel、P2-9 旋转页、P2-11 删除清理封面、测试门禁继续行为化。

---

## 七、验证记录（本检视环境）

| 命令 | 结果 |
| --- | --- |
| `node --test scripts/test-reader-settings.mjs` | 8 pass |
| `node scripts/check-reader-selection-ai.mjs` | pass |
| `cargo test --lib stop_question_terminates -- --test-threads=1` | **compile error**（`start_question` 需 `Arc`） |
| 全量 `cargo test` | 本轮未完整复跑（以针对性编译失败为准） |

---

## 八、检视声明

- 身份：**Grok 4.5（xAI）**
- 时间：**2026-08-07 11:13 CST**
- 范围：只读理解与检视；**未修改** `src-tauri` / `frontend` / `scripts` 业务实现；仅新增本报告文件。
- 局限：未连接真实 Codex/Claude/OpenCode CLI；未跑真实 PDF 长任务；未做双实例/高并发压力实验。P1-2/P1-3 的严重性基于代码路径与威胁建模，建议在修复合并后用集成测试固化。

---

*报告结束。*

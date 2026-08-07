# GoodReader 代码检视报告（全量代码库）

- **检视时间**：2026-08-07 11:20 CST
- **检视者**：Kimi Code CLI（Kimi 模型；只读检视，未修改/暂存/提交任何代码文件）
- **基线**：HEAD `38c4fb8`（`feat: deepen agent integration and PDF generation`）+ 当前未提交工作区
- **检视范围**：整个代码库现状（非 diff）：`src-tauri/src/` 全部 11 个 Rust 文件（约 1.15 万行）、`frontend/src/` 3 个 TS 文件（约 4 千行）、`docs/` 规格与 28 份 ADR
- **方法**：双轴并行只读子代理检视——**Standards**（项目文档约定 + Fowler 代码气味基线）与 **Spec**（对照 V1-SPEC / BOOK-IMPORT-DESIGN / AGENT-INTEGRATION-DESIGN / ADR），关键断言由子代理抽查核实
- **前置参考**：`docs/CODE-REVIEW-REPORT-0.2.10.md`（同日 codewhale 审查 Agent 针对 0.2.10 工作区 diff 的报告）。本报告为全量现状检视，两报告互补：0.2.10 报告偏行为正确性（P1/P2 已亲验项），本报告偏结构可维护性与规格符合性；重叠发现（如上帝对象、字符串检查脚本、封面解码、SSE 丢增量等）互相印证。

---

## 一、项目理解

**GoodReader** 是面向 macOS（13+，Apple Silicon）的本地书架与统一阅读器，Tauri 2 架构：Rust 后端（书库管理、SQLite 阅读状态、loopback HTTP 服务、制书管线）+ 轻量 TypeScript 前端（书架与阅读器）。

核心原则：

- **书籍 = 不可变静态 HTML 包**：`book.json` 是唯一权威（ADR-0004），正文块有全书唯一稳定标识（ADR-0006），书籍不执行自有脚本（ADR-0010）；
- **阅读状态外置**：进度/高亮/笔记/书签存 SQLite，移除书籍不丢状态；
- **制书链路**：PDF / 本地 HTML / 在线链接 → 来源快照 → 确定性转换器 →（可选）本机 Agent CLI（Codex/Claude/OpenCode/自定义）翻译或 PDF 逐页排版 → 契约校验 → 入架；Agent 只做语义工作，不能绕过校验发布；
- **AI 能力复用本机已认证 CLI**，GoodReader 不保存模型 API Key，通过持久会话 + SSE 流式与前端交互。

当前版本 0.2.10，主要新增：原生 Agent 会话（`agent_session.rs`）、PDF 每页 Agent 排版（`pdf_composer.rs`）、选区问 AI、阅读显示设置、替换封面。

---

## 二、Standards（编码规范 / 代码气味）

未发现违反项目明确文档约定的**硬违规**（注释均为中文、无投机性抽象反例）；以下均为判断性发现，按严重度分组。

### 高严重度（正确性 / 安全风险）

1. **过期 PID 误杀进程组**：`src-tauri/src/agent.rs:415-424` 从磁盘 `process.pid` 读出可能过期数天的 pid 直接 `kill(-pid)`，pid 被系统复用时会误杀无关进程组。
2. **async 上下文阻塞 tokio 运行时（多处）**：`generation.rs:536` 对 PDF 逐页处理直接 `.await`，其内逐页同步 `Command::status()`（`:766`、`:2077-2084`），几百页 PDF 独占 worker；`server.rs:981/1005` 阅读器最高频路径 `book_asset` 同步 `fs::read`；`db.rs:743-747` 备份持全局 DB 锁慢速执行且 `server.rs:1157-1174` 未走 `spawn_blocking`。同代码库其他导入路径都规范用了 `spawn_blocking`，属漏网。
3. **`<pre>` 块送翻译却从不回写**：提取列表含 `"pre"`（`generation.rs:3529-3542`），应用列表没有（`:3633-3645`），译文静默丢弃、白烧 token；两个列表顺序还不同——重复代码埋的不一致。
4. **黑名单正则消毒 HTML 有绕过形态**：`importer.rs:992` script 正则只匹配成对闭合标签，无闭合的 `<script src=...>` 会保留，目前仅靠 CSP 单层兜底。
5. **`stop_agent_task` 先提交后报错**：`db.rs:617-629` commit 之后才 `bail!`，调用方收到错误但状态已变更。
6. **PDF 页实时输出不可达**：`generation.rs:274-277` 仅 `translating` 阶段附加 `live_agent_output`，而 PDF 排版在 `converting` 阶段，该分支永远拼不进去（死代码）。

### 中严重度（可维护性）

- **阻塞/同步调用蔓延**：`agent.rs:759-829`、`server.rs:464`（轮询接口里整库扫盘）等，与既有 `spawn_blocking` 惯例不一致。
- **用错误文案子串做分类**：`generation.rs:3394-3408`、`agent_session.rs:1218-1230` 对本地化错误字符串 `contains` 匹配，上游改文案逻辑即静默失效（Primitive Obsession）。
- **状态/阶段魔法字符串全库散布**：db.rs 任务状态裸字符串 22 处（同项目 `AnnotationKind` 却是正式枚举）、`reader.ts:71` `status: string`、generation.rs stage 字面量十多处——新增状态需散弹式修改且会静默漏判（Repeated Switches / Shotgun Surgery）。
- **大批量重复**（Duplicated Code / Data Clumps）：`record_translation_batch_result` 5 处调用 × 14 参数（`generation.rs:977-1048`）；`agent.rs:1070-1292` 两条子进程管线 60+ 行近乎逐行重复；`terminate_process_group` 两个行为分歧版本（`agent.rs:1521` 无优雅退出窗口 vs `agent_session.rs:1232`）；三个 osascript 选择器（`server.rs:1021-1347`）；前端确认删除双份（`main.ts:1403-1469`）；标注类型中文标签映射 5 处（`reader.ts:1328-1745`）。
- **锁中毒即全局瘫痪**：`.lock().expect(...)` 在 agent.rs 15 处、db.rs 约 20 处、server.rs 6 处，一处持锁 panic 级联全部 panic。
- **静默吞掉**：`reader.ts:1347/1555` 设置持久化裸调 `api` 无 catch（旁路就是现成的 `savePreference`）；`generation.rs:755/1627` `pdf_image_pages().unwrap_or_default()` 使扫描页 OCR 判定悄悄降级，而相邻 `pdfinfo` 失败却 `bail!`，策略不一致；`db.rs:692` 参数 JSON 损坏静默变空。
- **Divergent Change**：`generation.rs` 的 `ImportManager` 一个 impl 承载队列、PDF 管线、翻译编排、事件日志等六类职责（5023 行单文件，上帝对象形态）；`reader.ts` 单文件 2155 行 + 约 30 个模块级可变全局。
- **死代码**：`main.ts:1285-1312` `showImportComplete` 零调用；`agent_session.rs:130` `ExecutionControl.pid` 只写不读；`generation.rs:1825-1833` 不可达分支；`pdf_composer.rs:85` `caption` 字段接收但从不使用（与给 Agent 的指令自相矛盾）。

### 低严重度（归类）

菜单 `{ once: true }` 监听边界（`main.ts:1374-1383`）；魔法数重复（`main.ts` 0.995×4、`reader.ts` 延迟常量散落）；正则每次调用现编译（generation.rs 约 15 处、`agent.rs:1576`）；事件追加 O(n²)（`generation.rs:1729`）；实体双重解码（`importer.rs:770-779`）；schema 版本号两处维护且无迁移路径（`db.rs:17/147`）；封面用错大小常量（`library.rs:140`）；`SidebarKind` 三元分派 6 处；toast 定时器不清理（`reader.ts:2050`）。

### Standards 总体评价

整体质量中上：路径穿越防护（`library.rs:189-213`）、SQL 全参数化、前端 XSS 转义贯穿、错误链上抛规范、测试覆盖扎实。最突出的三个问题：**① async 与阻塞代码混用是系统性短板**——作者显然知道 `spawn_blocking`，但 PDF 渲染、`book_asset`、DB 备份三条最热路径恰好漏网；**② 魔法字符串状态机贯穿前后端**，配合"靠错误文案子串分类"，状态/文案一改动就静默失效，枚举化收益最大；**③ `generation.rs` 的 ImportManager 已呈上帝对象形态**，5000 行单文件内的重复管线（14 参数 × 5 调用）开始滋生 `<pre>` 回写缺失这类一致性缺陷。安全面最值得优先修的是过期 PID 误杀进程组一项。

---

## 三、Spec（规格符合性）

### (a) 规格要求但缺失 / 部分实现

1. **质量报告"警告需用户明确接受后才入架"未实现**。BOOK-IMPORT-DESIGN.md「警告必须提供位置与预览入口，并由用户明确接受」「警告经用户确认并完成最终接入契约校验后，才原子化进入书架」。实现中 `generation.rs:659-693`：硬错误 `bail!`，警告仅附进任务摘要（`task.summary.quality`），随后直接 `fs::rename` 自动入架，无任何确认环节。对应地，AGENT-INTEGRATION-DESIGN.md 状态机里的 `validating`/`awaiting_confirmation` 状态在实现中不存在（实际只有 queued/running/paused/failed/completed/cancelled）。**判断：实现真缺**。
2. **SQLite FTS5 本地检索未实现**。AGENT-INTEGRATION-DESIGN.md 要求用 FTS5 做关键词检索；`db.rs:54-149` 无任何 FTS 表，实际做法是把全书章节 Markdown 铺进任务工作区（`agent.rs:1546-1573`）。**判断：有意的简化替代，文档超前**。
3. **DB 结构升级路径缺失**。V1-SPEC.md 要求「数据库结构升级前额外备份」；`db.rs:50-53` 对 `user_version > 3` 直接拒绝启动，无增量迁移机制，"升级前备份"无从谈起。**判断：实现缺失（当前无需求，属隐患）**。

### (b) 规格未要求的实现（scope creep）

1. **正文字号调节**。`reader.ts:317-357` 提供 80–160% 字号步进，服务端设置白名单含 `reader-font-size`（`server.rs:1468`）。V1-SPEC.md 明确将「全局字体、行距或统一正文排版」列为 non-goal 并重申「不提供全局排版设置」。**直接违反 non-goal**（或规格未更新的滞后）。
2. **用户替换封面**。`POST /api/books/:id/cover`（`server.rs:196-198, 574-598`，前端 `main.ts:1386-1396`）写 `CoverOverrides/`。规格与 ADR 只规定导入时封面推断/默认封面（ADR-0019），无用户换封面需求。**判断：规格外功能，文档滞后可能性大**；实现方式（包外覆盖）至少不违反不可变原则。

### (c) 实现与规格矛盾

1. **OpenCode/自定义 CLI 的"只读"仅靠 prompt**。AGENT-INTEGRATION-DESIGN.md 要求「不使用无边界执行模式；某个 CLI 无法落实任务能力策略时，该运行时对该任务不可用」。实现中 Codex/Claude 有 CLI 级硬约束（`--sandbox read-only`、`--permission-mode plan --tools Read,Grep,Glob`），但 OpenCode 与自定义 CLI 只有提示词约束（`agent.rs:984-989, 1004-1007`）却仍被允许执行问答任务。
2. **PDF text-layer 模式绕开扫描页暂停语义**。BOOK-IMPORT-DESIGN.md 要求混合 PDF 存在无文本层正文页时「暂停整个生成任务」「不生成只包含可提取页面的残缺书籍」；`generation.rs:1643` 的 TextLayer 模式把扫描页降级为 `uncertain_pages` 继续生成。虽有用户显式选择作缓解，但与条款字面冲突。
3. **"永久忘记"范围两份文档不一致**：V1-SPEC.md 只说删阅读状态；CONTEXT.md 含 AI 工作区。实现（`db.rs:360-370` 删 progress+annotations+agent_tasks+agent_sessions）遵循 CONTEXT.md，属文档间滞后，实现选择合理。

### (d) 规格声明"暂不支持"的对照

- 扫描 PDF OCR：规格声明延后，代码也确实只有检测与拦截（`generation.rs:203-208`），一致。`PdfImportMode::Ocr` 枚举是占位，前端文案明示未支持，无不一致。

### Spec 总体评价

核心契约覆盖率很高：book.json 权威（ADR-0004）、内容不可变（0007，全文无正文写路径）、稳定块 ID 与一一对应（0006/0026/0028，`validate_layout`/`validate_translation_map` 强制执行）、ephemeral 认证 loopback（0016，随机端口 + Cookie + Origin/Content-Type 校验）、脚本独占（0010，导入清理 + nonce CSP）均忠实落地。最关键的三个偏差：**① 警告未经用户确认即自动入架，缺 `awaiting_confirmation` 环节（规格明确的验收闭环）；② 正文字号调节直接踩了 spec 的排版 non-goal；③ OpenCode/自定义 CLI 缺乏能力策略的强制落实，Agent 只读契约在这类运行时上形同虚设**。

---

## 四、汇总

- **Standards**：高 6 项 / 中 7 类 / 低 1 组；最差项——过期 PID 误杀进程组（`agent.rs:415-424`）与三条热路径阻塞 tokio 运行时。
- **Spec**：缺失 3 项 / scope creep 2 项 / 矛盾 3 项 / 一致确认 1 项；最差项——质量警告未经用户确认即自动入架，缺 `awaiting_confirmation` 验收闭环。

两轴各自独立，不互相遮盖：代码风格整体合规并不能弥补规格验收闭环的缺失；规格高度落地也不代表上帝对象和魔法字符串状态机不需要收拾。建议与 `CODE-REVIEW-REPORT-0.2.10.md` 的 P1/P2 清单合并排期。

# GoodReader 工作区代码检视报告（opencode / GLM-5.2）

- **检视时间**：2026-08-07
- **检视者**：opencode（模型 `zai-coding-plan/glm-5.2`），只读审查，未修改/暂存/提交任何文件
- **协作对象**：官家
- **基线**：`38c4fb8 feat: deepen agent integration and PDF generation`（HEAD）
- **检视对象**：当前未提交工作区（HEAD 之上的未跟踪/已修改文件，含新增 `frontend/src/settings.ts`、`scripts/test-reader-settings.mjs`，以及 `main.ts`/`reader.ts`/`agent.rs`/`generation.rs`/`server.rs`/`db.rs` 等的活跃改动）
- **代码规模**：Rust 后端 ~1.4 万行（`generation.rs` 单文件 5084 行）、TypeScript 前端 ~4 千行、CSS ~4.7 千行
- **方法**：人工通读 README/CONTEXT/ADR + 4 个并行只读子代理逐文件深审（生成/PDF 链路、Agent 会话链路、HTTP/DB/书库链路、前端链路）+ 关键断言亲验（`terminate_process_group` 两份实现、`restore_backup` 版本预校验、`repeated_pdf_lines` 收紧、`settings.ts` 被引用、`#app` aria-live、`save_setting` 无钳制）
- **比对基准**：与 `docs/CODE-REVIEW-REPORT-0.2.10.md`（codewhale，2026-08-07）交叉比对，单列「已在工作区修复的项」一节
- **结论**：P0 无；P1 5 项；P2 9 项；P3 若干（引用原报告，不重复罗列）。相较 0.2.10 报告，开发者已在工作区修复了其中 5 项（含 1 项 P1），但「停止/并发/会话续接」链路仍是主要风险源。

---

## 一、项目理解

**GoodReader** 是一个 macOS 本地书架与统一阅读器（Tauri 2：Rust 后端 + 无框架 TS 前端）。核心设计：

- **书籍 = 不可变静态 HTML 包**，`book.json` 是身份/元数据/章节顺序的唯一权威；阅读进度、高亮、笔记、书签全部外置到 SQLite，正文不可变。
- **制书链路**：PDF / 本地 HTML / 在线链接 → 来源快照 → 确定性转换器（提取/静态化/稳定 ID/脚本清理）→（可选）本机 Agent CLI 翻译或逐页排版 → 契约校验 → 原子入架。
- **AI 能力**：复用用户已配置好的本机 Codex / Claude Code / Cursor / OpenCode / 自定义 CLI，GoodReader 不持有模型 API Key；问答走原生持久会话（按书续接）+ SSE 流式。
- **运行形态**：启动时拉起一个绑定 `127.0.0.1:0` 随机端口的 loopback HTTP 服务，用 bootstrap 查询参数换 HttpOnly cookie，Tauri WebView 加载该地址。所有书籍内容经服务注入阅读器运行时后呈现。

架构安全设计相当认真：书籍内容零信任校验、双 CSP、常数时间会话比较、路径穿越多层防护、标注去重/重叠检查/状态机守卫。问题集中在 **Agent 协调层的并发模型**、**停止/终止链路**、**正文完整性启发式**、**几个会让应用卡死/锁死的系统性路径** 四处。

---

## 二、已在当前工作区修复的项（与 0.2.10 报告比对）

以下 0.2.10 报告中的问题，在当前未提交工作区中**已被开发者修复**（均已亲验代码）：

| 原报告编号 | 原问题 | 当前状态 | 证据 |
| --- | --- | --- | --- |
| P1-3 | PDF 重复行启发式把跨页表头标为可移除 → 正文静默省略 | ✅ 已收紧 | `generation.rs:2035-2063` `repeated_pdf_lines` 现仅取每页**首行/末行**、阈值 `max(3, 80%)`、行 ≤40 字符 |
| P2-1 | 新安装字号被钳到 80%（`Number(null)===0`） | ✅ 已修复 | 新增 `frontend/src/settings.ts:12` `parseClampedSetting`（null/空串/非有限值 → fallback），已被 `reader.ts:2,2125-2132` 引用生效 |
| P2-4 | 恢复更高版本备份先覆盖活动库后报错 → 应用永久无法启动 | ✅ 已修复 | `db.rs:817-825` 覆盖前用只读连接检查 `backup_version > SCHEMA_VERSION` 并拒绝 |
| P2-10（部分）| 选区问 AI 负向断言用 slice 会恒真 | ✅ 已重写 | `scripts/check-reader-selection-ai.mjs:21-28` 改用正则提取函数体；新增 `test-reader-settings.mjs` 用 `node:test` 对纯函数做真实行为测试 |
| P3（迁移 v2→v3）| 旧数据可解析 | ✅ 维持 | 新字段 `#[serde(default)]`，纯新增表 |

> **说明**：本报告下文不再重复这些已修复项，只列出**当前工作区仍存在**的问题。

---

## 三、P1 严重问题（5 项）

### P1-1 隔离浏览器子进程无墙钟超时，可永久阻塞整个导入队列 ✅已亲验

- **位置**：`src-tauri/src/generation.rs:2567-2580`（`render_online_html`）、`2594-2607`（`render_local_html`）
- **证据**：`Command::new(chrome).args([...]).output();` 同步等待子进程，无任何超时；`--virtual-time-budget=8000` 只约束虚拟渲染时间，不约束真实进程寿命。生成槽 `generation_slot = Semaphore::new(1)`（`generation.rs:104`）。
- **失败场景**：Chrome 因沙箱弹窗、网络挂起、`while(true){}`、被调试器附加等原因挂住时，调用线程永久阻塞 → 整个导入队列永久停滞，只能强杀进程。`render_online_html` 在 `preflight_url` 路径被调用（预检阶段）。
- **修复方向**：用 `tokio::time::timeout` 包裹 `spawn_blocking`，超时后 `child.kill()`；或 `Command::spawn` + 手动 `wait` 轮询施加墙钟上限。
- **注**：这是本次新发现，0.2.10 报告未提及。

### P1-2 工作区并发覆盖 + 创建任务 TOCTOU：同书多任务/多 runtime 共写一个目录 ✅已亲验

- **位置**：`src-tauri/src/agent.rs:758-828`（`prepare_workspace`，工作区路径只由 `book_id` 决定，见 `session_workspace:126-138`）、`src-tauri/src/server.rs:678-699`（检查与创建非原子）、`db.rs:431-449`（`create_question_task` 无并发约束）
- **证据**：工作区路径 = `Sessions/<hex(book_id)>/workspace`，与 `task_id`/`runtime_id` 无关。`prepare_workspace` 每次覆写 `context/current.md`、`history.jsonl`、`chapters/*.md`。`AgentSessionHost` 仅按 `(book_id, runtime_id)` 加 backend 锁串行化，**不同 runtime 间不互斥**；且 `prepare_workspace` 发生在获取 backend 锁**之前**（`agent.rs:554→566`）。同时 `create_book_question` 的 `active_agent_tasks.is_empty()` 检查与 INSERT 之间无锁。
- **失败场景**：对同一本书用 Codex 提问未结束又用 Claude 提问（或前端未防抖重复触发），两个 `prepare_workspace` 并发 → Claude 的 `current.md`（问题 B）覆盖 Codex 的（问题 A）→ Codex 长连接进程读到问题 B，把回答写进问题 A 的执行记录。AI 历史「问题-回答」静默错配，不可察觉的数据损坏。
- **修复方向**：工作区按 `(book_id, task_id)` 隔离；或在 coordinator 层对同一 `book_id` 的问答加全局互斥；并在 `agent_tasks` 上加部分唯一索引 `(book_id) WHERE status NOT IN ('completed','stopped')`。
- **与 0.2.10 报告**：合并原 P1-2（并发错配）+ 后端 TOCTOU。

### P1-3 停止/终止链路三处缺陷叠加：进程杀不掉、键不匹配、实现不一致 ✅已亲验

- **位置**：
  - `src-tauri/src/agent.rs:482-507`（`stop_question`）：原生会话进程**从不写入** `active_processes`（注释 `:493` 已承认），按 `session_workspace` 查 pid 键必然落空；`start_question` 先插初始 control（`:431-435`），`run_native_question` 后用 `run.control()` 覆盖（`:652-655`），启动瞬间停止会 cancel 到**废弃 control**。
  - `src-tauri/src/agent.rs:1520-1528` vs `agent_session.rs:1232-1240`：两份 `terminate_process_group` 行为不同——
    ```rust
    // agent.rs:1520 — SIGTERM 后紧跟 SIGKILL（立即强杀，不给清理机会）
    libc::kill(-(pid as i32), libc::SIGTERM);
    libc::kill(-(pid as i32), libc::SIGKILL);
    // agent_session.rs:1232 — 仅 SIGTERM（且 pid==0 时 return）
    libc::kill(-(pid as i32), libc::SIGTERM);
    ```
    legacy 路径（翻译取消/超时、`stop_question:503`、`cancel_generations_under:409`）全部调用「立即 SIGKILL」版本。
- **失败场景**：(a) 任务启动瞬间快速停止 → cancel 发到废弃 control → 原生进程跑到 30 分钟 `TURN_TIMEOUT`，DB 已标 stopped 但进程未终止；(b) 翻译取消/超时对 Codex/Claude 瞬杀，会话 id/状态未落盘 → 下次 `thread/resume`、`--resume` 续接失败；(c) Cursor/自定义 CLI 走 legacy 路径，登记键是 workspace 路径而 `stop_question` 按 task_id 路径查，键不匹配杀不到（原 0.2.10 P1-1）。
- **修复方向**：原生会话登记 pid（`ExecutionControl.pid` 已有 `AtomicU32`，各 `ensure_process` 都在写），`stop_question` 直接读 `control.pid` 杀进程组；统一为一份 `terminate_process_group`：SIGTERM → 等 2s → 必要时 SIGKILL（与 `NativeProcess::terminate:1093` 对齐）；`run_native_question` 用单一长生命周期 control。
- **与 0.2.10 报告**：含原 P1-1 + 新增的原生会话停止竞态 + 两份实现不一致。

### P1-4 `dispose_book` 最坏阻塞 30 分钟，且不打断在途任务

- **位置**：`src-tauri/src/agent_session.rs:281-296`（`dispose_book` 对每个 slot `backend.lock().await.dispose().await`）、`agent_session.rs:237-249`（turn 持 backend 锁到结束，受 `TURN_TIMEOUT`=30min 包裹）、`agent.rs:160-162`（`dispose_book_sessions` 只调 `sessions.dispose_book`，不清理 `live_tasks`/`active_questions` 也不 cancel control）
- **失败场景**：移除书籍 / 清除 AI 工作区时，若该书有进行中问答，`dispose_book` 挂起直到该 turn 自然结束或 30 分钟超时；调用链若在 Tauri command 中 `await`，前端「移除书籍」操作长时间无响应，且在途任务不会被 cancel，只是被动等完。
- **修复方向**：`dispose_book` 先 cancel 该书所有活动 `ExecutionControl`（需维护 book→task→control 映射），再等 backend 锁；或给 dispose 限期，超时强制 `terminate` 进程组。
- **与 0.2.10 报告**：原 P2-7，因叠加 P1-3 的终止缺陷，影响放大，提升为 P1。

### P1-5 数据库迁移机制无版本阶梯/无事务，下次 schema 变更即变 P0

- **位置**：`src-tauri/src/db.rs:48-151`（`initialize`）
- **证据**：整段迁移只有 `CREATE TABLE IF NOT EXISTS ...`（幂等建表）+ 末尾 `PRAGMA user_version = 3`（`:147`），**没有任何 `ALTER TABLE` 或按版本号分支**；`execute_batch` **不在事务内**。
- **失败场景**：一旦未来 v3→v4 增加列或改约束，旧库的 `CREATE TABLE IF NOT EXISTS` 命中已存在表后什么都不做，新列缺失，后续指名新列的 `INSERT` 失败；中途崩溃会留下「部分表已建、user_version 未更新」的中间态。
- **修复方向**：引入显式版本阶梯（`match version { 0|1|2 => ALTER ..., 3 => noop }`），并用事务包裹（注意 `PRAGMA user_version` 需在事务外执行）。
- **与 0.2.10 报告**：原 P3 提及「迁移机制结构风险」，本次提升为 P1（结构性债，下次变更即阻断）。

---

## 四、P2 中等问题（9 项）

### P2-1 PDF 页码正则误判英文短词为可移除，破坏「不可变正文」承诺 ✅已亲验

- **位置**：`src-tauri/src/generation.rs:2065-2067`（`pdf_source_lines`），正则 `(?i)^(?:[0-9]{1,6}|[ivxlcdm]{1,6})$`
- **证据**：任何由 `{0-9}` 或 `{i,v,x,l,c,d,m}` 组成、1-6 字符的**独占行**都被 `removable=true`。诸如 `mix`、`did`、`ill`、`id`、`dim`、`lv`、`mc`、`xl` 等真实英文单词若独占一行，Agent 即可放入 `omittedLineIds` 静默丢弃；校验层（`pdf_composer.rs` 的 `validate_layout`）只验「被省略行须带 removable 标志」，不验其真是页码。
- **失败场景**：正文恰好含此类短词独占行（诗歌、清单、代码注释）→ 被合法省略且校验不报错，正文静默缺失。
- **修复方向**：roman-numeral 行额外要求「数值落在该书页码区间」或「位于页面首/末几行」才判页码。
- **与 0.2.10 报告**：原 P1-3 收紧后**仍残留**的同类问题，原报告 P3 末尾亦提及，概率更低故定 P2，但因其直接破坏核心承诺，建议尽早处理。

### P2-2 图片下载不受同源范围约束（SSRF / 越界批量抓取） *新发现*

- **位置**：`generation.rs:3120` `localize_images`（仅校验 `http|https` scheme，未调 `enforce_source_scope`）
- **证据**：章节正文里 `<img src="https://internal.corp/secret.png">` 会被本机 curl 拉取落地；单图 64 MiB 上限，**无聚合上限**（一页 1000 张 ≈ 64 GiB 落盘）。与 CONTEXT.md「在线书籍来源 = 同源同路径」承诺不符。
- **修复方向**：对图片 URL 同样施加 `enforce_source_scope`，并加聚合字节数/数量上限。

### P2-3 SSE 消费者在 broadcast `Lagged` 时静默丢消息 ✅已亲验

- **位置**：`src-tauri/src/server.rs:727-730`
- **证据**：`BroadcastStream` 的 `Err(Lagged(n))` 在 `filter_map` 中被 `_ => None` 丢弃。`broadcast` 容量 256（`agent.rs:144`）。
- **失败场景**：高吞吐 delta 或前端短暂阻塞时，`running → completed/error` 等关键事件可能丢失，前端 SSE 停在「运行中」假状态，无重读快照信号。
- **修复方向**：`Err(Lagged)` 分支发一个「状态可能过期」事件或重发最新 snapshot（前端 `reader.ts:1176` 的 sequence 去重正好支持自愈）。

### P2-4 PDF + 翻译组合：静默只翻译标题或排版完成后才失败

- **位置**：`frontend/src/main.ts:808-811`（前端允许）→ `generation.rs:209-215`（无拦截）→ `generation.rs:866-877` + `3485-3525`（`chapter_blocks` 正则要求 `data-goodreader-block`，而 PDF 页面 HTML 只有 `data-source-page`）
- **失败场景**：英文 PDF + 翻译 → 只有标题/章节标题被翻译、正文不翻译，任务**成功**（静默部分翻译）；或用户把标题改中文 → blocks 为空 → 全部页面排版完成后才 `bail!`（数小时计算白费）。
- **修复方向**：`start_import` 对 `kind==Pdf && translate` 提前拒绝，或前端禁用该复选框。
- **与 0.2.10 报告**：原 P2-2，相关代码路径本次未变，结论保留。

### P2-5 大量 `.expect("…锁")`，任一持锁段 panic 即中毒级联瘫痪

- **位置**：遍布 `generation.rs`（`:189/288/318/319/380/1733/1885-1894`）、`agent.rs`（`:406/434/436/470/488/499/512/575/646/664/1089/1210/1512`）、`agent_session.rs`、`db.rs`
- **证据**：所有 `std::sync::Mutex` 的 `.lock().expect("…")`。持锁段 panic 即中毒，之后所有访问该表的调用 panic，级联到 Tauri 命令层。`run_task`（`generation.rs:410`）只捕获 `Result` 错误不捕获 panic。
- **失败场景**：一次 panic（未来改动、上游 JSON 在持锁路径出错、OOM）→ mutex 中毒 → 整个 `ImportManager`/`AgentCoordinator` 不可用。
- **修复方向**：关键路径 `lock().unwrap_or_else(|e| e.into_inner())` 主动恢复，或改用 `parking_lot::Mutex`（无中毒概念）。
- **定级说明**：当前持锁段多为 `Result` 传播，panic 概率低，属「系统性脆弱」而非现存 bug，定 P2。

### P2-6 全新安装默认字号钳制已修复，但后端 `save_setting` 仍无数值钳制 ✅已亲验

- **位置**：`src-tauri/src/server.rs:1186-1200`（只校验 `value.len() > 256`）、`db.rs:729-737`
- **证据**：白名单含 `reader-font-size`、`sidebar-width`、`ai-sidebar-width` 等数值 key，但后端不校验数值范围。前端 `settings.ts` 已做钳制防御，故前端路径安全；后端缺纵深防御。
- **修复方向**：`save_setting` 按 key 做语义校验（数值区间、枚举值），或在读出时对关键字段钳制。

### P2-7 无单实例锁，双实例并发写 SQLite

- **位置**：`src-tauri/src/lib.rs:16-109` 无 single-instance 处理，`db.rs:36-38` busy_timeout 5s
- **失败场景**：`open -n` 二次启动 → 两进程各持连接，进度互相覆盖、偶发 `SQLITE_BUSY` 500。
- **修复方向**：`tauri-plugin-single-instance` 或 flock/socket 锁。
- **与 0.2.10 报告**：原 P2-5，未变。

### P2-8 封面仅 magic bytes 校验、无像素尺寸上限，巨幅 PNG 可冻结 WebView

- **位置**：`src-tauri/src/server.rs:1268-1310`（`cover_image_format` 只查 4-12 字节魔数，`save_cover_override` 只限 32MB 不解码）
- **失败场景**：32MB 的巨幅 PNG 头（如 65535×65535）原样吐出，WebView 解码时内存暴涨。本地 DoS 级别（非 XSS，`nosniff` + `img-src 'self'` 已兜底）。
- **修复方向**：用 `image` crate 完整解码并校验像素上限（如 ≤8192×8192）。
- **与 0.2.10 报告**：原 P2-6，未变。

### P2-9 前端「#app 整体 aria-live」+ AI 流式高频全量重渲染 ✅已亲验

- **位置**：`frontend/index.html:11`（`<div id="app" aria-live="polite">`）；`frontend/src/reader.ts:1060-1072`（`updateAiStreamingMessage` 每次 `text_delta` 都 `markdown-it.render` 全量 + 整段 `innerHTML` 重建 + 强制滚动）
- **失败场景**：(a) 书架任何一次状态变化，整个侧栏+书格被屏幕阅读器整段朗读，可访问性严重劣化；(b) 长回答（数千字）下每个 token 触发 O(n) markdown 解析 + 整段重排，主线程阻塞，用户中途选区被下次刷新清空。
- **修复方向**：去掉 `#app` 的 `aria-live`（局部 live region 已足够）；对 `updateAiStreamingMessage` 加 requestAnimationFrame/时间节流（50-80ms）或仅追加增量 token。
- **与 0.2.10 报告**：原 P3 提及流式重渲染，本次因叠加可访问性问题提升为 P2。

---

## 五、P3 轻微问题（概述）

为控制篇幅，下列 P3 不逐条展开，多数与 0.2.10 报告一致，子代理另有补充：

- **生成/PDF**：事件追加为算 seq 全量重读 → O(n²)（`generation.rs:1733-1744`）；热路径正则每次重编（`pdf_source_lines:2067` 等，对比 `importer.rs` 已用 `OnceLock` 缓存）；`png_dimensions` 读整张 PNG 仅为取 24 字节头；翻译仅保护 `code/kbd/samp/img` 丢失链接与行内格式；`cancel` 与运行中任务并发删目录（TOCTOU）；PDF 全部页面 HTML 驻留内存（`MAX_PDF_PAGES=5000`）。
- **Agent 会话**：`broadcast(256)` 慢消费者丢 Provider 事件（`agent.rs:709`）；async 函数内大量同步阻塞 IO（写 events.jsonl、`capture_runtime_stream`）；`stderr` 缓冲无上限；Custom 运行时无沙箱（与 CONTEXT.md「任务能力策略…不含完全信任」措辞不符）；`find_executable` 硬编码回退目录（PATH 外也命中）；`prepare_workspace` 用 `chapter.id` 直接拼文件名（潜在路径穿越面）；`forced_error.expect` 隐式不变量。
- **DB/书库**：删除书籍副本不清理封面覆盖（`server.rs:918-932`，与 handoff 声明不符）；遗留 `status='running'` 问题任务无启动恢复（与导入任务侧不对称）；清除 AI 工作区/忘记书籍不删磁盘工作区目录（残留每任务数百 KB~数 MB）；`save_cover_override` 固定临时名竞态；`ai_messages.duration_ms` 在 retry 后偏大；`before-restore` 备份挤占配额。
- **前端**：SSE 持续 CONNECTING 无兜底刷新（`reader.ts:1147-1151`）；耗时展示依赖客户端时钟（`:1055`）；`closeBookMenu` 用全局 `[aria-expanded="true"]` 选择器可能误伤（`main.ts:1398`）；`main.ts`/`reader.ts` 核心契约类型与 `api()` 重复定义易漂移；reader.ts 启动失败仅 toast 无重试入口；`window.confirm` 与自定义模态风格分裂。
- **死代码**：`scripts/convert-rust-for-dummies.mjs`、`scripts/apply-rust-figure-images.mjs` 无调用方。

---

## 六、已验证无问题的关键路径

- **stopped 终态竞争**：`complete_agent_execution`/`pause_agent_execution` 的 `UPDATE` 均带 `WHERE status` 守卫（`db.rs:539-605`），迟到完成不能覆盖 stopped，且有回归测试（`db.rs:1101-1126`）。⚠️ 注意：守卫防的是「DB 终态被覆盖」，不防 P1-3 的「进程未被杀」。
- **跨书会话隔离**：`SessionKey = (book_id, runtime_id)`（`agent_session.rs:25-29`），每 slot 独立 `AsyncMutex`。
- **进程组回收**：`process_group(0)` + `kill_on_drop(true)`，冷启动 `cleanup_recorded_processes_under` 扫 `process.pid` 回收孤儿。
- **XSS 与危险导航**：markdown-it `html:false` + `validateLink` 拦 `javascript:/vbscript:/file:` + citation 正则白名单 + `escapeHtml` + `bindExternalLinks` 协议白名单 + 后端 `open_external` 二次校验；书籍页 CSP 用 nonce。
- **路径穿越**：`resolve_package_file` canonicalize + `starts_with`；`book_id` 强制 `Uuid::parse_str`；备份名单组件校验。
- **正文块账本**：`validate_layout` 逐行核对（不可移除行必须且只能出现一次、figure 在页面边界内）+ `validate_translation_map`（key 集合 + 受保护片段占位符**多重集**一致，尊重目标语言语序），有测试覆盖。
- **会话认证**：127.0.0.1 随机端口 + 64 位随机 token + HttpOnly/SameSite=Strict cookie + 非 GET Origin 校验 + JSON Content-Type 强制 + `secure_eq` 常数时间比较。
- **标注完整性**：同一锁内完成「去重 → 重叠检测 → 插入」，DB 有唯一索引 + CHECK 约束双保险。

---

## 七、建议的处理顺序

1. **发布前必修（P1）**：P1-1（Chrome 超时，会让制书永久卡死）、P1-2（工作区并发覆盖，数据错配）、P1-3（停止/终止链路）、P1-4（dispose 阻塞）。P1-3 的「统一 terminate 实现 + 工作区按 task 隔离」可同时消解 P1-2/P1-4 的部分症状。
2. **尽快补**：P2-1（页码正则）、P2-2（图片越界抓取）、P2-3（SSE Lagged）、P2-4（PDF+翻译）。
3. **近期**：P2-5（锁中毒）、P2-6（设置钳制纵深）、P2-7（单实例）、P2-8（封面解码）、P2-9（aria-live + 流式节流）。
4. **结构债**：P1-5（迁移框架）虽不阻塞当前发布，但下次 schema 变更前必须补，否则即时升级为 P0。
5. P3 各项按性价比择机；其中「删除副本不清理封面」「遗留 running 任务无恢复」与文档承诺不一致，建议优先对齐。

## 八、遗留风险与未验证项

- P2-1（页码正则误判）、P2-2（图片越界）、P2-8（封面解码）的真实触发率依赖真实 PDF / 在线来源验证，建议发布前用含旋转页、跨页表格、巨幅封面的 fixture 补一次集成验证。
- 对 Codex `app-server`、Claude `stream-json`、OpenCode 事件格式的判断基于代码与本地 fake-CLI 测试，未连接真实 CLI 验证协议细节。
- 本报告未对 `sample-books/`、`site/`、`tools/`、`design*/` 做逐文件审查（非核心代码路径）。

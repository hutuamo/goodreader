# GoodReader 0.2.10 工作区代码检视报告

- **检视时间**：2026-08-07
- **检视者**：codewhale 审查 Agent（deepseek-v4-flash，只读审查，未修改/暂存/提交任何文件）
- **基线**：`df1ca14`（`origin/main`）
- **检视对象**：当前未提交工作区（20 个已跟踪修改 + 12 个未跟踪新增文件）
- **产品版本**：0.2.10 | macOS 13+ / Apple Silicon / Tauri 2 + Rust + TypeScript
- **方法**：人工通读核心文件（`agent_session.rs` 全文、`pdf_composer.rs` 全文、`agent.rs`/`db.rs`/`server.rs`/`generation.rs`/`lib.rs`/`models.rs` 关键路径、`reader.ts`/`main.ts` 关键路径）+ 4 个并行只读子代理深度审查（Agent 会话链路、PDF 排版链路、数据库/书库链路、前端/安全链路），关键断言均经调用链亲验。
- **结论**：P0 无；P1 3 项；P2 9 项；P3 若干。`cargo test` 64 通过 / 0 失败 / 4 ignored（本环境复跑），8 个 npm 检查脚本全部通过。

---

## 一、项目理解

**GoodReader** 是一个 macOS 本地书架与统一阅读器（Tauri 2：Rust 后端 + TypeScript 前端，约 1.8 万行代码）。核心思路：

- **书籍 = 不可变静态 HTML 包**（`book.json` 是唯一权威），阅读进度/高亮/笔记/书签全部外置到 SQLite；
- **制书链路**：PDF / 本地 HTML / 在线链接 → 来源快照 → 确定性转换器 →（可选）本机 Agent 翻译/排版 → 契约校验 → 入架；
- **0.2.10 本轮改动**：
  1. 深度 Agent 会话集成（`agent_session.rs` 新增，Codex/Claude/OpenCode 原生持久会话 + SSE 流式 + 停止/重试）；
  2. PDF 每页由 Agent 排版（`pdf_composer.rs` 新增，行账本 + 裁图 + 检查点，替换旧脚本方案）；
  3. PDF 预检三模式（auto / text-layer / ocr）、选区问 AI、阅读显示设置（字号/侧栏宽度）、替换封面、AI 工作区 UI。

架构上安全设计相当认真：书籍页 CSP 用 nonce 注入、会话 token 绑定 loopback、路径穿越全面防护、stopped 终态有数据库守卫。问题集中在"停止/并发竞争"、"正文完整性启发式软肋"、"新功能组合未验证"三处。

---

## 二、P1 严重问题（3 项）

### P1-1 停止请求对 Cursor/自定义 CLI 的问题进程完全不生效 *已亲验*

- **位置**：`src-tauri/src/agent.rs:466-489`（`stop_question`）、`:550-551`（legacy 分支）、`:1069-1074`（pid 登记键）
- **证据链**：`stop_question` 先写 DB `stopped`（成功），然后 `control.cancel()`——但 legacy 路径的 `run_runtime` **从不订阅** cancel watch，是空操作；随后按 `tasks_dir.join(task_id)` 查 `active_processes`——而 legacy 进程的登记键是 `prepare_workspace` 返回的 `tasks_dir/Sessions/<hex(book_id)>/workspace`（`agent.rs:745-755, 1069-1074`），**键不匹配，杀不到进程**。
- **失败场景**：用 Cursor 或自定义 CLI 提问 → 点"停止请求" → UI 显示已停止，但进程继续运行最长 30 分钟（`EXECUTION_TIMEOUT`），期间仍可写共享工作区（自定义 CLI 无沙箱参数），污染后续 turn 输入；`active_processes` 条目泄漏。
- **测试为何没覆盖**：`scripts/check-ai-stop.mjs` 只是前端字符串检查；`agent.rs:1848-1898` 的测试直接调 `terminate_process_group(pid)`，绕过了 `stop_question` 的查表逻辑。
- **修复**：`stop_question` 改为按 `task.book_id` 计算 `Sessions/<hex>/workspace` 键查表；legacy 路径传入 `ExecutionControl` 并在执行循环中加 cancel 检查。回归测试：fake `sleep 30` 脚本走完整 start→stop，断言秒级退出。

### P1-2 同书并发问题任务存在 TOCTOU + 共享工作区覆盖竞态 *已亲验*

- **位置**：`src-tauri/src/server.rs:678-687`（检查与创建非原子）、`src-tauri/src/db.rs:431-449`（`create_question_task` 无并发约束）、`src-tauri/src/agent.rs:738-818`（`prepare_workspace` 对同书所有任务覆盖写同一目录）
- **失败场景**：两个 `POST /api/books/:id/ai/questions` 几乎同时通过 `active_agent_tasks` 空检查（TOCTOU 窗口），或"停止旧任务后立刻发新问题"（旧 turn 尚未结束）。后启动的任务在**取会话锁之前**就覆盖了共享的 `Sessions/<book>/workspace/context/current.md`——旧 turn 的 Provider 若尚未读取或重试读取，回答的是新问题；旧任务若未及时收到停止，其 `complete_agent_execution` 会把"问题 A + 回答 B"写入 `ai_messages`（同书不同 runtime 时完全无锁，更易触发）。
- **影响**：AI 历史问题-回答静默错配，不可察觉的数据损坏。
- **测试为何没覆盖**：无任何并发/HTTP 集成测试，Rust 测试单线程。
- **修复**：在 `agent_tasks` 上加部分唯一索引（`book_id` + `status NOT IN ('completed','stopped')`），并把 `prepare_workspace` + 会话执行放入按 book_id 键控的异步锁。回归测试：并发双 POST 断言只有一个任务进入 queued/running。

### P1-3 PDF 重复行启发式可能把跨页正文标为可移除，正文可静默省略 *已亲验*

- **位置**：`src-tauri/src/generation.rs:2030-2052`（`repeated_pdf_lines`）、`:2054-2069`（`pdf_source_lines`）、`src-tauri/src/pdf_composer.rs:210-219`（omitted 校验只查 removable 标志）
- **证据**：任何出现在 ≥3 页**每页前 3 行或后 3 行**的归一化文本行都会被标 `removable=true`；校验层只验证"被省略的行必须带 removable 标志"，**不验证它真的是页眉/页脚**。
- **失败场景**：跨 3+ 页长表格的重复表头（"序号 项目 数量"）、双栏书中每页顶部重复的栏目名/章节口号、习题册每页顶部的题号。Agent 按指令（`pdf_composer.rs:151` 允许 omit removable 行）将其省略 → 校验通过 → 正文静默消失。这是整条链路上唯一能在**不失败**的前提下破坏"正文不可变"承诺的机制。
- **测试为何没覆盖**：`pdf_composer.rs:414-449` 只测"不可移除行被漏"；`repeated_pdf_lines` 本身无单测。
- **修复**：收紧启发式（只取每页第 1 行/最后 1 行、短行 ≤40 字符、出现于 ≥80% 页数），并把"removable 行被省略"写入任务事件供事后审计。回归测试：构造 3 页每页顶部同一正文行的页面序列，断言不被标 removable。

---

## 三、P2 中等问题（9 项）

### P2-1 全新安装默认字号被钳到 80% *已亲验*
- `frontend/src/reader.ts:2127-2132` + `src-tauri/src/server.rs:1177-1185` + `db.rs:719-730`。设置未写入时 `loadPreference` 返回 `null`，`Number(null) === 0` 且 `Number.isFinite(0)` 为真 → `clampNumber(0, 80, 160) = 80`。侧栏宽度同理被置 0。新用户（或恢复不含新 key 的旧备份）首启字号 80%、侧栏取最小宽度。
- **修复**：解析前排除 `null`/`""`；回归测试补 `null`/`""`/`"abc"`/`"-50"`/`"1e999"` 用例。

### P2-2 PDF+翻译组合：前端允许、后端不拦截，排版完成后**静默只翻译标题**或失败 *已亲验，比子代理结论更严重*
- `frontend/src/main.ts:808-811`（非中文 PDF 且 Agent 可用时翻译复选框可用）→ `generation.rs:209-215`（只校验 runtime 必选，无 PDF+translate 拦截）→ `generation.rs:866-877` + `:3485-3525`（`chapter_blocks` 正则严格要求 `data-goodreader-block`，而 PDF 页面 HTML 只有 `data-source-page`）。
- 结果：英文标题 PDF → 只有标题/章节标题被翻译、正文不翻译，任务**成功**（静默部分翻译）；用户把标题改成中文 → `blocks` 为空 → 在**全部页面排版完成之后**才 `bail!`（数小时计算白费）。
- **修复**：后端 `start_import` 对 `kind==Pdf && translate` 提前拒绝，或前端禁用该复选框。

### P2-3 零文本页空 blocks 可通过校验，产生静默空白页 *已亲验*
- `src-tauri/src/pdf_composer.rs:230-232`：`if layout.blocks.is_empty() && !source.lines.is_empty()` 才拒绝。纯矢量整页插图页（无文本层、`pdfimages` 无输出）在 auto 模式下预检放行，Agent 返回空 blocks → 校验通过 → 书中出现空白 `<section class="pdf-page">`。
- **修复**：空 blocks 一律拒绝（真正空白页应由上游跳过而非静默通过）。

### P2-4 恢复更高版本备份会先覆盖活动库、后报错 *已亲验*
- `src-tauri/src/db.rs:794-813`：`restore_backup` 先 `create_backup("before-restore")` → `run_to_completion` 把备份**写入活动连接** → 最后 `initialize()` 在 `db.rs:51-53` 因 `user_version > SCHEMA_VERSION` bail。活动库已被未来 schema 覆盖，应用持续报错，用户只能手工处理。
- **修复**：覆盖前先只读打开源备份检查 `user_version`。

### P2-5 无单实例锁，双实例并发写 SQLite *已亲验*
- `src-tauri/src/lib.rs:16-109` 无 single-instance 处理，`db.rs:36-38` busy_timeout 5 秒。`open -n` 二次启动 → 两个进程各自持有连接，进度互相覆盖、偶发 SQLITE_BUSY 500。
- **修复**：`tauri-plugin-single-instance` 或 flock/socket 锁。

### P2-6 封面仅魔数校验、无像素尺寸上限，解压炸弹可冻结 WebView *已亲验*
- `src-tauri/src/server.rs:1268-1310`：`cover_image_format` 只查 4-12 字节魔数，`save_cover_override` 只限 32MB 大小、不解码。32MB 的巨幅 PNG 头（如 65535×65535）会被原样吐出，WebView 解码时内存暴涨。非 XSS（`nosniff` + `img-src 'self'` 已兜底），本地 DoS 级别。
- **修复**：用 `image` crate 完整解码并校验像素上限（如 ≤8192×8192）。

### P2-7 清除 AI 工作区在 turn 进行中阻塞最长 30 分钟 *已亲验*
- `src-tauri/src/agent_session.rs:281-296`（`dispose_book` 对每个 slot `backend.lock().await.dispose().await`）+ `src-tauri/src/server.rs:796-807`。turn 正在执行时 slot 锁被持有到 turn 结束（最长 `TURN_TIMEOUT` 30 分钟），清除请求挂起，前端无感知。
- **修复**：dispose 前先对该书全部 active control 调 `cancel()`。

### P2-8 SSE broadcast Lagged 时增量被静默丢弃 *已亲验*
- `src-tauri/src/server.rs:727-730` + `agent.rs:130`（capacity 256）。`BroadcastStream` 的 `Err(Lagged(n))` 在 `filter_map` 中被 `_ => None` 丢弃，高吞吐 delta 或客户端短暂阻塞时流式文本中途截断；若任务以 paused/error 结束，截断成为永久状态。
- **修复**：Lagged 时改发一个最新 Snapshot（前端 `reader.ts:1178` 的 sequence 去重正好支持自愈）。

### P2-9 旋转页裁区坐标语义未验证，且裁图输出从不校验尺寸
- `src-tauri/src/generation.rs:2091-2124`（`render_pdf_region` 只查退出码和文件存在）+ `pdf_composer.rs:196-205`。含 `/Rotate 90/270` 的页面（扫描书常见）上，`pdftoppm -x/-y/-W/-H` 的坐标语义跨 poppler 版本不一致，错位裁图不可检测。
- **修复**：裁图后读取输出 PNG 尺寸断言等于 crop 尺寸；补旋转页 fixture 测试。

### P2-10 全部 8 个前端检查脚本是字符串存在性检查，防御为零 *已亲验*
- `scripts/check-*.mjs` 全部是 `source.includes(marker)` 断言。具体缺陷：`check-reader-selection-ai.mjs:21-28` 用 `slice(indexOf("function askAiAboutSelection"), indexOf("async function updateParallelButton"))` 做负向断言——两个函数相对顺序一变，slice 为空串，**负向断言恒真**。正是这类脚本漏掉了 P2-1。
- **修复**：把解析/clamp/escape 抽成纯函数用 `node:test` 做真实行为测试。

---

## 四、P3 轻微问题（合并列出）

**数据库/书库链路**：
- 删除书籍副本不清理封面覆盖（`server.rs:918-932`，与 handoff §2.5 声明不符）；
- 备份/恢复在 async handler 内同步执行且全程持有全局 DB 锁（`db.rs:739-753` + `server.rs:1157-1175`），备份期间 UI 卡死数秒；
- 遗留 `status='running'` 的问题任务无启动恢复（`agent.rs:127-140`），崩溃后 UI 永远显示"处理中"（导入任务侧有 `recover_interrupted_tasks`，两边不对称）；
- 清除 AI 工作区/忘记书籍不删除磁盘工作区目录（`Sessions/<hex>`、`AgentTasks/<task_id>` 残留，每任务数百 KB~数 MB）；
- `save_cover_override` 用固定 `{book_id}.tmp` 临时名（`server.rs:1301-1308`），连续替换封面有竞态；
- 迁移机制仅 `CREATE TABLE IF NOT EXISTS` 无版本分步迁移与事务包裹（`db.rs:48-151`），本次 v2→v3 安全，属结构风险。

**PDF 链路**：
- `pdfimages` 失败被 `unwrap_or_default()` 静默吞掉，漏图检测整体失效（`generation.rs:755, 1627`）；
- 检查点不绑定 Agent 运行时与指令版本（`pdf_composer.rs:130-132`），切换 Agent 复用旧版式且无"强制全量重排"入口；
- `rendered-page.png` 缓存无版本戳（`generation.rs:763-766`）；
- 失败任务不清理 preflight token 快照目录（`generation.rs:398-403`）；
- 页码正则 `(?i)^(?:[0-9]+|[ivxlcdm]+)$` 会把代码清单中的裸数字行/诗行编号标为 removable（`generation.rs:2055, 2065`，与 P1-3 同类、概率更低）；
- `_caption` 死字段（`pdf_composer.rs:85-86`），Agent 提交的 caption 从不生效；
- PDF 书无 `data-goodreader-block`，块级高亮/书签/原文对 PDF 书不可用（已知边界，建议导入向导明示）；
- 每页写一次 task.json + 事件追加，5000 页上限下写入与轮询放大。

**前端链路**：
- 流式期间 `#grAiTask` 整体 innerHTML 重渲染（`reader.ts:1168-1172, 1191-1193`），正在点停止的用户交互可能被打断，`aria-live` 整段重播；
- SSE 持续 CONNECTING 无兜底刷新（`reader.ts:1149-1153`），后端进程被杀后任务状态卡死直到用户重开侧栏；
- 小窗口拖拽会持久化已被限幅的值（`reader.ts:442-448`）；
- 耗时展示依赖客户端时钟（`reader.ts:1057`）。

**死代码**：`scripts/convert-rust-for-dummies.mjs`、`scripts/apply-rust-figure-images.mjs` 在仓库中无任何调用方，可随本轮删除。

---

## 五、已验证无问题的关键路径（检视结论）

- **stopped 终态竞争**：`complete_agent_execution`/`pause_agent_execution` 的 UPDATE 均带 `WHERE status` 守卫（`db.rs:539-605`），迟到完成不能覆盖 stopped 也不会插入 ai_message；写序"先 DB 置终态、再杀进程"无反向窗口；`db.rs:1101-1126` 有迟到完成/迟到失败回归测试。
- **跨书会话隔离**：`SessionKey = (book_id, runtime_id)`（`agent_session.rs:25-29`），每 slot 独立 `AsyncMutex` 串行化 turn；`dispose_book` 按书移除。
- **进程回收**：`prepare_command` 设 `process_group(0)` + `kill_on_drop(true)`，`terminate()` 走进程组终止（`agent_session.rs:1093-1108`），应用退出时 native 进程有回收路径。
- **XSS 与危险导航**（逐层亲验）：markdown-it `html:false` + v15 默认 `validateLink` 拦 `javascript:/vbscript:/file:` + citation 正则白名单 `[A-Za-z0-9._:-]` + `escapeHtml` 输出 + `bindExternalLinks` 协议白名单 + 后端 `open_external` 二次校验；书籍页 `book_csp` 用 `script-src 'nonce-*'`，书籍自有脚本无法执行。
- **路径穿越**：`resolve_package_file` canonicalize + `starts_with(root)`；book_id 强制 `Uuid::parse_str`（`library.rs:120`）；备份名校验只允许单组件文件名。
- **行账本核心**：遗漏行硬拒绝、重复引用拒绝、负坐标/溢出用 `saturating_add` 防御、`requires_figure` 页强制 figure、`deny_unknown_fields` 严格解析——这些都有单测且逻辑正确。
- **会话认证**：127.0.0.1 随机端口 + 64 位随机 token + HttpOnly/SameSite=Strict cookie + 非 GET 请求 Origin 校验 + JSON Content-Type 强制 + `secure_eq` 常数时间比较。
- **数据库迁移 v2→v3**：纯新增独立表，老库打开即建表，安全；新 preflight 字段全部 `#[serde(default)]`，旧数据可解析。

---

## 六、验证记录（本环境复跑）

- `cargo test --test-threads=1`：**64 通过、0 失败、4 ignored**（与 handoff 记录一致）；
- 8 个 `npm run test:*` 检查脚本全部通过（注意：它们只是字符串检查，见 P2-10）。

## 七、建议的处理顺序

1. **发布前必修**：P1-1（停止无效）、P1-2（并发错配）、P1-3（正文静默省略）、P2-1（新用户字号 80%）、P2-2（PDF+翻译静默部分翻译）——前两项是行为错误，后三项是数据/体验正确性问题，修复成本都很低；
2. **尽快补**：P2-4（恢复备份）、P2-3（空白页）、P2-8（SSE 丢增量）；
3. **近期**：P2-5 单实例、P2-6 封面解码、P2-7 清除阻塞、P2-9 旋转页、P2-10 测试门禁升级；
4. P3 各项按性价比择机处理，其中"删除副本不清理封面""遗留 running 任务无恢复"与文档承诺不一致，建议优先对齐。

## 八、遗留风险

- 旋转页裁区（P2-9）与 `repeated_pdf_lines` 的实际误判率依赖真实 poppler/PDF 验证，建议在发布前用含旋转页和跨页表格的 fixture 补一次集成验证；
- Agent 会话子代理报告的 P3 尾部细节未完整取回（不影响 P1/P2 结论，其核心问题均已亲验）；
- 本报告对 Codex `app-server`、Claude `stream-json`、OpenCode 事件格式的判断基于代码与本地 fake-CLI 测试，未连接真实 CLI 验证协议细节。

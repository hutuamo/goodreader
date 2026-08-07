use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;

use crate::agent_session::{
    AgentSessionHost, ExecutionControl, ProviderExecutionEvent, ProviderKind, SessionConfig,
    SessionKey,
};
use crate::db::Database;
use crate::library::resolve_package_file;
use crate::models::{
    AgentRuntime, AgentRuntimeCapabilities, AgentTask, AiMessage, Annotation, BookPackage,
    CustomAgentRuntime,
};

const EXECUTION_TIMEOUT: Duration = Duration::from_secs(60 * 30);
const NETWORK_FAILURE_TIMEOUT: Duration = Duration::from_secs(90);
const TRANSLATION_EXECUTION_TIMEOUT: Duration = Duration::from_secs(60 * 8);
const MAX_INVALID_STRUCTURED_OUTPUTS: usize = 2;

#[derive(Clone)]
pub struct AgentCoordinator {
    tasks_dir: PathBuf,
    database: Arc<Database>,
    active_processes: Arc<Mutex<HashMap<PathBuf, u32>>>,
    active_questions: Arc<Mutex<HashMap<String, ExecutionControl>>>,
    live_tasks: Arc<Mutex<HashMap<String, LiveTaskState>>>,
    sessions: AgentSessionHost,
    task_updates: broadcast::Sender<AgentTaskStreamEvent>,
}

#[derive(Clone, Default)]
struct LiveTaskState {
    phase: Option<String>,
    partial_output: String,
    stream_sequence: u64,
    execution_id: Option<String>,
    turn_id: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AgentTaskStreamEvent {
    Snapshot {
        task: AgentTask,
    },
    Provider {
        task_id: String,
        event: ProviderExecutionEvent,
    },
}

impl AgentTaskStreamEvent {
    pub fn task_id(&self) -> &str {
        match self {
            Self::Snapshot { task } => &task.id,
            Self::Provider { task_id, .. } => task_id,
        }
    }
}

#[derive(Clone)]
enum RuntimeKind {
    Codex,
    Claude,
    OpenCode,
    Cursor,
    Custom,
}

#[derive(Clone)]
struct RuntimeSpec {
    id: String,
    executable: PathBuf,
    arguments: Vec<String>,
    kind: RuntimeKind,
}

struct RuntimeOutput {
    answer: String,
    stdout: String,
    stderr: String,
}

pub struct TranslationRun {
    pub translations: BTreeMap<String, String>,
    pub answer: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub elapsed_ms: u64,
}

#[derive(Deserialize)]
struct StructuredTranslation {
    translations: BTreeMap<String, String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskFile<'a> {
    schema_version: u32,
    task_id: &'a str,
    book_id: &'a str,
    kind: &'a str,
    goal: &'a str,
    runtime_id: &'a str,
    output_directory: &'a str,
}

impl AgentCoordinator {
    pub fn new(tasks_dir: PathBuf, database: Arc<Database>) -> Result<Self> {
        fs::create_dir_all(&tasks_dir)
            .with_context(|| format!("无法创建 Agent 任务目录 {}", tasks_dir.display()))?;
        let (task_updates, _) = broadcast::channel(256);
        Ok(Self {
            tasks_dir,
            database,
            active_processes: Arc::new(Mutex::new(HashMap::new())),
            active_questions: Arc::new(Mutex::new(HashMap::new())),
            live_tasks: Arc::new(Mutex::new(HashMap::new())),
            sessions: AgentSessionHost::new(),
            task_updates,
        })
    }

    pub fn subscribe_task_updates(&self) -> broadcast::Receiver<AgentTaskStreamEvent> {
        self.task_updates.subscribe()
    }

    pub async fn dispose_book_sessions(&self, book_id: &str) {
        self.sessions.dispose_book(book_id).await;
    }

    pub async fn runtimes(&self) -> Result<Vec<AgentRuntime>> {
        let mut runtimes = Vec::new();
        for (id, name, executable_name, kind) in [
            ("codex", "Codex", "codex", RuntimeKind::Codex),
            ("claude", "Claude Code", "claude", RuntimeKind::Claude),
            ("cursor", "Cursor Agent", "cursor", RuntimeKind::Cursor),
        ] {
            runtimes.push(probe_builtin(id, name, executable_name, kind).await);
        }

        for custom in self.database.custom_agent_runtimes()? {
            let path = PathBuf::from(&custom.executable);
            let available = is_executable_file(&path);
            let kind = custom_runtime_kind(&path);
            let version = if available && matches!(kind, RuntimeKind::OpenCode) {
                probe_version(&path, &kind).await.ok()
            } else {
                None
            };
            runtimes.push(AgentRuntime {
                id: custom.id,
                name: custom.name,
                executable: Some(custom.executable),
                available,
                version,
                detail: (!available).then(|| "找不到可执行文件".to_string()),
                built_in: false,
                capabilities: if matches!(kind, RuntimeKind::OpenCode) {
                    builtin_capabilities(&kind)
                } else {
                    AgentRuntimeCapabilities {
                        streaming: false,
                        native_resume: false,
                        structured_output: false,
                        permission_mapping: false,
                        tool_use: false,
                    }
                },
            });
        }
        Ok(runtimes)
    }

    pub async fn run_generation(
        &self,
        runtime_id: &str,
        workspace: &Path,
        instruction: &str,
    ) -> Result<()> {
        let runtime = self.runtime_spec(runtime_id)?;
        let logs = workspace.join("logs");
        fs::create_dir_all(&logs)?;
        run_runtime_instruction(
            &runtime,
            workspace,
            instruction,
            true,
            Some(&logs),
            Some(&self.active_processes),
        )
        .await?;
        Ok(())
    }

    pub async fn run_translation(
        &self,
        runtime_id: &str,
        workspace: &Path,
        blocks: &BTreeMap<String, String>,
        source_language: &str,
    ) -> Result<TranslationRun> {
        let runtime = self.runtime_spec(runtime_id)?;
        if matches!(
            runtime.kind,
            RuntimeKind::Custom | RuntimeKind::Cursor | RuntimeKind::OpenCode
        ) {
            let instruction = format!(
                "请读取 input/blocks.json，{}\n把最终 JSON 对象写入 output/translations.json；只修改这个输出文件。",
                translation_instruction(source_language)
            );
            let started = std::time::Instant::now();
            let mut execution = Box::pin(self.run_generation(runtime_id, workspace, &instruction));
            let translations = loop {
                tokio::select! {
                    result = &mut execution => {
                        result?;
                        break read_translation_output(workspace)?;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(120)) => {
                        let Ok(translations) = read_translation_output(workspace) else {
                            continue;
                        };
                        if translations.keys().eq(blocks.keys()) {
                            if tokio::time::timeout(Duration::from_millis(750), &mut execution).await.is_err() {
                                self.cancel_generations_under(workspace);
                                let _ = execution.await;
                            }
                            break translations;
                        }
                    }
                }
            };
            let answer = fs::read_to_string(workspace.join("logs/stdout.log"))
                .unwrap_or_default()
                .trim()
                .to_string();
            return Ok(TranslationRun {
                translations,
                answer,
                session_id: None,
                model: None,
                elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            });
        }

        let logs = workspace.join("logs");
        let input = workspace.join("input");
        let output = workspace.join("output");
        fs::create_dir_all(&logs)?;
        fs::create_dir_all(&input)?;
        fs::create_dir_all(&output)?;
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "translations": {
                    "type": "object",
                    "additionalProperties": { "type": "string" }
                }
            },
            "required": ["translations"],
            "additionalProperties": false
        });
        let schema_text = serde_json::to_string(&schema)?;
        let schema_path = input.join("translation-schema.json");
        fs::write(&schema_path, serde_json::to_vec_pretty(&schema)?)?;
        let result_path = output.join("agent-result.json");
        let instruction = format!(
            "{}\n\n以下是必须逐项翻译的正文块 JSON。只返回符合给定 Schema 的结果，不要解释。\n{}",
            translation_instruction(source_language),
            serde_json::to_string(blocks)?
        );

        let mut command = Command::new(&runtime.executable);
        command.current_dir(workspace);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.kill_on_drop(true);
        let uses_stdin = match runtime.kind {
            RuntimeKind::Claude => {
                command.args([
                    "-p",
                    "--safe-mode",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--no-session-persistence",
                    "--permission-mode",
                    "dontAsk",
                    "--effort",
                    "low",
                    "--system-prompt",
                    "你是翻译执行器。逐项忠实翻译，严格保留键和受保护标记，只输出 Schema 要求的 JSON。不要解释，不要调用工具。",
                    "--tools",
                    "",
                    "--json-schema",
                ]);
                command.arg(schema_text);
                true
            }
            RuntimeKind::Codex => {
                command.args([
                    "exec",
                    "--skip-git-repo-check",
                    "--ephemeral",
                    "--ignore-rules",
                    "--sandbox",
                    "read-only",
                    "--json",
                    "--output-schema",
                ]);
                command.arg(&schema_path);
                command.arg("--output-last-message");
                command.arg(&result_path);
                command.arg("-C");
                command.arg(workspace);
                command.arg("-");
                true
            }
            RuntimeKind::OpenCode | RuntimeKind::Cursor | RuntimeKind::Custom => false,
        };
        let started = std::time::Instant::now();
        let runtime_output = if matches!(runtime.kind, RuntimeKind::Claude) {
            execute_claude_translation_command(
                command,
                workspace,
                &instruction,
                &logs,
                &self.active_processes,
            )
            .await?
        } else {
            execute_command(
                command,
                workspace,
                &instruction,
                uses_stdin,
                Some(&logs),
                Some(&self.active_processes),
            )
            .await?
        };
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (structured, session_id, model, answer) = match runtime.kind {
            RuntimeKind::Claude => parse_claude_translation_stream(&runtime_output.stdout)?,
            RuntimeKind::Codex => {
                let value =
                    fs::read_to_string(&result_path).context("Codex 没有生成结构化翻译结果")?;
                let structured = serde_json::from_str::<StructuredTranslation>(&value)
                    .context("Codex 翻译结果不符合结构化协议")?;
                (
                    structured,
                    parse_codex_session_id(&runtime_output.stdout),
                    parse_runtime_model(&runtime_output.stdout),
                    value,
                )
            }
            RuntimeKind::OpenCode | RuntimeKind::Cursor | RuntimeKind::Custom => unreachable!(),
        };
        write_json(&output.join("translations.json"), &structured.translations)?;
        Ok(TranslationRun {
            translations: structured.translations,
            answer,
            session_id,
            model,
            elapsed_ms,
        })
    }

    pub fn cancel_generations_under(&self, root: &Path) {
        let processes = self.active_processes.lock().expect("Agent 活动进程锁");
        for (workspace, pid) in processes.iter() {
            if workspace.starts_with(root) {
                terminate_process_group(*pid);
            }
        }
    }

    pub fn cleanup_recorded_processes_under(root: &Path) {
        for path in collect_process_files(root) {
            if let Ok(value) = fs::read_to_string(&path) {
                if let Ok(pid) = value.trim().parse::<u32>() {
                    terminate_process_group(pid);
                }
            }
            let _ = fs::remove_file(path);
        }
    }

    pub fn start_question(
        self: &Arc<Self>,
        package: BookPackage,
        annotations: Vec<Annotation>,
        task: AgentTask,
    ) {
        let control = ExecutionControl::new();
        self.active_questions
            .lock()
            .expect("Agent 问题控制锁")
            .insert(task.id.clone(), control.clone());
        self.live_tasks.lock().expect("Agent 实时状态锁").insert(
            task.id.clone(),
            LiveTaskState {
                phase: Some("等待 Agent 启动".to_string()),
                partial_output: String::new(),
                stream_sequence: 0,
                execution_id: None,
                turn_id: None,
            },
        );
        self.publish_task(&task.id);
        let coordinator = self.clone();
        tokio::spawn(async move {
            if let Err(error) = coordinator
                .execute_question(&package, &annotations, &task, &control)
                .await
            {
                if !coordinator
                    .database
                    .agent_task(&task.id)
                    .ok()
                    .flatten()
                    .is_some_and(|current| current.status == "stopped")
                {
                    let _ = coordinator.database.pause_agent_execution(
                        &task.id,
                        None,
                        &friendly_error(&error),
                    );
                }
            }
            coordinator.publish_task(&task.id);
            coordinator
                .active_questions
                .lock()
                .expect("Agent 问题控制锁")
                .remove(&task.id);
            coordinator
                .live_tasks
                .lock()
                .expect("Agent 实时状态锁")
                .remove(&task.id);
            coordinator.publish_task(&task.id);
        });
    }

    pub fn stop_question(&self, task_id: &str) -> Result<AgentTask> {
        let task = self.database.stop_agent_task(task_id)?;
        if let Some(control) = self
            .active_questions
            .lock()
            .expect("Agent 问题控制锁")
            .get(task_id)
            .cloned()
        {
            control.cancel();
        }
        let workspace = self.tasks_dir.join(task_id);
        if let Some(pid) = self
            .active_processes
            .lock()
            .expect("Agent 活动进程锁")
            .get(&workspace)
            .copied()
        {
            terminate_process_group(pid);
        }
        self.publish_task(task_id);
        Ok(task)
    }

    pub fn decorate_task(&self, mut task: AgentTask) -> AgentTask {
        if let Some(live) = self
            .live_tasks
            .lock()
            .expect("Agent 实时状态锁")
            .get(&task.id)
            .cloned()
        {
            task.phase = live.phase;
            task.partial_output = (!live.partial_output.is_empty()).then_some(live.partial_output);
            task.stream_sequence = Some(live.stream_sequence);
            task.execution_id = live.execution_id;
            task.turn_id = live.turn_id;
        }
        task
    }

    pub fn decorate_tasks(&self, tasks: Vec<AgentTask>) -> Vec<AgentTask> {
        tasks
            .into_iter()
            .map(|task| self.decorate_task(task))
            .collect()
    }

    fn publish_task(&self, task_id: &str) {
        let Ok(Some(task)) = self.database.agent_task(task_id) else {
            return;
        };
        let _ = self.task_updates.send(AgentTaskStreamEvent::Snapshot {
            task: self.decorate_task(task),
        });
    }

    async fn execute_question(
        &self,
        package: &BookPackage,
        annotations: &[Annotation],
        task: &AgentTask,
        control: &ExecutionControl,
    ) -> Result<()> {
        let runtime = self.runtime_spec(&task.current_runtime_id)?;
        let execution_id = self.database.start_agent_execution(&task.id, &runtime.id)?;

        let result = async {
            let history = self.database.ai_messages(&task.book_id)?;
            let workspace = self.prepare_workspace(package, annotations, task, &history)?;
            if self
                .database
                .agent_task(&task.id)?
                .is_some_and(|current| current.status == "stopped")
            {
                bail!("Agent 任务已停止");
            }
            let output = if matches!(
                runtime.kind,
                RuntimeKind::Codex | RuntimeKind::Claude | RuntimeKind::OpenCode
            ) {
                self.run_native_question(&runtime, task, &workspace, control)
                    .await?
            } else {
                let output = run_runtime(&runtime, &workspace, &self.active_processes).await?;
                if let Some(live) = self
                    .live_tasks
                    .lock()
                    .expect("Agent 实时状态锁")
                    .get_mut(&task.id)
                {
                    live.partial_output = output.answer.clone();
                }
                output
            };
            write_execution_log(&workspace, &execution_id, &output)?;
            self.database.complete_agent_execution(
                &task.id,
                &execution_id,
                &runtime.id,
                &output.answer,
            )?;
            Ok::<(), anyhow::Error>(())
        }
        .await;

        if let Err(error) = result {
            if !self
                .database
                .agent_task(&task.id)?
                .is_some_and(|current| current.status == "stopped")
            {
                self.database.pause_agent_execution(
                    &task.id,
                    Some(&execution_id),
                    &friendly_error(&error),
                )?;
            }
            return Err(error);
        }
        Ok(())
    }

    async fn run_native_question(
        &self,
        runtime: &RuntimeSpec,
        task: &AgentTask,
        workspace: &Path,
        control: &ExecutionControl,
    ) -> Result<RuntimeOutput> {
        let session = self.database.agent_session(&task.book_id, &runtime.id)?;
        let events_path = workspace.join("logs/events.jsonl");
        let mut events_file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events_path)
            .with_context(|| format!("无法创建 Agent 事件日志 {}", events_path.display()))?;
        let provider_kind = match runtime.kind {
            RuntimeKind::Codex => ProviderKind::Codex,
            RuntimeKind::Claude => ProviderKind::Claude,
            RuntimeKind::OpenCode => ProviderKind::OpenCode,
            RuntimeKind::Cursor | RuntimeKind::Custom => unreachable!(),
        };
        let config = SessionConfig {
            key: SessionKey {
                book_id: task.book_id.clone(),
                runtime_id: runtime.id.clone(),
            },
            provider: provider_kind,
            executable: runtime.executable.clone(),
            arguments: runtime.arguments.clone(),
            workspace: workspace.to_path_buf(),
            resume_session_id: session.map(|value| value.provider_session_id),
        };
        let instruction = "请阅读 context/current.md，并完成其中的 GoodReader 书籍问答任务。";
        let mut run = self
            .sessions
            .execute(config, instruction.to_string(), control.clone());
        {
            let mut tasks = self.live_tasks.lock().expect("Agent 实时状态锁");
            if let Some(live) = tasks.get_mut(&task.id) {
                live.execution_id = Some(run.execution_id.clone());
                live.turn_id = Some(run.turn_id.clone());
            }
        }
        self.active_questions
            .lock()
            .expect("Agent 问题控制锁")
            .insert(task.id.clone(), run.control());
        self.publish_task(&task.id);
        while let Some(event) = run.events.recv().await {
            if let Ok(line) = serde_json::to_string(&event) {
                use std::io::Write;
                let _ = writeln!(events_file, "{line}");
                let _ = events_file.flush();
            }
            {
                let mut tasks = self.live_tasks.lock().expect("Agent 实时状态锁");
                if let Some(live) = tasks.get_mut(&task.id) {
                    live.stream_sequence = event.scope().sequence;
                    match &event {
                        ProviderExecutionEvent::Phase { label, .. } => {
                            live.phase = Some(label.clone());
                        }
                        ProviderExecutionEvent::TextDelta { text, .. } => {
                            live.partial_output.push_str(text);
                        }
                        ProviderExecutionEvent::ToolStarted { name, .. } => {
                            live.phase = Some(format!("Agent 正在使用{name}"));
                        }
                        ProviderExecutionEvent::ToolCompleted { .. } => {
                            live.phase = Some("Agent 正在组织回答".to_string());
                        }
                        ProviderExecutionEvent::TurnCompleted { .. } => {
                            live.phase = Some("回答完成".to_string());
                        }
                        ProviderExecutionEvent::Cancelled { .. } => {
                            live.phase = Some("请求已停止".to_string());
                        }
                        ProviderExecutionEvent::ExecutionError { message, .. } => {
                            live.phase = Some(message.clone());
                        }
                        ProviderExecutionEvent::TurnStarted { .. }
                        | ProviderExecutionEvent::SessionStateChanged { .. }
                        | ProviderExecutionEvent::ThinkingDelta { .. } => {}
                    }
                }
            }
            if let ProviderExecutionEvent::SessionStateChanged {
                provider_session_id,
                model,
                ..
            } = &event
            {
                let state = serde_json::json!({ "model": model });
                self.database.save_agent_session(
                    &task.book_id,
                    &runtime.id,
                    provider_session_id,
                    &state.to_string(),
                )?;
            }
            let _ = self.task_updates.send(AgentTaskStreamEvent::Provider {
                task_id: task.id.clone(),
                event,
            });
        }
        let provider = run.finish().await?;
        if let Some(session_id) = provider.session_id.as_deref() {
            let state = serde_json::json!({ "model": provider.model });
            self.database.save_agent_session(
                &task.book_id,
                &runtime.id,
                session_id,
                &serde_json::to_string(&state)?,
            )?;
        }
        Ok(RuntimeOutput {
            answer: provider.answer,
            stdout: provider.transcript,
            stderr: provider.stderr,
        })
    }

    fn runtime_spec(&self, runtime_id: &str) -> Result<RuntimeSpec> {
        let builtin = match runtime_id {
            "codex" => Some(("codex", RuntimeKind::Codex)),
            "claude" => Some(("claude", RuntimeKind::Claude)),
            "cursor" => Some(("cursor", RuntimeKind::Cursor)),
            _ => None,
        };
        if let Some((name, kind)) = builtin {
            let executable = find_executable(name)
                .with_context(|| format!("找不到 {name} CLI，请先完成安装"))?;
            return Ok(RuntimeSpec {
                id: runtime_id.to_string(),
                executable,
                arguments: Vec::new(),
                kind,
            });
        }

        let custom = self
            .database
            .custom_agent_runtimes()?
            .into_iter()
            .find(|runtime| runtime.id == runtime_id)
            .with_context(|| format!("未知 Agent 运行时：{runtime_id}"))?;
        custom_runtime_spec(custom)
    }

    fn prepare_workspace(
        &self,
        package: &BookPackage,
        annotations: &[Annotation],
        task: &AgentTask,
        history: &[AiMessage],
    ) -> Result<PathBuf> {
        let session_directory = task
            .book_id
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let workspace = self
            .tasks_dir
            .join("Sessions")
            .join(session_directory)
            .join("workspace");
        let context_dir = workspace.join("context");
        let book_dir = workspace.join("book");
        let chapters_dir = book_dir.join("chapters");
        let output_dir = workspace.join("output");
        let logs_dir = workspace.join("logs");
        fs::create_dir_all(&context_dir)?;
        fs::create_dir_all(&chapters_dir)?;
        fs::create_dir_all(&output_dir)?;
        fs::create_dir_all(&logs_dir)?;

        let task_file = TaskFile {
            schema_version: 1,
            task_id: &task.id,
            book_id: &task.book_id,
            kind: &task.kind,
            goal: &task.goal,
            runtime_id: &task.current_runtime_id,
            output_directory: "output",
        };
        write_json(&workspace.join("task.json"), &task_file)?;
        write_json(&book_dir.join("book.json"), &package.manifest)?;
        write_json(&book_dir.join("annotations.json"), annotations)?;

        let mut chapter_index = String::new();
        for chapter in &package.manifest.chapters {
            let source = resolve_package_file(&package.root, &chapter.path)?;
            let html = fs::read_to_string(&source)
                .with_context(|| format!("无法读取章节 {}", source.display()))?;
            let markdown = chapter_markdown(&chapter.id, &chapter.title, &html);
            fs::write(chapters_dir.join(format!("{}.md", chapter.id)), &markdown)?;
            chapter_index.push_str(&format!(
                "- {}：book/chapters/{}.md\n",
                chapter.title, chapter.id
            ));
        }

        let mut history_jsonl = String::new();
        for message in history {
            history_jsonl.push_str(&serde_json::to_string(message)?);
            history_jsonl.push('\n');
        }
        fs::write(context_dir.join("history.jsonl"), history_jsonl)?;

        let current = format!(
            "# GoodReader 书籍问答任务\n\n\
             你正在为《{}》回答问题。请以书籍内容为主要依据，并利用同一本书的共享协作历史和用户阅读标注。\n\n\
             ## 当前问题\n\n{}\n\n\
             ## 可用资料\n\n\
             - 完整历史：`context/history.jsonl`\n\
             - 阅读标注：`book/annotations.json`\n\
             - 书籍清单：`book/book.json`\n\
             - 章节：\n{}\n\
             ## 回答要求\n\n\
             1. 先检索并阅读相关章节，不要只凭常识回答。\n\
             2. 引用书籍时使用 `[chapter:<chapter-id>#<block-id>]` 格式。\n\
             3. 引用用户笔记时明确标记为“你的笔记”。\n\
             4. 如果书中没有依据，请明确说明这是推断。\n\
             5. 直接给出完整、清晰的中文回答，不要描述你的工具调用过程。\n",
            package.manifest.title, task.goal, chapter_index
        );
        fs::write(context_dir.join("current.md"), current)?;
        Ok(workspace)
    }
}

fn custom_runtime_spec(custom: CustomAgentRuntime) -> Result<RuntimeSpec> {
    let executable = PathBuf::from(&custom.executable);
    if !is_executable_file(&executable) {
        bail!("找不到自定义 Agent 可执行文件：{}", executable.display());
    }
    Ok(RuntimeSpec {
        id: custom.id,
        executable,
        arguments: custom.arguments,
        kind: custom_runtime_kind(Path::new(&custom.executable)),
    })
}

fn custom_runtime_kind(executable: &Path) -> RuntimeKind {
    if executable.file_name().and_then(|name| name.to_str()) == Some("opencode") {
        RuntimeKind::OpenCode
    } else {
        RuntimeKind::Custom
    }
}

async fn probe_builtin(
    id: &str,
    name: &str,
    executable_name: &str,
    kind: RuntimeKind,
) -> AgentRuntime {
    let Some(executable) = find_executable(executable_name) else {
        return AgentRuntime {
            id: id.to_string(),
            name: name.to_string(),
            executable: None,
            available: false,
            version: None,
            detail: Some("未安装".to_string()),
            built_in: true,
            capabilities: builtin_capabilities(&kind),
        };
    };

    let version = probe_version(&executable, &kind).await;
    AgentRuntime {
        id: id.to_string(),
        name: name.to_string(),
        executable: Some(executable.display().to_string()),
        available: version.is_ok(),
        version: version.as_ref().ok().cloned(),
        detail: version.err().map(|error| friendly_error(&error)),
        built_in: true,
        capabilities: builtin_capabilities(&kind),
    }
}

fn builtin_capabilities(kind: &RuntimeKind) -> AgentRuntimeCapabilities {
    let structured = matches!(kind, RuntimeKind::Claude | RuntimeKind::Codex);
    AgentRuntimeCapabilities {
        streaming: matches!(
            kind,
            RuntimeKind::Claude | RuntimeKind::Codex | RuntimeKind::OpenCode
        ),
        native_resume: matches!(
            kind,
            RuntimeKind::Claude | RuntimeKind::Codex | RuntimeKind::OpenCode
        ),
        structured_output: structured,
        permission_mapping: true,
        tool_use: true,
    }
}

async fn probe_version(executable: &Path, kind: &RuntimeKind) -> Result<String> {
    let mut command = Command::new(executable);
    match kind {
        RuntimeKind::Cursor => {
            command.args(["agent", "--version"]);
        }
        _ => {
            command.arg("--version");
        }
    }
    command.stdin(Stdio::null());
    let output = tokio::time::timeout(Duration::from_secs(4), command.output())
        .await
        .context("版本检查超时")??;
    if !output.status.success() {
        bail!("版本检查失败");
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        bail!("版本检查没有返回结果");
    }
    Ok(version.lines().next().unwrap_or_default().to_string())
}

async fn run_runtime(
    runtime: &RuntimeSpec,
    workspace: &Path,
    active_processes: &Arc<Mutex<HashMap<PathBuf, u32>>>,
) -> Result<RuntimeOutput> {
    let instruction = "请阅读 context/current.md，并完成其中的 GoodReader 书籍问答任务。";
    run_runtime_instruction(
        runtime,
        workspace,
        instruction,
        false,
        None,
        Some(active_processes),
    )
    .await
}

async fn run_runtime_instruction(
    runtime: &RuntimeSpec,
    workspace: &Path,
    instruction: &str,
    writable: bool,
    live_logs: Option<&Path>,
    active_processes: Option<&Arc<Mutex<HashMap<PathBuf, u32>>>>,
) -> Result<RuntimeOutput> {
    let mut command = Command::new(&runtime.executable);
    command.current_dir(workspace);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);

    let uses_stdin = match runtime.kind {
        RuntimeKind::Codex => {
            command.args(["exec", "--skip-git-repo-check", "--sandbox"]);
            command.arg(if writable {
                "workspace-write"
            } else {
                "read-only"
            });
            command.arg("-C");
            command.arg(workspace);
            command.arg("-");
            true
        }
        RuntimeKind::Claude => {
            command.args(["-p", "--output-format", "text", "--permission-mode"]);
            command.arg(if writable { "acceptEdits" } else { "plan" });
            command.arg("--tools");
            command.arg(if writable {
                "Read,Write,Edit,Grep,Glob"
            } else {
                "Read,Grep,Glob"
            });
            true
        }
        RuntimeKind::OpenCode => {
            command.args(["run", "--format", "json"]);
            command.args(&runtime.arguments);
            command.arg(instruction);
            false
        }
        RuntimeKind::Cursor => {
            command.args([
                "agent",
                "-p",
                "--output-format",
                "text",
                "--mode",
                if writable { "agent" } else { "ask" },
                "--workspace",
            ]);
            command.arg(workspace);
            command.arg(instruction);
            false
        }
        RuntimeKind::Custom => {
            command.args(&runtime.arguments);
            true
        }
    };

    let mut output = execute_command(
        command,
        workspace,
        instruction,
        uses_stdin,
        live_logs,
        active_processes,
    )
    .await?;
    if matches!(runtime.kind, RuntimeKind::OpenCode) {
        output.answer = parse_opencode_answer(&output.stdout)?;
    }
    Ok(output)
}

fn parse_opencode_answer(output: &str) -> Result<String> {
    let mut answer = String::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("OpenCode 返回了无效 JSON 事件：{}", truncate(line, 200)))?;
        if value.get("type").and_then(serde_json::Value::as_str) == Some("text") {
            if let Some(text) = value
                .pointer("/part/text")
                .and_then(serde_json::Value::as_str)
            {
                answer.push_str(text);
            }
        }
    }
    if answer.trim().is_empty() {
        bail!("OpenCode 没有返回回答");
    }
    Ok(answer.trim().to_string())
}

async fn execute_command(
    command: Command,
    workspace: &Path,
    instruction: &str,
    uses_stdin: bool,
    live_logs: Option<&Path>,
    active_processes: Option<&Arc<Mutex<HashMap<PathBuf, u32>>>>,
) -> Result<RuntimeOutput> {
    execute_command_with_limits(
        command,
        workspace,
        instruction,
        uses_stdin,
        live_logs,
        active_processes,
        EXECUTION_TIMEOUT,
        NETWORK_FAILURE_TIMEOUT,
    )
    .await
}

async fn execute_command_with_limits(
    mut command: Command,
    workspace: &Path,
    instruction: &str,
    uses_stdin: bool,
    live_logs: Option<&Path>,
    active_processes: Option<&Arc<Mutex<HashMap<PathBuf, u32>>>>,
    execution_timeout: Duration,
    network_failure_timeout: Duration,
) -> Result<RuntimeOutput> {
    #[cfg(unix)]
    {
        std::os::unix::process::CommandExt::process_group(command.as_std_mut(), 0);
    }
    let mut child = command.spawn().context("无法启动 Agent 运行时")?;
    let pid = child.id().context("无法取得 Agent 进程标识")?;
    if let Some(processes) = active_processes {
        processes
            .lock()
            .expect("Agent 活动进程锁")
            .insert(workspace.to_path_buf(), pid);
    }
    if let Some(logs) = live_logs {
        fs::create_dir_all(logs)?;
        fs::write(logs.join("process.pid"), pid.to_string())?;
    }
    let stdout = child.stdout.take().context("无法读取 Agent 标准输出")?;
    let stderr = child.stderr.take().context("无法读取 Agent 错误输出")?;
    let stdout_log = live_logs.map(|logs| logs.join("stdout.log"));
    let stderr_log = live_logs.map(|logs| logs.join("stderr.log"));
    let network_failure = Arc::new(AtomicBool::new(false));
    let stdout_task = tokio::spawn(capture_runtime_stream(
        stdout,
        stdout_log,
        Some(network_failure.clone()),
        true,
    ));
    let stderr_task = tokio::spawn(capture_runtime_stream(
        stderr,
        stderr_log,
        Some(network_failure.clone()),
        false,
    ));
    if uses_stdin {
        let mut stdin = child.stdin.take().context("无法连接 Agent 标准输入")?;
        stdin.write_all(instruction.as_bytes()).await?;
        stdin.shutdown().await?;
        drop(stdin);
    }

    let started = Instant::now();
    let mut network_failure_started = None;
    let mut forced_error = None;
    let mut monitor = tokio::time::interval(Duration::from_millis(50));
    monitor.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let status = loop {
        tokio::select! {
            status = child.wait() => break Some(status?),
            _ = monitor.tick() => {
                if network_failure.load(Ordering::Relaxed) && network_failure_started.is_none() {
                    network_failure_started = Some(Instant::now());
                } else if !network_failure.load(Ordering::Relaxed) {
                    network_failure_started = None;
                }
                if network_failure_started
                    .is_some_and(|since: Instant| since.elapsed() >= network_failure_timeout)
                {
                    forced_error = Some(format!(
                        "Agent 网络连接持续失败超过 {} 秒，请检查当前 CLI 的网络连接后重试",
                        network_failure_timeout.as_secs()
                    ));
                    break None;
                }
                if started.elapsed() >= execution_timeout {
                    forced_error = Some(format!(
                        "Agent 执行超过 {} 分钟",
                        execution_timeout.as_secs() / 60
                    ));
                    break None;
                }
            }
        }
    };
    let Some(status) = status else {
        terminate_process_group(pid);
        let _ = child.kill().await;
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        finish_process_tracking(workspace, live_logs, active_processes);
        bail!(forced_error.expect("Agent 强制结束原因"));
    };
    let stdout = stdout_task.await.context("无法汇总 Agent 标准输出")??;
    let stderr = stderr_task.await.context("无法汇总 Agent 错误输出")??;
    let stdout = String::from_utf8_lossy(&stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
    finish_process_tracking(workspace, live_logs, active_processes);
    if !status.success() {
        let detail = runtime_failure_detail(&stdout, &stderr);
        if detail.is_empty() {
            bail!("Agent 执行失败（退出状态：{status}）");
        }
        bail!("Agent 执行失败：{}", truncate(&detail, 1_500));
    }
    if stdout.is_empty() {
        bail!("Agent 没有返回回答");
    }
    Ok(RuntimeOutput {
        answer: stdout.clone(),
        stdout,
        stderr,
    })
}

async fn execute_claude_translation_command(
    mut command: Command,
    workspace: &Path,
    instruction: &str,
    logs: &Path,
    active_processes: &Arc<Mutex<HashMap<PathBuf, u32>>>,
) -> Result<RuntimeOutput> {
    #[cfg(unix)]
    {
        std::os::unix::process::CommandExt::process_group(command.as_std_mut(), 0);
    }
    let mut child = command.spawn().context("无法启动 Claude 翻译运行时")?;
    let pid = child.id().context("无法取得 Agent 进程标识")?;
    active_processes
        .lock()
        .expect("Agent 活动进程锁")
        .insert(workspace.to_path_buf(), pid);
    fs::create_dir_all(logs)?;
    fs::write(logs.join("process.pid"), pid.to_string())?;

    let stdout = child.stdout.take().context("无法读取 Agent 标准输出")?;
    let stderr = child.stderr.take().context("无法读取 Agent 错误输出")?;
    let mut stdout_lines = BufReader::new(stdout).lines();
    let stderr_task = tokio::spawn(capture_runtime_stream(
        stderr,
        Some(logs.join("stderr.log")),
        None,
        false,
    ));
    let mut stdin = child.stdin.take().context("无法连接 Agent 标准输入")?;
    stdin.write_all(instruction.as_bytes()).await?;
    stdin.shutdown().await?;
    drop(stdin);

    let mut stdout_log = fs::File::create(logs.join("stdout.log"))?;
    let mut retained = String::new();
    let mut invalid_outputs = 0usize;
    let mut forced_error = None;
    let timeout = tokio::time::sleep(TRANSLATION_EXECUTION_TIMEOUT);
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            line = stdout_lines.next_line() => {
                let Some(line) = line? else { break };
                let filtered = filter_claude_translation_event(&line, &mut invalid_outputs);
                if let Some(filtered) = filtered {
                    use std::io::Write;
                    writeln!(stdout_log, "{filtered}")?;
                    stdout_log.flush()?;
                    retained.push_str(&filtered);
                    retained.push('\n');
                }
                if invalid_outputs >= MAX_INVALID_STRUCTURED_OUTPUTS {
                    forced_error = Some(format!(
                        "Claude 连续 {invalid_outputs} 次返回无效结构化工具输入；当前批次将自动拆分"
                    ));
                    terminate_process_group(pid);
                    break;
                }
            }
            _ = &mut timeout => {
                forced_error = Some("Agent 翻译执行超过 8 分钟；当前批次将自动拆分".to_string());
                terminate_process_group(pid);
                break;
            }
        }
    }

    if let Some(error) = forced_error {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        stderr_task.abort();
        let _ = stderr_task.await;
        finish_process_tracking(workspace, Some(logs), Some(active_processes));
        bail!("{error}");
    }
    let status = child.wait().await?;
    let stderr = stderr_task.await.context("无法汇总 Agent 错误输出")??;
    let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
    finish_process_tracking(workspace, Some(logs), Some(active_processes));
    let stdout = retained.trim().to_string();
    if !status.success() {
        let detail = runtime_failure_detail(&stdout, &stderr);
        if detail.is_empty() {
            bail!("Agent 执行失败（退出状态：{status}）");
        }
        bail!("Agent 执行失败：{}", truncate(&detail, 1_500));
    }
    if stdout.is_empty() {
        bail!("Agent 没有返回翻译结果");
    }
    Ok(RuntimeOutput {
        answer: stdout.clone(),
        stdout,
        stderr,
    })
}

fn filter_claude_translation_event(line: &str, invalid_outputs: &mut usize) -> Option<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Some(line.to_string());
    };
    if json_contains_string(&value, "__unparsedToolInput") {
        *invalid_outputs += 1;
        return Some(
            serde_json::json!({
                "type": "goodreader",
                "subtype": "invalid_structured_output",
                "attempt": *invalid_outputs,
                "detail": "Claude 返回的结构化 JSON 无效或被截断"
            })
            .to_string(),
        );
    }
    if json_contains_string(&value, "thinking_delta")
        || value.get("type").and_then(|item| item.as_str()) == Some("thinking")
    {
        return None;
    }
    Some(line.to_string())
}

fn json_contains_string(value: &serde_json::Value, expected: &str) -> bool {
    match value {
        serde_json::Value::String(value) => value == expected,
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| json_contains_string(value, expected)),
        serde_json::Value::Object(values) => values
            .iter()
            .any(|(key, value)| key == expected || json_contains_string(value, expected)),
        _ => false,
    }
}

fn runtime_failure_detail(stdout: &str, stderr: &str) -> String {
    if !stderr.trim().is_empty() {
        return tail_chars(stderr.trim(), 1_500);
    }
    let mut invalid_outputs = 0usize;
    for line in stdout.lines().rev().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return tail_chars(line.trim(), 1_500);
        };
        if json_contains_string(&value, "__unparsedToolInput")
            || value.get("subtype").and_then(|item| item.as_str())
                == Some("invalid_structured_output")
        {
            invalid_outputs += 1;
        }
        if value.get("type").and_then(|item| item.as_str()) == Some("result") {
            for key in ["error", "result", "message"] {
                if let Some(detail) = value.get(key).and_then(|item| item.as_str()) {
                    if !detail.trim().is_empty() {
                        return tail_chars(detail.trim(), 1_500);
                    }
                }
            }
        }
    }
    if invalid_outputs > 0 {
        return format!("Claude 返回了 {invalid_outputs} 次无效结构化工具输入");
    }
    stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| tail_chars(line.trim(), 1_500))
        .unwrap_or_default()
}

fn tail_chars(value: &str, max_chars: usize) -> String {
    let length = value.chars().count();
    value
        .chars()
        .skip(length.saturating_sub(max_chars))
        .collect()
}

async fn capture_runtime_stream<R>(
    mut stream: R,
    log_path: Option<PathBuf>,
    network_failure: Option<Arc<AtomicBool>>,
    clears_network_failure: bool,
) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut log = log_path.map(fs::File::create).transpose()?;
    let mut buffer = [0u8; 8 * 1024];
    let mut diagnostic_tail = String::new();
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..read]);
        if let Some(log) = &mut log {
            std::io::Write::write_all(log, &buffer[..read])?;
            std::io::Write::flush(log)?;
        }
        if let Some(network_failure) = &network_failure {
            if clears_network_failure {
                network_failure.store(false, Ordering::Relaxed);
            } else {
                diagnostic_tail.push_str(&String::from_utf8_lossy(&buffer[..read]));
                if reports_network_failure(&diagnostic_tail) {
                    network_failure.store(true, Ordering::Relaxed);
                }
                diagnostic_tail = tail_chars(&diagnostic_tail, 4_096);
            }
        }
    }
    Ok(output)
}

fn reports_network_failure(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("tls handshake eof")
        || value.contains("stream disconnected before completion")
        || value.contains("error sending request for url")
}

fn translation_instruction(source_language: &str) -> String {
    format!(
        "把每个翻译单元翻译为简体中文。输出键必须与输入完全一致，不得合并、拆分、增加或删除翻译单元。\n\
         以 `goodreader-metadata-` 开头的键是书名或章节目录标题，译文应简洁并保留章节序号；其他键是正文块。\n\
         `{{{{GR数字}}}}` 是受保护片段：每个标记必须原样出现且只能出现一次，但可以随中文语序重新排列。\n\
         不翻译代码、命令、路径、URL、API 名称、标识符、公式和结构化数据。来源主语言：{source_language}。"
    )
}

fn read_translation_output(workspace: &Path) -> Result<BTreeMap<String, String>> {
    let path = workspace.join("output/translations.json");
    let value = fs::read(&path).with_context(|| format!("Agent 没有生成 {}", path.display()))?;
    serde_json::from_slice(&value).context("Agent 生成的译文不是合法 JSON 对象")
}

fn parse_claude_translation_stream(
    output: &str,
) -> Result<(
    StructuredTranslation,
    Option<String>,
    Option<String>,
    String,
)> {
    let mut session_id = None;
    let mut model = None;
    let mut answer = String::new();
    let mut structured = None;
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if session_id.is_none() {
            session_id = value
                .get("session_id")
                .and_then(|value| value.as_str())
                .map(str::to_string);
        }
        if model.is_none() {
            model = value
                .get("model")
                .and_then(|value| value.as_str())
                .map(str::to_string);
        }
        if value.get("type").and_then(|value| value.as_str()) != Some("result") {
            continue;
        }
        if let Some(result) = value.get("result").and_then(|value| value.as_str()) {
            answer = result.to_string();
        }
        if let Some(result) = value.get("structured_output") {
            structured = serde_json::from_value::<StructuredTranslation>(result.clone()).ok();
        }
        if structured.is_none() && !answer.is_empty() {
            structured = serde_json::from_str::<StructuredTranslation>(&answer).ok();
        }
    }
    let structured = structured.context("Claude 没有返回符合 Schema 的翻译结果")?;
    Ok((structured, session_id, model, answer))
}

fn parse_runtime_model(output: &str) -> Option<String> {
    output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|value| {
            value
                .get("model")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
}

fn parse_codex_session_id(output: &str) -> Option<String> {
    output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|value| {
            value
                .get("thread_id")
                .or_else(|| value.get("threadId"))
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
}

fn finish_process_tracking(
    workspace: &Path,
    live_logs: Option<&Path>,
    active_processes: Option<&Arc<Mutex<HashMap<PathBuf, u32>>>>,
) {
    if let Some(processes) = active_processes {
        processes
            .lock()
            .expect("Agent 活动进程锁")
            .remove(workspace);
    }
    if let Some(logs) = live_logs {
        let _ = fs::remove_file(logs.join("process.pid"));
    }
}

fn terminate_process_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn collect_process_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return files;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_dir() {
            files.extend(collect_process_files(&path));
        } else if path.file_name().is_some_and(|name| name == "process.pid") {
            files.push(path);
        }
    }
    files
}

fn chapter_markdown(chapter_id: &str, title: &str, html: &str) -> String {
    let block_regex = Regex::new(
        r#"(?is)<([a-z][a-z0-9:-]*)\b[^>]*\bdata-goodreader-block\s*=\s*[\"']([^\"']+)[\"'][^>]*>"#,
    )
    .expect("正文块正则固定有效");
    let lower = html.to_ascii_lowercase();
    let mut output = format!("# {title}\n\n<!-- chapter-id: {chapter_id} -->\n\n");
    for captures in block_regex.captures_iter(html) {
        let Some(opening) = captures.get(0) else {
            continue;
        };
        let tag = captures.get(1).map(|value| value.as_str()).unwrap_or("div");
        let block_id = captures
            .get(2)
            .map(|value| value.as_str())
            .unwrap_or_default();
        let closing = format!("</{}", tag.to_ascii_lowercase());
        let body_end = lower[opening.end()..]
            .find(&closing)
            .map(|offset| opening.end() + offset)
            .unwrap_or(opening.end());
        let text = strip_html(&html[opening.end()..body_end]);
        if !text.is_empty() {
            output.push_str(&format!("## {block_id}\n\n{text}\n\n"));
        }
    }
    output
}

fn strip_html(value: &str) -> String {
    let tag_regex = Regex::new(r"(?is)<[^>]+>").expect("HTML 标签正则固定有效");
    let text = tag_regex.replace_all(value, " ");
    decode_basic_entities(&text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_basic_entities(value: &str) -> String {
    value
        .replace("&nbsp;", " ")
        .replace("&#160;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn write_execution_log(workspace: &Path, execution_id: &str, output: &RuntimeOutput) -> Result<()> {
    let log = format!(
        "--- stdout ---\n{}\n\n--- stderr ---\n{}\n",
        output.stdout, output.stderr
    );
    fs::write(
        workspace.join("logs").join(format!("{execution_id}.log")),
        log,
    )?;
    Ok(())
}

fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("无法写入 {}", path.display()))
}

fn find_executable(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return is_executable_file(&path).then_some(path);
    }
    if let Some(paths) = env::var_os("PATH") {
        for directory in env::split_paths(&paths) {
            let candidate = directory.join(name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    let home = env::var_os("HOME").map(PathBuf::from);
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/bin").join(name),
        PathBuf::from("/usr/local/bin").join(name),
        PathBuf::from("/usr/bin").join(name),
    ];
    if let Some(home) = home {
        candidates.insert(0, home.join(".local/bin").join(name));
    }
    candidates.into_iter().find(|path| is_executable_file(path))
}

fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn friendly_error(error: &anyhow::Error) -> String {
    truncate(&format!("{error:#}"), 2_000)
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    use super::{
        chapter_markdown, custom_runtime_spec, execute_claude_translation_command, execute_command,
        execute_command_with_limits, filter_claude_translation_event,
        parse_claude_translation_stream, run_runtime_instruction, terminate_process_group,
        truncate, AgentCoordinator,
    };
    use crate::db::Database;
    use crate::models::CustomAgentRuntime;

    #[test]
    fn builds_a_book_view_with_stable_block_ids() {
        let html = r#"
            <main data-goodreader-content data-goodreader-chapter="ch1">
              <p data-goodreader-block="ch1-p1">所有权是 <strong>Rust</strong> 的核心规则。</p>
              <pre data-goodreader-block="ch1-code"><code>let value = 1;</code></pre>
            </main>
        "#;
        let markdown = chapter_markdown("ch1", "第一章", html);
        assert!(markdown.contains("## ch1-p1"));
        assert!(markdown.contains("所有权是 Rust 的核心规则。"));
        assert!(markdown.contains("## ch1-code"));
        assert!(markdown.contains("let value = 1;"));
    }

    #[test]
    fn truncates_diagnostic_text_by_character() {
        assert_eq!(truncate("中文错误详情", 4), "中文错误");
    }

    #[tokio::test]
    async fn opencode_custom_runtime_uses_noninteractive_json_mode() {
        let temp = TempDir::new().expect("临时目录");
        let executable = temp.path().join("opencode");
        fs::write(
            &executable,
            r#"#!/bin/sh
if [ "$1" != "run" ] || [ "$2" != "--format" ] || [ "$3" != "json" ]; then
  sleep 10
  exit 1
fi
printf '%s\n' '{"type":"step_start","sessionID":"session-1","part":{"type":"step-start"}}'
printf '%s\n' '{"type":"text","sessionID":"session-1","part":{"type":"text","text":"OpenCode 已启动"}}'
printf '%s\n' '{"type":"step_finish","sessionID":"session-1","part":{"type":"step-finish","reason":"stop"}}'
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let runtime = custom_runtime_spec(CustomAgentRuntime {
            id: "custom-opencode".to_string(),
            name: "OpenCode".to_string(),
            executable: executable.display().to_string(),
            arguments: Vec::new(),
        })
        .expect("识别 OpenCode 运行时");

        let output = tokio::time::timeout(
            Duration::from_secs(1),
            run_runtime_instruction(
                &runtime,
                temp.path(),
                "只回复：OpenCode 已启动",
                false,
                None,
                None,
            ),
        )
        .await
        .expect("OpenCode 不应进入交互式 TUI")
        .expect("OpenCode 应完成请求");

        assert_eq!(output.answer, "OpenCode 已启动");
    }

    #[test]
    fn reads_structured_translation_from_claude_event_stream() {
        let output = r#"{"type":"system","subtype":"init","session_id":"session-1","model":"deepseek-v4-flash"}
{"type":"result","subtype":"success","result":"done","session_id":"session-1","structured_output":{"translations":{"block-1":"译文"}}}"#;
        let (result, session_id, model, answer) =
            parse_claude_translation_stream(output).expect("应解析结构化翻译事件");
        assert_eq!(
            result.translations.get("block-1").map(String::as_str),
            Some("译文")
        );
        assert_eq!(session_id.as_deref(), Some("session-1"));
        assert_eq!(model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(answer, "done");
    }

    #[tokio::test]
    async fn reports_the_terminal_claude_error_instead_of_the_init_event() {
        let temp = TempDir::new().expect("临时目录");
        let executable = temp.path().join("failing-claude.sh");
        fs::write(
            &executable,
            r#"#!/bin/sh
printf '%s\n' '{"type":"system","subtype":"init","session_id":"session-1"}'
printf '%s\n' '{"type":"result","subtype":"error","is_error":true,"result":"结构化输出无效：translations 字段被截断"}'
exit 2
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let mut command = tokio::process::Command::new(&executable);
        command
            .current_dir(temp.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let error = match execute_command(command, temp.path(), "", true, None, None).await {
            Ok(_) => panic!("运行时应失败"),
            Err(error) => error,
        };
        let detail = format!("{error:#}");
        assert!(detail.contains("translations 字段被截断"), "{detail}");
        assert!(!detail.contains("subtype\":\"init"), "{detail}");
    }

    #[tokio::test]
    async fn stops_waiting_after_the_agent_reports_persistent_network_failures() {
        let temp = TempDir::new().expect("临时目录");
        let executable = temp.path().join("network-failure.sh");
        fs::write(
            &executable,
            r#"#!/bin/sh
cat >/dev/null
printf '%s\n' 'ERROR: Reconnecting... 5/5 (tls handshake eof)' >&2
sleep 30
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let mut command = tokio::process::Command::new(&executable);
        command
            .current_dir(temp.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let started = Instant::now();
        let error = match execute_command_with_limits(
            command,
            temp.path(),
            "question",
            true,
            None,
            None,
            Duration::from_secs(5),
            Duration::from_millis(150),
        )
        .await
        {
            Ok(_) => panic!("持续网络错误必须停止等待"),
            Err(error) => error,
        };

        let detail = format!("{error:#}");
        assert!(detail.contains("网络连接持续失败"), "{detail}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn keeps_waiting_when_the_agent_recovers_and_starts_answering() {
        let temp = TempDir::new().expect("临时目录");
        let executable = temp.path().join("network-recovered.sh");
        fs::write(
            &executable,
            r#"#!/bin/sh
cat >/dev/null
printf '%s\n' 'ERROR: tls handshake eof' >&2
sleep 0.05
printf '%s\n' '网络恢复后的回答'
sleep 0.3
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let mut command = tokio::process::Command::new(&executable);
        command
            .current_dir(temp.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let output = execute_command_with_limits(
            command,
            temp.path(),
            "question",
            true,
            None,
            None,
            Duration::from_secs(5),
            Duration::from_millis(150),
        )
        .await
        .expect("Agent 恢复输出后应继续等待完成");

        assert_eq!(output.answer, "网络恢复后的回答");
    }

    #[tokio::test]
    async fn tracks_and_terminates_a_running_question_process() {
        let temp = TempDir::new().expect("临时目录");
        let executable = temp.path().join("long-running-agent.sh");
        fs::write(&executable, "#!/bin/sh\ncat >/dev/null\nsleep 30\n").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let workspace = temp.path().join("question-task");
        fs::create_dir_all(&workspace).unwrap();
        let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let active_for_run = active.clone();
        let workspace_for_run = workspace.clone();
        let mut command = tokio::process::Command::new(&executable);
        command
            .current_dir(&workspace)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let execution = tokio::spawn(async move {
            execute_command(
                command,
                &workspace_for_run,
                "question",
                true,
                None,
                Some(&active_for_run),
            )
            .await
        });

        let pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(pid) = active.lock().unwrap().get(&workspace).copied() {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("问答进程应登记为活动进程");
        terminate_process_group(pid);

        let result = tokio::time::timeout(Duration::from_secs(2), execution)
            .await
            .expect("停止后进程应及时退出")
            .expect("执行任务");
        assert!(result.is_err());
        assert!(active.lock().unwrap().is_empty());
    }

    #[test]
    fn removes_thinking_events_and_summarizes_invalid_structured_output() {
        let mut invalid_outputs = 0;
        let thinking = r#"{"type":"stream_event","event":{"delta":{"type":"thinking_delta","thinking":"long internal trace"}}}"#;
        assert!(filter_claude_translation_event(thinking, &mut invalid_outputs).is_none());
        let invalid = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","input":{"__unparsedToolInput":"very long invalid json"}}]}}"#;
        let filtered = filter_claude_translation_event(invalid, &mut invalid_outputs)
            .expect("无效结构化输出应保留摘要");
        assert_eq!(invalid_outputs, 1);
        assert!(filtered.contains("invalid_structured_output"));
        assert!(!filtered.contains("very long invalid json"));
    }

    #[tokio::test]
    async fn stops_claude_after_repeated_invalid_structured_outputs() {
        let temp = TempDir::new().expect("临时目录");
        let executable = temp.path().join("invalid-claude.sh");
        fs::write(
            &executable,
            r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"type":"system","subtype":"init","session_id":"session-1"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"input":{"__unparsedToolInput":"broken-1"}}]}}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"input":{"__unparsedToolInput":"broken-2"}}]}}'
sleep 30
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let workspace = temp.path().join("workspace");
        let logs = workspace.join("logs");
        fs::create_dir_all(&workspace).unwrap();
        let mut command = tokio::process::Command::new(&executable);
        command
            .current_dir(&workspace)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let started = Instant::now();

        let error = match execute_claude_translation_command(
            command,
            &workspace,
            "translate",
            &logs,
            &active,
        )
        .await
        {
            Ok(_) => panic!("重复无效输出必须失败"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("连续 2 次"));
        assert!(started.elapsed() < Duration::from_secs(10));
        let log = fs::read_to_string(logs.join("stdout.log")).unwrap();
        assert!(!log.contains("broken-1"));
        assert!(!logs.join("process.pid").exists());
    }

    #[tokio::test]
    async fn accepts_a_complete_translation_file_without_waiting_for_cli_exit() {
        let temp = TempDir::new().expect("临时目录");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(workspace.join("input")).unwrap();
        let blocks = BTreeMap::from([("block-1".to_string(), "English text".to_string())]);
        fs::write(
            workspace.join("input/blocks.json"),
            serde_json::to_vec_pretty(&blocks).unwrap(),
        )
        .unwrap();
        let executable = temp.path().join("write-then-wait.sh");
        fs::write(
            &executable,
            "#!/bin/sh\nset -eu\ncat >/dev/null\nmkdir -p output\ncp input/blocks.json output/translations.json\nsleep 10\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let database = Arc::new(Database::open(&temp.path().join("Data")).unwrap());
        let runtime = database
            .save_custom_agent_runtime("延迟退出 Agent", executable.to_str().unwrap(), &[])
            .unwrap();
        let coordinator = AgentCoordinator::new(temp.path().join("Tasks"), database).unwrap();

        let started = Instant::now();
        let result = coordinator
            .run_translation(&runtime.id, &workspace, &blocks, "non-zh")
            .await
            .expect("输出文件完整后应结束翻译步骤");

        assert_eq!(result.translations, blocks);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(!workspace.join("logs/process.pid").exists());
    }
}

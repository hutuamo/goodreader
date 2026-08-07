use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, watch, Mutex as AsyncMutex};
use uuid::Uuid;

const TURN_TIMEOUT: Duration = Duration::from_secs(60 * 30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Codex,
    Claude,
    OpenCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub book_id: String,
    pub runtime_id: String,
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub key: SessionKey,
    pub provider: ProviderKind,
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub workspace: PathBuf,
    pub resume_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventScope {
    pub session_instance_id: String,
    pub execution_id: String,
    pub turn_id: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderExecutionEvent {
    TurnStarted {
        scope: EventScope,
    },
    Phase {
        scope: EventScope,
        label: String,
    },
    SessionStateChanged {
        scope: EventScope,
        provider_session_id: String,
        model: Option<String>,
        status: String,
    },
    TextDelta {
        scope: EventScope,
        text: String,
    },
    ThinkingDelta {
        scope: EventScope,
        text: String,
    },
    ToolStarted {
        scope: EventScope,
        tool_call_id: String,
        name: String,
    },
    ToolCompleted {
        scope: EventScope,
        tool_call_id: String,
        name: String,
        is_error: bool,
    },
    TurnCompleted {
        scope: EventScope,
        reason: String,
    },
    Cancelled {
        scope: EventScope,
        reason: String,
    },
    ExecutionError {
        scope: EventScope,
        category: String,
        message: String,
        recoverable: bool,
    },
}

impl ProviderExecutionEvent {
    pub fn scope(&self) -> &EventScope {
        match self {
            Self::TurnStarted { scope }
            | Self::Phase { scope, .. }
            | Self::SessionStateChanged { scope, .. }
            | Self::TextDelta { scope, .. }
            | Self::ThinkingDelta { scope, .. }
            | Self::ToolStarted { scope, .. }
            | Self::ToolCompleted { scope, .. }
            | Self::TurnCompleted { scope, .. }
            | Self::Cancelled { scope, .. }
            | Self::ExecutionError { scope, .. } => scope,
        }
    }
}

#[derive(Debug)]
pub struct ProviderOutput {
    pub answer: String,
    pub transcript: String,
    pub stderr: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
}

#[derive(Clone)]
pub struct ExecutionControl {
    cancel: watch::Sender<bool>,
    pid: Arc<AtomicU32>,
}

impl ExecutionControl {
    pub fn new() -> Self {
        let (cancel, _) = watch::channel(false);
        Self {
            cancel,
            pid: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.cancel.subscribe()
    }

    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
}

pub struct ExecutionRun {
    pub execution_id: String,
    pub turn_id: String,
    pub events: mpsc::UnboundedReceiver<ProviderExecutionEvent>,
    completion: oneshot::Receiver<Result<ProviderOutput>>,
    control: ExecutionControl,
}

impl ExecutionRun {
    pub async fn finish(self) -> Result<ProviderOutput> {
        self.completion.await.context("Agent 执行通道提前关闭")?
    }

    pub fn control(&self) -> ExecutionControl {
        self.control.clone()
    }
}

#[derive(Clone)]
pub struct AgentSessionHost {
    sessions: Arc<Mutex<HashMap<SessionKey, Arc<SessionSlot>>>>,
}

struct SessionSlot {
    instance_id: String,
    backend: AsyncMutex<ProviderBackend>,
    active_control: Mutex<Option<ExecutionControl>>,
}

enum ProviderBackend {
    Codex(CodexSession),
    Claude(ClaudeSession),
    OpenCode(OpenCodeSession),
}

impl AgentSessionHost {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn execute(
        &self,
        config: SessionConfig,
        instruction: String,
        control: ExecutionControl,
    ) -> ExecutionRun {
        let slot = {
            let mut sessions = self.sessions.lock().expect("Agent 会话表锁");
            sessions
                .entry(config.key.clone())
                .or_insert_with(|| {
                    Arc::new(SessionSlot {
                        instance_id: Uuid::new_v4().to_string(),
                        backend: AsyncMutex::new(match config.provider {
                            ProviderKind::Codex => {
                                ProviderBackend::Codex(CodexSession::new(&config))
                            }
                            ProviderKind::Claude => {
                                ProviderBackend::Claude(ClaudeSession::new(&config))
                            }
                            ProviderKind::OpenCode => {
                                ProviderBackend::OpenCode(OpenCodeSession::new(&config))
                            }
                        }),
                        active_control: Mutex::new(None),
                    })
                })
                .clone()
        };
        let execution_id = Uuid::new_v4().to_string();
        let turn_id = Uuid::new_v4().to_string();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (completion_tx, completion_rx) = oneshot::channel();
        let instance_id = slot.instance_id.clone();
        let execution_id_for_task = execution_id.clone();
        let turn_id_for_task = turn_id.clone();
        let control_for_task = control.clone();
        // 记录当前 turn 的控制句柄：dispose_book 先 cancel 再等后端锁（P2-7）。
        *slot.active_control.lock().expect("Agent 会话控制锁") = Some(control.clone());

        tokio::spawn(async move {
            let emitter = EventEmitter::new(
                instance_id,
                execution_id_for_task,
                turn_id_for_task,
                events_tx,
            );
            emitter.emit(|scope| ProviderExecutionEvent::TurnStarted { scope });
            let mut backend = slot.backend.lock().await;
            let result = match tokio::time::timeout(
                TURN_TIMEOUT,
                backend.execute(&config, &instruction, &control_for_task, &emitter),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    backend.dispose().await;
                    Err(anyhow::anyhow!("Agent 执行超过 30 分钟"))
                }
            };
            match &result {
                Ok(_) => emitter.emit(|scope| ProviderExecutionEvent::TurnCompleted {
                    scope,
                    reason: "completed".to_string(),
                }),
                Err(error) if *control_for_task.subscribe().borrow() => {
                    emitter.emit(|scope| ProviderExecutionEvent::Cancelled {
                        scope,
                        reason: "用户停止了请求".to_string(),
                    });
                    let _ = error;
                }
                Err(error) => emitter.emit(|scope| ProviderExecutionEvent::ExecutionError {
                    scope,
                    category: classify_error(error),
                    message: format!("{error:#}"),
                    recoverable: true,
                }),
            }
            let _ = completion_tx.send(result);
        });

        ExecutionRun {
            execution_id,
            turn_id,
            events: events_rx,
            completion: completion_rx,
            control,
        }
    }

    pub async fn dispose_book(&self, book_id: &str) {
        let slots = {
            let mut sessions = self.sessions.lock().expect("Agent 会话表锁");
            let keys = sessions
                .keys()
                .filter(|key| key.book_id == book_id)
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| sessions.remove(&key))
                .collect::<Vec<_>>()
        };
        // 先取消该书全部活跃 turn：不 cancel 的话后端锁会被 turn 持有到
        // TURN_TIMEOUT（最长 30 分钟），清除请求会一直挂起（P2-7）。
        for slot in &slots {
            if let Some(control) = slot.active_control.lock().expect("Agent 会话控制锁").take()
            {
                control.cancel();
            }
        }
        for slot in slots {
            slot.backend.lock().await.dispose().await;
        }
    }
}

impl Default for AgentSessionHost {
    fn default() -> Self {
        Self::new()
    }
}

struct EventEmitter {
    session_instance_id: String,
    execution_id: String,
    turn_id: String,
    sequence: Mutex<u64>,
    run_tx: mpsc::UnboundedSender<ProviderExecutionEvent>,
}

impl EventEmitter {
    fn new(
        session_instance_id: String,
        execution_id: String,
        turn_id: String,
        run_tx: mpsc::UnboundedSender<ProviderExecutionEvent>,
    ) -> Self {
        Self {
            session_instance_id,
            execution_id,
            turn_id,
            sequence: Mutex::new(0),
            run_tx,
        }
    }

    fn emit(&self, build: impl FnOnce(EventScope) -> ProviderExecutionEvent) {
        let mut sequence = self.sequence.lock().expect("Agent 事件序号锁");
        *sequence += 1;
        let event = build(EventScope {
            session_instance_id: self.session_instance_id.clone(),
            execution_id: self.execution_id.clone(),
            turn_id: self.turn_id.clone(),
            sequence: *sequence,
        });
        let _ = self.run_tx.send(event);
    }
}

impl ProviderBackend {
    async fn execute(
        &mut self,
        config: &SessionConfig,
        instruction: &str,
        control: &ExecutionControl,
        events: &EventEmitter,
    ) -> Result<ProviderOutput> {
        match self {
            Self::Codex(session) => session.execute(config, instruction, control, events).await,
            Self::Claude(session) => session.execute(config, instruction, control, events).await,
            Self::OpenCode(session) => session.execute(config, instruction, control, events).await,
        }
    }

    async fn dispose(&mut self) {
        match self {
            Self::Codex(session) => session.dispose().await,
            Self::Claude(session) => session.dispose().await,
            Self::OpenCode(session) => session.dispose().await,
        }
    }
}

struct CodexSession {
    process: Option<NativeProcess>,
    thread_id: Option<String>,
    model: Option<String>,
    resume_session_id: Option<String>,
    request_id: u64,
}

impl CodexSession {
    fn new(config: &SessionConfig) -> Self {
        Self {
            process: None,
            thread_id: None,
            model: None,
            resume_session_id: config.resume_session_id.clone(),
            request_id: 0,
        }
    }

    async fn execute(
        &mut self,
        config: &SessionConfig,
        instruction: &str,
        control: &ExecutionControl,
        events: &EventEmitter,
    ) -> Result<ProviderOutput> {
        self.ensure_process(config, control, events).await?;
        events.emit(|scope| ProviderExecutionEvent::Phase {
            scope,
            label: "Codex 正在阅读书籍并组织回答".to_string(),
        });
        let request_id = self.next_request_id();
        let process = self.process.as_mut().context("Codex 会话进程不可用")?;
        send_json(
            &mut process.stdin,
            &json!({
                "id": request_id,
                "method": "turn/start",
                "params": {
                    "threadId": self.thread_id,
                    "input": [{ "type": "text", "text": instruction, "text_elements": [] }],
                    "cwd": config.workspace,
                    "approvalPolicy": "never"
                }
            }),
        )
        .await?;
        let mut transcript = Vec::new();
        let turn_result = read_rpc_response(process, request_id, &mut transcript).await?;
        let turn_id = turn_result
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .context("Codex 没有返回 turn id")?
            .to_string();
        let mut answer = String::new();
        let mut cancel = control.subscribe();
        loop {
            tokio::select! {
                changed = cancel.changed() => {
                    if changed.is_ok() && *cancel.borrow() {
                        let interrupt_id = self.next_request_id();
                        if let Some(process) = self.process.as_mut() {
                            let _ = send_json(&mut process.stdin, &json!({
                                "id": interrupt_id,
                                "method": "turn/interrupt",
                                "params": { "threadId": self.thread_id, "turnId": turn_id }
                            })).await;
                        }
                        if let Some(mut process) = self.process.take() {
                            process.terminate().await;
                        }
                        bail!("Agent 请求已取消");
                    }
                }
                line = self.process.as_mut().context("Codex 会话进程不可用")?.lines.next_line() => {
                    let Some(line) = line.context("读取 Codex 事件失败")? else {
                        self.process = None;
                        bail!("Codex app-server 在回答完成前退出");
                    };
                    transcript.push(line.clone());
                    let value: Value = serde_json::from_str(&line).context("Codex 返回了无效 JSON-RPC 消息")?;
                    if value.get("id").is_some() && value.get("method").is_some() {
                        if let Some(process) = self.process.as_mut() {
                            reject_server_request(&mut process.stdin, &value).await?;
                        }
                        continue;
                    }
                    let method = value.get("method").and_then(Value::as_str).unwrap_or_default();
                    let params = value.get("params").cloned().unwrap_or(Value::Null);
                    match method {
                        "item/agentMessage/delta" => {
                            if let Some(text) = params.get("delta").and_then(Value::as_str) {
                                answer.push_str(text);
                                events.emit(|scope| ProviderExecutionEvent::TextDelta { scope, text: text.to_string() });
                            }
                        }
                        "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                            if let Some(text) = params.get("delta").and_then(Value::as_str) {
                                events.emit(|scope| ProviderExecutionEvent::ThinkingDelta { scope, text: text.to_string() });
                            }
                        }
                        "item/started" => {
                            if let Some((tool_call_id, name)) = codex_tool(&params) {
                                events.emit(|scope| ProviderExecutionEvent::ToolStarted { scope, tool_call_id, name });
                            }
                        }
                        "item/completed" => {
                            if let Some((tool_call_id, name)) = codex_tool(&params) {
                                events.emit(|scope| ProviderExecutionEvent::ToolCompleted { scope, tool_call_id, name, is_error: false });
                            }
                        }
                        "turn/completed" => {
                            let native_turn_id = params.pointer("/turn/id").and_then(Value::as_str).unwrap_or_default();
                            if !native_turn_id.is_empty() && native_turn_id != turn_id { continue; }
                            let status = params.pointer("/turn/status").and_then(Value::as_str).unwrap_or("failed");
                            if answer.trim().is_empty() {
                                answer = codex_final_answer(&params).unwrap_or_default();
                            }
                            if status != "completed" {
                                let detail = params.pointer("/turn/error/message").and_then(Value::as_str).unwrap_or("Codex 回答失败");
                                bail!("{detail}");
                            }
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
        if answer.trim().is_empty() {
            bail!("Codex 没有返回回答");
        }
        Ok(ProviderOutput {
            answer: answer.trim().to_string(),
            transcript: transcript.join("\n"),
            stderr: self
                .process
                .as_ref()
                .map(NativeProcess::stderr)
                .unwrap_or_default(),
            session_id: self.thread_id.clone(),
            model: self.model.clone(),
        })
    }

    async fn ensure_process(
        &mut self,
        config: &SessionConfig,
        control: &ExecutionControl,
        events: &EventEmitter,
    ) -> Result<()> {
        if self
            .process
            .as_mut()
            .is_some_and(|process| process.is_running())
        {
            if let Some(process) = self.process.as_ref() {
                control.pid.store(process.pid, Ordering::Relaxed);
            }
            return Ok(());
        }
        self.process = None;
        events.emit(|scope| ProviderExecutionEvent::Phase {
            scope,
            label: "正在连接 Codex app-server".to_string(),
        });
        let mut command = Command::new(&config.executable);
        command.args(["app-server", "--stdio"]);
        prepare_command(&mut command, &config.workspace);
        let mut process = NativeProcess::spawn(command)
            .await
            .context("无法启动 Codex app-server")?;
        control.pid.store(process.pid, Ordering::Relaxed);
        self.request_id = 0;
        let initialize_id = self.next_request_id();
        send_json(
            &mut process.stdin,
            &json!({
                "id": initialize_id,
                "method": "initialize",
                "params": {
                    "clientInfo": { "name": "GoodReader", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "experimentalApi": true }
                }
            }),
        )
        .await?;
        let mut transcript = Vec::new();
        read_rpc_response(&mut process, initialize_id, &mut transcript).await?;
        send_json(
            &mut process.stdin,
            &json!({ "method": "initialized", "params": {} }),
        )
        .await?;
        let thread_id = self.next_request_id();
        let resume_id = self.thread_id.as_ref().or(self.resume_session_id.as_ref());
        let request = if let Some(session_id) = resume_id {
            json!({
                "id": thread_id,
                "method": "thread/resume",
                "params": {
                    "threadId": session_id,
                    "cwd": config.workspace,
                    "approvalPolicy": "never",
                    "sandbox": "read-only"
                }
            })
        } else {
            codex_thread_start(thread_id, &config.workspace)
        };
        send_json(&mut process.stdin, &request).await?;
        let result = match read_rpc_response(&mut process, thread_id, &mut transcript).await {
            Ok(result) => result,
            Err(error) if resume_id.is_some() => {
                events.emit(|scope| ProviderExecutionEvent::Phase {
                    scope,
                    label: "原 Codex 会话不可用，正在创建新会话".to_string(),
                });
                let request_id = self.next_request_id();
                send_json(
                    &mut process.stdin,
                    &codex_thread_start(request_id, &config.workspace),
                )
                .await?;
                read_rpc_response(&mut process, request_id, &mut transcript)
                    .await
                    .with_context(|| format!("恢复 Codex 会话失败：{error}"))?
            }
            Err(error) => return Err(error),
        };
        self.thread_id = Some(
            result
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .context("Codex 没有返回 thread id")?
                .to_string(),
        );
        self.model = result
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        events.emit(|scope| ProviderExecutionEvent::SessionStateChanged {
            scope,
            provider_session_id: self.thread_id.clone().unwrap_or_default(),
            model: self.model.clone(),
            status: "idle".to_string(),
        });
        self.process = Some(process);
        Ok(())
    }

    fn next_request_id(&mut self) -> u64 {
        self.request_id += 1;
        self.request_id
    }

    async fn dispose(&mut self) {
        if let Some(mut process) = self.process.take() {
            process.terminate().await;
        }
    }
}

struct ClaudeSession {
    process: Option<NativeProcess>,
    session_id: Option<String>,
    model: Option<String>,
}

impl ClaudeSession {
    fn new(config: &SessionConfig) -> Self {
        Self {
            process: None,
            session_id: config.resume_session_id.clone(),
            model: None,
        }
    }

    async fn execute(
        &mut self,
        config: &SessionConfig,
        instruction: &str,
        control: &ExecutionControl,
        events: &EventEmitter,
    ) -> Result<ProviderOutput> {
        self.ensure_process(config, control, events).await?;
        let message = json!({
            "type": "user",
            "message": { "role": "user", "content": instruction },
            "parent_tool_use_id": null,
            "session_id": self.session_id.clone().unwrap_or_default(),
            "uuid": Uuid::new_v4().to_string()
        });
        let process = self
            .process
            .as_mut()
            .context("Claude Code 会话进程不可用")?;
        send_json(&mut process.stdin, &message).await?;
        events.emit(|scope| ProviderExecutionEvent::Phase {
            scope,
            label: "Claude Code 正在阅读书籍并组织回答".to_string(),
        });
        let mut transcript = Vec::new();
        let mut answer = String::new();
        let mut saw_delta = false;
        let mut active_tools = HashMap::<u64, (String, String)>::new();
        let mut cancel = control.subscribe();
        let mut result_error = None;
        loop {
            tokio::select! {
                changed = cancel.changed() => {
                    if changed.is_ok() && *cancel.borrow() {
                        if let Some(mut process) = self.process.take() {
                            process.terminate().await;
                        }
                        bail!("Agent 请求已取消");
                    }
                }
                line = self.process.as_mut().context("Claude Code 会话进程不可用")?.lines.next_line() => {
                    let Some(line) = line.context("读取 Claude Code 事件失败")? else {
                        let stderr = self.process.as_ref().map(NativeProcess::stderr).unwrap_or_default();
                        self.process = None;
                        bail!("Claude Code 会话意外退出：{stderr}");
                    };
                    transcript.push(line.clone());
                    let value: Value = serde_json::from_str(&line).context("Claude Code 返回了无效 stream-json 消息")?;
                    match value.get("type").and_then(Value::as_str).unwrap_or_default() {
                        "system" if value.get("subtype").and_then(Value::as_str) == Some("init") => {
                            if let Some(session_id) = value.get("session_id").and_then(Value::as_str) {
                                self.session_id = Some(session_id.to_string());
                            }
                            self.model = value.get("model").and_then(Value::as_str).map(str::to_string);
                            if let Some(session_id) = self.session_id.clone() {
                                events.emit(|scope| ProviderExecutionEvent::SessionStateChanged {
                                    scope,
                                    provider_session_id: session_id,
                                    model: self.model.clone(),
                                    status: "executing".to_string(),
                                });
                            }
                        }
                        "stream_event" => {
                            let event = value.get("event").unwrap_or(&Value::Null);
                            match event.get("type").and_then(Value::as_str).unwrap_or_default() {
                                "content_block_start" => {
                                    let block = event.get("content_block").unwrap_or(&Value::Null);
                                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                                        let index = event.get("index").and_then(Value::as_u64).unwrap_or_default();
                                        let tool_call_id = block.get("id").and_then(Value::as_str).unwrap_or("tool").to_string();
                                        let name = block.get("name").and_then(Value::as_str).unwrap_or("工具").to_string();
                                        active_tools.insert(index, (tool_call_id.clone(), name.clone()));
                                        events.emit(|scope| ProviderExecutionEvent::ToolStarted {
                                            scope,
                                            tool_call_id,
                                            name,
                                        });
                                    }
                                }
                                "content_block_stop" => {
                                    let index = event.get("index").and_then(Value::as_u64).unwrap_or_default();
                                    if let Some((tool_call_id, name)) = active_tools.remove(&index) {
                                        events.emit(|scope| ProviderExecutionEvent::ToolCompleted {
                                            scope,
                                            tool_call_id,
                                            name,
                                            is_error: false,
                                        });
                                    }
                                }
                                _ => {}
                            }
                            match event.pointer("/delta/type").and_then(Value::as_str).unwrap_or_default() {
                                "text_delta" => {
                                    if let Some(text) = event.pointer("/delta/text").and_then(Value::as_str) {
                                        saw_delta = true;
                                        answer.push_str(text);
                                        events.emit(|scope| ProviderExecutionEvent::TextDelta { scope, text: text.to_string() });
                                    }
                                }
                                "thinking_delta" => {
                                    if let Some(text) = event.pointer("/delta/thinking").and_then(Value::as_str) {
                                        events.emit(|scope| ProviderExecutionEvent::ThinkingDelta { scope, text: text.to_string() });
                                    }
                                }
                                _ => {}
                            }
                        }
                        "assistant" if !saw_delta => {
                            if let Some(text) = claude_message_text(&value) {
                                answer.push_str(&text);
                                events.emit(|scope| ProviderExecutionEvent::TextDelta { scope, text });
                            }
                        }
                        "result" => {
                            if answer.trim().is_empty() {
                                if let Some(text) = value.get("result").and_then(Value::as_str) {
                                    answer = text.to_string();
                                    events.emit(|scope| ProviderExecutionEvent::TextDelta { scope, text: text.to_string() });
                                }
                            }
                            if value.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                                result_error = value.get("result").and_then(Value::as_str).map(str::to_string);
                            }
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(error) = result_error {
            bail!("Claude Code 执行失败：{error}");
        }
        if answer.trim().is_empty() {
            bail!("Claude Code 没有返回回答");
        }
        Ok(ProviderOutput {
            answer: answer.trim().to_string(),
            transcript: transcript.join("\n"),
            stderr: self
                .process
                .as_ref()
                .map(NativeProcess::stderr)
                .unwrap_or_default(),
            session_id: self.session_id.clone(),
            model: self.model.clone(),
        })
    }

    async fn ensure_process(
        &mut self,
        config: &SessionConfig,
        control: &ExecutionControl,
        events: &EventEmitter,
    ) -> Result<()> {
        if self
            .process
            .as_mut()
            .is_some_and(|process| process.is_running())
        {
            if let Some(process) = self.process.as_ref() {
                control.pid.store(process.pid, Ordering::Relaxed);
            }
            return Ok(());
        }
        self.process = None;
        events.emit(|scope| ProviderExecutionEvent::Phase {
            scope,
            label: if self.session_id.is_some() {
                "正在恢复 Claude Code 持久会话".to_string()
            } else {
                "正在创建 Claude Code 持久会话".to_string()
            },
        });
        let mut command = Command::new(&config.executable);
        command.args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--replay-user-messages",
            "--permission-mode",
            "plan",
            "--tools",
            "Read,Grep,Glob",
        ]);
        if let Some(session_id) = self.session_id.as_ref() {
            command.arg("--resume").arg(session_id);
        }
        prepare_command(&mut command, &config.workspace);
        let process = NativeProcess::spawn(command)
            .await
            .context("无法启动 Claude Code 持久会话")?;
        control.pid.store(process.pid, Ordering::Relaxed);
        self.process = Some(process);
        Ok(())
    }

    async fn dispose(&mut self) {
        if let Some(mut process) = self.process.take() {
            process.terminate().await;
        }
    }
}

struct OpenCodeSession {
    process: Option<NativeProcess>,
    session_id: Option<String>,
}

impl OpenCodeSession {
    fn new(config: &SessionConfig) -> Self {
        Self {
            process: None,
            session_id: config.resume_session_id.clone(),
        }
    }

    async fn execute(
        &mut self,
        config: &SessionConfig,
        instruction: &str,
        control: &ExecutionControl,
        events: &EventEmitter,
    ) -> Result<ProviderOutput> {
        self.dispose().await;
        events.emit(|scope| ProviderExecutionEvent::Phase {
            scope,
            label: if self.session_id.is_some() {
                "正在恢复 OpenCode 会话".to_string()
            } else {
                "正在启动 OpenCode 非交互会话".to_string()
            },
        });
        let mut command = Command::new(&config.executable);
        command.args(["run", "--format", "json"]);
        command.args(&config.arguments);
        if let Some(session_id) = self.session_id.as_ref() {
            command.arg("--session").arg(session_id);
        }
        command.arg(instruction);
        prepare_command(&mut command, &config.workspace);
        let process = NativeProcess::spawn(command)
            .await
            .context("无法启动 OpenCode 非交互会话")?;
        control.pid.store(process.pid, Ordering::Relaxed);
        self.process = Some(process);

        let mut transcript = Vec::new();
        let mut answer = String::new();
        let mut active_tools = HashMap::<String, String>::new();
        let mut announced_session = false;
        let mut terminal_error = None;
        let mut cancel = control.subscribe();
        loop {
            tokio::select! {
                changed = cancel.changed() => {
                    if changed.is_ok() && *cancel.borrow() {
                        self.dispose().await;
                        bail!("Agent 请求已取消");
                    }
                }
                line = self.process.as_mut().context("OpenCode 会话进程不可用")?.lines.next_line() => {
                    let Some(line) = line.context("读取 OpenCode 事件失败")? else { break };
                    transcript.push(line.clone());
                    let value: Value = match serde_json::from_str(&line) {
                        Ok(value) => value,
                        Err(error) => {
                            terminal_error = Some(format!("OpenCode 返回了无效 JSON 事件：{error}"));
                            break;
                        }
                    };
                    if let Some(session_id) = value.get("sessionID").and_then(Value::as_str) {
                        self.session_id = Some(session_id.to_string());
                        if !announced_session {
                            announced_session = true;
                            events.emit(|scope| ProviderExecutionEvent::SessionStateChanged {
                                scope,
                                provider_session_id: session_id.to_string(),
                                model: None,
                                status: "executing".to_string(),
                            });
                        }
                    }
                    match value.get("type").and_then(Value::as_str).unwrap_or_default() {
                        "step_start" => events.emit(|scope| ProviderExecutionEvent::Phase {
                            scope,
                            label: "OpenCode 正在阅读书籍并组织回答".to_string(),
                        }),
                        "text" => {
                            if let Some(text) = value.pointer("/part/text").and_then(Value::as_str) {
                                answer.push_str(text);
                                events.emit(|scope| ProviderExecutionEvent::TextDelta {
                                    scope,
                                    text: text.to_string(),
                                });
                            }
                        }
                        "error" => {
                            terminal_error = Some(
                                value.get("error")
                                    .and_then(Value::as_str)
                                    .unwrap_or("OpenCode 执行失败")
                                    .to_string(),
                            );
                            break;
                        }
                        "step_finish" => break,
                        _ => {
                            if let Some((tool_call_id, name, status, is_error)) = opencode_tool(&value) {
                                if matches!(status, "pending" | "running") {
                                    if active_tools.insert(tool_call_id.clone(), name.clone()).is_none() {
                                        events.emit(|scope| ProviderExecutionEvent::ToolStarted {
                                            scope,
                                            tool_call_id,
                                            name,
                                        });
                                    }
                                } else if matches!(status, "completed" | "error") {
                                    if active_tools.remove(&tool_call_id).is_none() {
                                        events.emit(|scope| ProviderExecutionEvent::ToolStarted {
                                            scope,
                                            tool_call_id: tool_call_id.clone(),
                                            name: name.clone(),
                                        });
                                    }
                                    events.emit(|scope| ProviderExecutionEvent::ToolCompleted {
                                        scope,
                                        tool_call_id,
                                        name,
                                        is_error,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut process = self.process.take().context("OpenCode 会话进程不可用")?;
        let status = process
            .child
            .wait()
            .await
            .context("等待 OpenCode 退出失败")?;
        let stderr = process.stderr();
        if let Some(error) = terminal_error {
            bail!("{error}");
        }
        if !status.success() {
            let detail = if stderr.is_empty() {
                format!("退出状态：{status}")
            } else {
                stderr.clone()
            };
            bail!("OpenCode 执行失败：{detail}");
        }
        if answer.trim().is_empty() {
            bail!("OpenCode 没有返回回答");
        }
        Ok(ProviderOutput {
            answer: answer.trim().to_string(),
            transcript: transcript.join("\n"),
            stderr,
            session_id: self.session_id.clone(),
            model: None,
        })
    }

    async fn dispose(&mut self) {
        if let Some(mut process) = self.process.take() {
            process.terminate().await;
        }
    }
}

struct NativeProcess {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    pid: u32,
}

fn opencode_tool(value: &Value) -> Option<(String, String, &str, bool)> {
    let part = value.get("part")?;
    if part.get("type").and_then(Value::as_str) != Some("tool") {
        return None;
    }
    let id = part.get("id").and_then(Value::as_str)?.to_string();
    let name = part
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("工具")
        .to_string();
    let status = part
        .pointer("/state/status")
        .and_then(Value::as_str)
        .unwrap_or("running");
    Some((id, name, status, status == "error"))
}

impl NativeProcess {
    async fn spawn(mut command: Command) -> Result<Self> {
        let mut child = command.spawn()?;
        let pid = child.id().context("无法取得 Agent 进程标识")?;
        let stdin = child.stdin.take().context("无法连接 Agent 输入")?;
        let stdout = child.stdout.take().context("无法读取 Agent 输出")?;
        let stderr_stream = child.stderr.take().context("无法读取 Agent 错误输出")?;
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_for_task = stderr.clone();
        tokio::spawn(async move {
            let mut stream = stderr_stream;
            let mut buffer = [0_u8; 4096];
            loop {
                match stream.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => stderr_for_task
                        .lock()
                        .expect("Agent stderr 锁")
                        .extend_from_slice(&buffer[..read]),
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            stderr,
            pid,
        })
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().expect("Agent stderr 锁"))
            .trim()
            .to_string()
    }

    async fn terminate(&mut self) {
        terminate_process_group(self.pid);
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
        let _ = self.child.kill().await;
    }
}

fn prepare_command(command: &mut Command, workspace: &Path) {
    command.current_dir(workspace);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command.as_std_mut(), 0);
}

async fn send_json(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    stdin
        .write_all(serde_json::to_string(value)?.as_bytes())
        .await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_rpc_response(
    process: &mut NativeProcess,
    id: u64,
    transcript: &mut Vec<String>,
) -> Result<Value> {
    loop {
        let line = process
            .lines
            .next_line()
            .await
            .context("读取 Codex JSON-RPC 响应失败")?
            .context("Codex app-server 提前退出")?;
        transcript.push(line.clone());
        let value: Value = serde_json::from_str(&line).context("Codex 返回了无效 JSON-RPC 消息")?;
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            if let Some(error) = value.get("error") {
                bail!("Codex JSON-RPC 请求失败：{error}");
            }
            return value
                .get("result")
                .cloned()
                .context("Codex 响应缺少 result");
        }
        if value.get("id").is_some() && value.get("method").is_some() {
            reject_server_request(&mut process.stdin, &value).await?;
        }
    }
}

async fn reject_server_request(stdin: &mut ChildStdin, request: &Value) -> Result<()> {
    let Some(id) = request.get("id") else {
        return Ok(());
    };
    send_json(
        stdin,
        &json!({
            "id": id,
            "error": { "code": -32601, "message": "GoodReader 只读会话不接受交互式请求" }
        }),
    )
    .await
}

fn codex_thread_start(id: u64, workspace: &Path) -> Value {
    json!({
        "id": id,
        "method": "thread/start",
        "params": {
            "cwd": workspace,
            "approvalPolicy": "never",
            "sandbox": "read-only",
            "baseInstructions": "你是 GoodReader 的书籍问答 Agent。只读取当前工作区提供的书籍、历史和标注，不修改任何文件。"
        }
    })
}

fn codex_tool(params: &Value) -> Option<(String, String)> {
    let item = params.get("item")?;
    let kind = item.get("type")?.as_str()?;
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(kind)
        .to_string();
    let name = match kind {
        "commandExecution" => "命令".to_string(),
        "mcpToolCall" => item
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("MCP 工具")
            .to_string(),
        "webSearch" => "网页检索".to_string(),
        _ => return None,
    };
    Some((id, name))
}

fn codex_final_answer(params: &Value) -> Option<String> {
    params
        .pointer("/turn/items")?
        .as_array()?
        .iter()
        .rev()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("agentMessage"))?
        .get("text")?
        .as_str()
        .map(str::to_string)
}

fn claude_message_text(value: &Value) -> Option<String> {
    let content = value.pointer("/message/content")?.as_array()?;
    let answer = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<String>();
    (!answer.is_empty()).then_some(answer)
}

fn classify_error(error: &anyhow::Error) -> String {
    let message = format!("{error:#}").to_lowercase();
    if message.contains("auth") || message.contains("login") || message.contains("认证") {
        "authentication"
    } else if message.contains("提前退出") || message.contains("意外退出") {
        "process-exited"
    } else if message.contains("json-rpc") || message.contains("stream-json") {
        "transport"
    } else {
        "provider"
    }
    .to_string()
}

fn terminate_process_group(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::{AgentSessionHost, ProviderKind, SessionConfig, SessionKey};

    #[tokio::test]
    async fn codex_session_reuses_one_process_for_multiple_turns() {
        let temp = TempDir::new().unwrap();
        let executable = temp.path().join("fake-codex");
        let launches = temp.path().join("launches");
        fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
echo launch >> '{}'
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) echo '{{"id":1,"result":{{}}}}' ;;
    *'"method":"thread/start"'*) echo '{{"id":2,"result":{{"thread":{{"id":"thread-1"}},"model":"test"}}}}' ;;
    *'"method":"turn/start"'*)
      id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
      echo "{{\"id\":$id,\"result\":{{\"turn\":{{\"id\":\"turn-$id\"}}}}}}"
      echo '{{"method":"item/agentMessage/delta","params":{{"delta":"回答"}}}}'
      echo "{{\"method\":\"turn/completed\",\"params\":{{\"turn\":{{\"id\":\"turn-$id\",\"status\":\"completed\"}}}}}}"
      ;;
  esac
done
"#,
                launches.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let host = AgentSessionHost::new();
        let config = SessionConfig {
            key: SessionKey {
                book_id: "book".to_string(),
                runtime_id: "codex".to_string(),
            },
            provider: ProviderKind::Codex,
            executable,
            arguments: Vec::new(),
            workspace: temp.path().to_path_buf(),
            resume_session_id: None,
        };
        let mut session_instance_id = None;
        let mut execution_ids = Vec::new();
        for _ in 0..2 {
            let mut run = host.execute(
                config.clone(),
                "回答问题".to_string(),
                super::ExecutionControl::new(),
            );
            let mut first_sequence = None;
            while let Some(event) = run.events.recv().await {
                let scope = event.scope();
                first_sequence.get_or_insert(scope.sequence);
                if let Some(expected) = session_instance_id.as_ref() {
                    assert_eq!(&scope.session_instance_id, expected);
                } else {
                    session_instance_id = Some(scope.session_instance_id.clone());
                }
                execution_ids.push(scope.execution_id.clone());
            }
            assert_eq!(first_sequence, Some(1));
            assert_eq!(run.finish().await.unwrap().answer, "回答");
        }
        assert_eq!(fs::read_to_string(launches).unwrap().lines().count(), 1);
        execution_ids.dedup();
        assert_eq!(execution_ids.len(), 2);
    }

    #[tokio::test]
    async fn claude_session_keeps_stream_json_process_alive_between_turns() {
        let temp = TempDir::new().unwrap();
        let executable = temp.path().join("fake-claude");
        let launches = temp.path().join("launches");
        fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
echo launch >> '{}'
initialized=0
while IFS= read -r line; do
  if [ "$initialized" -eq 0 ]; then
    echo '{{"type":"system","subtype":"init","session_id":"session-1","model":"test"}}'
    initialized=1
  fi
  echo '{{"type":"stream_event","event":{{"type":"content_block_start","index":1,"content_block":{{"type":"tool_use","id":"tool-1","name":"Read"}}}}}}'
  echo '{{"type":"stream_event","event":{{"type":"content_block_stop","index":1}}}}'
  echo '{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"回答"}}}}}}'
  echo '{{"type":"result","subtype":"success","is_error":false,"result":"回答","session_id":"session-1"}}'
done
"#,
                launches.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let host = AgentSessionHost::new();
        let config = SessionConfig {
            key: SessionKey {
                book_id: "book".to_string(),
                runtime_id: "claude".to_string(),
            },
            provider: ProviderKind::Claude,
            executable,
            arguments: Vec::new(),
            workspace: temp.path().to_path_buf(),
            resume_session_id: None,
        };
        for _ in 0..2 {
            let mut run = host.execute(
                config.clone(),
                "回答问题".to_string(),
                super::ExecutionControl::new(),
            );
            let mut tool_started = false;
            let mut tool_completed = false;
            while let Some(event) = run.events.recv().await {
                tool_started |= matches!(event, super::ProviderExecutionEvent::ToolStarted { .. });
                tool_completed |=
                    matches!(event, super::ProviderExecutionEvent::ToolCompleted { .. });
            }
            assert!(tool_started);
            assert!(tool_completed);
            assert_eq!(run.finish().await.unwrap().answer, "回答");
        }
        assert_eq!(fs::read_to_string(launches).unwrap().lines().count(), 1);
    }

    #[tokio::test]
    async fn opencode_session_uses_json_mode_and_resumes_native_session() {
        let temp = TempDir::new().unwrap();
        let executable = temp.path().join("opencode");
        let arguments = temp.path().join("arguments");
        fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
echo '{{"type":"step_start","sessionID":"session-1","part":{{"type":"step-start"}}}}'
echo '{{"type":"text","sessionID":"session-1","part":{{"type":"text","text":"回答"}}}}'
echo '{{"type":"step_finish","sessionID":"session-1","part":{{"type":"step-finish","reason":"stop"}}}}'
"#,
                arguments.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let host = AgentSessionHost::new();
        let config = SessionConfig {
            key: SessionKey {
                book_id: "book".to_string(),
                runtime_id: "opencode".to_string(),
            },
            provider: ProviderKind::OpenCode,
            executable,
            arguments: Vec::new(),
            workspace: temp.path().to_path_buf(),
            resume_session_id: None,
        };
        for _ in 0..2 {
            let mut run = host.execute(
                config.clone(),
                "回答问题".to_string(),
                super::ExecutionControl::new(),
            );
            while run.events.recv().await.is_some() {}
            let output = run.finish().await.unwrap();
            assert_eq!(output.answer, "回答");
            assert_eq!(output.session_id.as_deref(), Some("session-1"));
        }
        let invocations = fs::read_to_string(arguments).unwrap();
        let lines = invocations.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("run --format json "));
        assert!(lines[1].contains("--session session-1"));
    }
}

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{Local, Utc};
use rusqlite::backup::Backup;
use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use crate::models::{
    AgentSession, AgentTask, AiMessage, Annotation, AnnotationKind, BackupInfo, CreateAnnotation,
    CustomAgentRuntime, Progress, SaveProgress,
};

const SCHEMA_VERSION: i64 = 3;
const BACKUP_LIMIT: usize = 7;
const HIGHLIGHT_COLORS: [&str; 4] = ["yellow", "green", "blue", "pink"];

pub struct Database {
    connection: Mutex<Connection>,
    backups_dir: PathBuf,
}

impl Database {
    pub fn open(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir)
            .with_context(|| format!("无法创建数据目录 {}", data_dir.display()))?;
        let backups_dir = data_dir.join("Backups");
        fs::create_dir_all(&backups_dir)
            .with_context(|| format!("无法创建备份目录 {}", backups_dir.display()))?;
        let path = data_dir.join("goodreader.sqlite3");
        let connection =
            Connection::open(&path).with_context(|| format!("无法打开 {}", path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(Duration::from_secs(5))?;

        let database = Self {
            connection: Mutex::new(connection),
            backups_dir,
        };
        database.initialize()?;
        Ok(database)
    }

    fn initialize(&self) -> Result<()> {
        let connection = self.connection.lock().expect("数据库互斥锁");
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            bail!("数据库版本 {version} 高于应用支持的 {SCHEMA_VERSION}");
        }
        connection.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS progress (
                book_id TEXT PRIMARY KEY NOT NULL,
                chapter_id TEXT NOT NULL,
                block_id TEXT,
                chapter_progress REAL NOT NULL CHECK(chapter_progress >= 0 AND chapter_progress <= 1),
                overall_progress REAL NOT NULL CHECK(overall_progress >= 0 AND overall_progress <= 1),
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS annotations (
                id TEXT PRIMARY KEY NOT NULL,
                book_id TEXT NOT NULL,
                chapter_id TEXT NOT NULL,
                block_id TEXT NOT NULL,
                start_offset INTEGER NOT NULL CHECK(start_offset >= 0),
                end_offset INTEGER NOT NULL CHECK(end_offset > start_offset),
                quote TEXT NOT NULL,
                kind TEXT NOT NULL CHECK(kind IN ('highlight', 'note', 'bookmark')),
                color TEXT,
                note TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE UNIQUE INDEX IF NOT EXISTS annotation_exact_unique
            ON annotations(book_id, chapter_id, block_id, start_offset, end_offset, kind);

            CREATE INDEX IF NOT EXISTS annotation_book_created
            ON annotations(book_id, created_at DESC);

            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS agent_runtimes (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                executable TEXT NOT NULL,
                arguments_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS agent_tasks (
                id TEXT PRIMARY KEY NOT NULL,
                book_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                goal TEXT NOT NULL,
                current_runtime_id TEXT NOT NULL,
                error TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS agent_task_book_updated
            ON agent_tasks(book_id, updated_at DESC);

            CREATE TABLE IF NOT EXISTS agent_executions (
                id TEXT PRIMARY KEY NOT NULL,
                task_id TEXT NOT NULL REFERENCES agent_tasks(id) ON DELETE CASCADE,
                runtime_id TEXT NOT NULL,
                status TEXT NOT NULL,
                output TEXT,
                error TEXT,
                started_at INTEGER NOT NULL,
                finished_at INTEGER
            );

            CREATE TABLE IF NOT EXISTS ai_messages (
                id TEXT PRIMARY KEY NOT NULL,
                book_id TEXT NOT NULL,
                task_id TEXT NOT NULL REFERENCES agent_tasks(id) ON DELETE CASCADE,
                role TEXT NOT NULL CHECK(role IN ('user', 'assistant')),
                content TEXT NOT NULL,
                runtime_id TEXT,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS ai_message_book_created
            ON ai_messages(book_id, created_at ASC);

            CREATE TABLE IF NOT EXISTS agent_sessions (
                book_id TEXT NOT NULL,
                runtime_id TEXT NOT NULL,
                provider_session_id TEXT NOT NULL,
                provider_state_json TEXT NOT NULL DEFAULT '{}',
                updated_at INTEGER NOT NULL,
                PRIMARY KEY(book_id, runtime_id)
            );

            CREATE UNIQUE INDEX IF NOT EXISTS agent_tasks_one_active_per_book
                ON agent_tasks(book_id) WHERE status NOT IN ('completed', 'stopped');

            PRAGMA user_version = 3;
            "#,
        )?;
        Ok(())
    }

    pub fn all_progress(&self) -> Result<Vec<Progress>> {
        let connection = self.connection.lock().expect("数据库互斥锁");
        let mut statement = connection.prepare(
            "SELECT book_id, chapter_id, block_id, chapter_progress, overall_progress, updated_at
             FROM progress",
        )?;
        let rows = statement.query_map([], map_progress)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("读取阅读进度失败")
    }

    pub fn progress(&self, book_id: &str) -> Result<Option<Progress>> {
        let connection = self.connection.lock().expect("数据库互斥锁");
        connection
            .query_row(
                "SELECT book_id, chapter_id, block_id, chapter_progress, overall_progress, updated_at
                 FROM progress WHERE book_id = ?1",
                [book_id],
                map_progress,
            )
            .optional()
            .context("读取阅读进度失败")
    }

    pub fn save_progress(&self, book_id: &str, progress: &SaveProgress) -> Result<Progress> {
        if !(0.0..=1.0).contains(&progress.chapter_progress)
            || !(0.0..=1.0).contains(&progress.overall_progress)
        {
            bail!("阅读进度必须位于 0 到 1 之间");
        }
        if progress.chapter_id.trim().is_empty() {
            bail!("章节 ID 不能为空");
        }
        let now = Utc::now().timestamp_millis();
        let connection = self.connection.lock().expect("数据库互斥锁");
        connection.execute(
            r#"
            INSERT INTO progress(
                book_id, chapter_id, block_id, chapter_progress, overall_progress, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(book_id) DO UPDATE SET
                chapter_id = excluded.chapter_id,
                block_id = excluded.block_id,
                chapter_progress = excluded.chapter_progress,
                overall_progress = excluded.overall_progress,
                updated_at = excluded.updated_at
            "#,
            params![
                book_id,
                progress.chapter_id,
                progress.block_id,
                progress.chapter_progress,
                progress.overall_progress,
                now
            ],
        )?;
        Ok(Progress {
            book_id: book_id.to_string(),
            chapter_id: progress.chapter_id.clone(),
            block_id: progress.block_id.clone(),
            chapter_progress: progress.chapter_progress,
            overall_progress: progress.overall_progress,
            updated_at: now,
        })
    }

    pub fn annotations(&self, book_id: &str) -> Result<Vec<Annotation>> {
        let connection = self.connection.lock().expect("数据库互斥锁");
        let mut statement = connection.prepare(
            r#"
            SELECT id, book_id, chapter_id, block_id, start_offset, end_offset, quote,
                   kind, color, note, created_at, updated_at
            FROM annotations
            WHERE book_id = ?1
            ORDER BY created_at DESC
            "#,
        )?;
        let rows = statement.query_map([book_id], map_annotation)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("读取标注失败")
    }

    pub fn create_annotation(&self, book_id: &str, input: &CreateAnnotation) -> Result<Annotation> {
        validate_annotation(input)?;
        let connection = self.connection.lock().expect("数据库互斥锁");

        let exact = connection
            .query_row(
                r#"
                SELECT id, book_id, chapter_id, block_id, start_offset, end_offset, quote,
                       kind, color, note, created_at, updated_at
                FROM annotations
                WHERE book_id = ?1 AND chapter_id = ?2 AND block_id = ?3
                  AND start_offset = ?4 AND end_offset = ?5 AND kind = ?6
                "#,
                params![
                    book_id,
                    input.chapter_id,
                    input.block_id,
                    input.start_offset,
                    input.end_offset,
                    input.kind.as_str()
                ],
                map_annotation,
            )
            .optional()?;
        if let Some(existing) = exact {
            return Ok(existing);
        }

        if input.kind == AnnotationKind::Highlight {
            let overlap: bool = connection.query_row(
                r#"
                SELECT EXISTS(
                    SELECT 1 FROM annotations
                    WHERE book_id = ?1 AND chapter_id = ?2 AND block_id = ?3
                      AND kind = 'highlight'
                      AND NOT (?5 <= start_offset OR ?4 >= end_offset)
                )
                "#,
                params![
                    book_id,
                    input.chapter_id,
                    input.block_id,
                    input.start_offset,
                    input.end_offset
                ],
                |row| row.get(0),
            )?;
            if overlap {
                bail!("选区与已有高亮重叠");
            }
        }

        let now = Utc::now().timestamp_millis();
        let id = Uuid::new_v4().to_string();
        connection.execute(
            r#"
            INSERT INTO annotations(
                id, book_id, chapter_id, block_id, start_offset, end_offset, quote,
                kind, color, note, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
            "#,
            params![
                id,
                book_id,
                input.chapter_id,
                input.block_id,
                input.start_offset,
                input.end_offset,
                input.quote,
                input.kind.as_str(),
                input.color,
                input.note,
                now
            ],
        )?;

        Ok(Annotation {
            id,
            book_id: book_id.to_string(),
            chapter_id: input.chapter_id.clone(),
            block_id: input.block_id.clone(),
            start_offset: input.start_offset,
            end_offset: input.end_offset,
            quote: input.quote.clone(),
            kind: input.kind,
            color: input.color.clone(),
            note: input.note.clone(),
            created_at: now,
            updated_at: now,
        })
    }

    pub fn update_note(&self, annotation_id: &str, note: &str) -> Result<Annotation> {
        let note = note.trim();
        if note.is_empty() {
            bail!("笔记内容不能为空");
        }
        let now = Utc::now().timestamp_millis();
        let connection = self.connection.lock().expect("数据库互斥锁");
        let changed = connection.execute(
            "UPDATE annotations SET note = ?1, updated_at = ?2
             WHERE id = ?3 AND kind = 'note'",
            params![note, now, annotation_id],
        )?;
        if changed == 0 {
            bail!("找不到可编辑的笔记");
        }
        connection
            .query_row(
                r#"
                SELECT id, book_id, chapter_id, block_id, start_offset, end_offset, quote,
                       kind, color, note, created_at, updated_at
                FROM annotations WHERE id = ?1
                "#,
                [annotation_id],
                map_annotation,
            )
            .context("读取更新后的笔记失败")
    }

    pub fn delete_annotation(&self, annotation_id: &str) -> Result<bool> {
        let connection = self.connection.lock().expect("数据库互斥锁");
        Ok(connection.execute("DELETE FROM annotations WHERE id = ?1", [annotation_id])? > 0)
    }

    pub fn forget_book(&self, book_id: &str) -> Result<(usize, usize)> {
        let mut connection = self.connection.lock().expect("数据库互斥锁");
        let transaction = connection.transaction()?;
        let progress = transaction.execute("DELETE FROM progress WHERE book_id = ?1", [book_id])?;
        let annotations =
            transaction.execute("DELETE FROM annotations WHERE book_id = ?1", [book_id])?;
        transaction.execute("DELETE FROM agent_tasks WHERE book_id = ?1", [book_id])?;
        transaction.execute("DELETE FROM agent_sessions WHERE book_id = ?1", [book_id])?;
        transaction.commit()?;
        Ok((progress, annotations))
    }

    pub fn clear_ai_workspace(&self, book_id: &str) -> Result<usize> {
        let mut connection = self.connection.lock().expect("数据库互斥锁");
        let transaction = connection.transaction()?;
        let removed =
            transaction.execute("DELETE FROM agent_tasks WHERE book_id = ?1", [book_id])?;
        transaction.execute("DELETE FROM agent_sessions WHERE book_id = ?1", [book_id])?;
        transaction.commit()?;
        Ok(removed)
    }

    pub fn agent_session(&self, book_id: &str, runtime_id: &str) -> Result<Option<AgentSession>> {
        let connection = self.connection.lock().expect("数据库互斥锁");
        connection
            .query_row(
                "SELECT book_id, runtime_id, provider_session_id, provider_state_json, updated_at
                 FROM agent_sessions WHERE book_id = ?1 AND runtime_id = ?2",
                params![book_id, runtime_id],
                |row| {
                    Ok(AgentSession {
                        book_id: row.get(0)?,
                        runtime_id: row.get(1)?,
                        provider_session_id: row.get(2)?,
                        provider_state_json: row.get(3)?,
                        updated_at: row.get(4)?,
                    })
                },
            )
            .optional()
            .context("读取 Agent 会话失败")
    }

    pub fn save_agent_session(
        &self,
        book_id: &str,
        runtime_id: &str,
        provider_session_id: &str,
        provider_state_json: &str,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        let connection = self.connection.lock().expect("数据库互斥锁");
        connection.execute(
            "INSERT INTO agent_sessions(
                book_id, runtime_id, provider_session_id, provider_state_json, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(book_id, runtime_id) DO UPDATE SET
                provider_session_id = excluded.provider_session_id,
                provider_state_json = excluded.provider_state_json,
                updated_at = excluded.updated_at",
            params![
                book_id,
                runtime_id,
                provider_session_id,
                provider_state_json,
                now
            ],
        )?;
        Ok(())
    }

    pub fn create_question_task(
        &self,
        book_id: &str,
        runtime_id: &str,
        goal: &str,
    ) -> Result<AgentTask> {
        let goal = goal.trim();
        if goal.is_empty() {
            bail!("问题不能为空");
        }
        if goal.chars().count() > 20_000 {
            bail!("问题过长");
        }
        let now = Utc::now().timestamp_millis();
        let task_id = Uuid::new_v4().to_string();
        let message_id = Uuid::new_v4().to_string();
        let mut connection = self.connection.lock().expect("数据库互斥锁");
        let transaction = connection.transaction()?;
        transaction
            .execute(
                "INSERT INTO agent_tasks(
                    id, book_id, kind, status, goal, current_runtime_id, error, created_at, updated_at
                 ) VALUES (?1, ?2, 'question', 'queued', ?3, ?4, NULL, ?5, ?5)",
                params![task_id, book_id, goal, runtime_id, now],
            )
            .map_err(|error| {
                if error.to_string().contains("UNIQUE constraint") {
                    anyhow!("这本书已有正在运行的 AI 请求，请先等待完成或停止当前请求")
                } else {
                    anyhow::Error::from(error)
                }
            })?;
        transaction.execute(
            "INSERT INTO ai_messages(
                id, book_id, task_id, role, content, runtime_id, created_at
             ) VALUES (?1, ?2, ?3, 'user', ?4, NULL, ?5)",
            params![message_id, book_id, task_id, goal, now],
        )?;
        transaction.commit()?;
        drop(connection);
        self.agent_task(&task_id)?
            .context("创建 Agent 任务后无法重新读取")
    }

    pub fn agent_task(&self, task_id: &str) -> Result<Option<AgentTask>> {
        let connection = self.connection.lock().expect("数据库互斥锁");
        connection
            .query_row(
                "SELECT id, book_id, kind, status, goal, current_runtime_id, error,
                        created_at, updated_at
                 FROM agent_tasks WHERE id = ?1",
                [task_id],
                map_agent_task,
            )
            .optional()
            .context("读取 Agent 任务失败")
    }

    pub fn active_agent_tasks(&self, book_id: &str) -> Result<Vec<AgentTask>> {
        let connection = self.connection.lock().expect("数据库互斥锁");
        let mut statement = connection.prepare(
            "SELECT id, book_id, kind, status, goal, current_runtime_id, error,
                    created_at, updated_at
             FROM agent_tasks
             WHERE book_id = ?1 AND status NOT IN ('completed', 'stopped')
             ORDER BY created_at ASC",
        )?;
        let rows = statement.query_map([book_id], map_agent_task)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("读取活跃 Agent 任务失败")
    }

    pub fn ai_messages(&self, book_id: &str) -> Result<Vec<AiMessage>> {
        let connection = self.connection.lock().expect("数据库互斥锁");
        let mut statement = connection.prepare(
            "SELECT messages.id, messages.book_id, messages.task_id, messages.role,
                    messages.content, messages.runtime_id, messages.created_at,
                    CASE
                        WHEN messages.role = 'assistant' AND tasks.status = 'completed'
                        THEN MAX(0, tasks.updated_at - tasks.created_at)
                        ELSE NULL
                    END
             FROM ai_messages AS messages
             JOIN agent_tasks AS tasks ON tasks.id = messages.task_id
             WHERE messages.book_id = ?1
             ORDER BY messages.created_at ASC, messages.rowid ASC",
        )?;
        let rows = statement.query_map([book_id], map_ai_message)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("读取书籍 AI 历史失败")
    }

    pub fn start_agent_execution(&self, task_id: &str, runtime_id: &str) -> Result<String> {
        let now = Utc::now().timestamp_millis();
        let execution_id = Uuid::new_v4().to_string();
        let mut connection = self.connection.lock().expect("数据库互斥锁");
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE agent_tasks
             SET status = 'running', current_runtime_id = ?1, error = NULL, updated_at = ?2
             WHERE id = ?3 AND status = 'queued'",
            params![runtime_id, now, task_id],
        )?;
        if changed == 0 {
            bail!("只有排队中的 Agent 任务可以开始执行");
        }
        transaction.execute(
            "INSERT INTO agent_executions(
                id, task_id, runtime_id, status, started_at
             ) VALUES (?1, ?2, ?3, 'running', ?4)",
            params![execution_id, task_id, runtime_id, now],
        )?;
        transaction.commit()?;
        Ok(execution_id)
    }

    pub fn complete_agent_execution(
        &self,
        task_id: &str,
        execution_id: &str,
        runtime_id: &str,
        content: &str,
    ) -> Result<()> {
        let content = content.trim();
        if content.is_empty() {
            bail!("Agent 没有返回内容");
        }
        let now = Utc::now().timestamp_millis();
        let message_id = Uuid::new_v4().to_string();
        let mut connection = self.connection.lock().expect("数据库互斥锁");
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE agent_tasks SET status = 'completed', error = NULL, updated_at = ?1
             WHERE id = ?2 AND status = 'running'",
            params![now, task_id],
        )?;
        if changed == 0 {
            transaction.commit()?;
            return Ok(());
        }
        transaction.execute(
            "UPDATE agent_executions
             SET status = 'completed', output = ?1, finished_at = ?2
             WHERE id = ?3 AND status = 'running'",
            params![content, now, execution_id],
        )?;
        transaction.execute(
            "INSERT INTO ai_messages(
                id, book_id, task_id, role, content, runtime_id, created_at
             )
             SELECT ?1, book_id, id, 'assistant', ?2, ?3, ?4
             FROM agent_tasks WHERE id = ?5",
            params![message_id, content, runtime_id, now, task_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn pause_agent_execution(
        &self,
        task_id: &str,
        execution_id: Option<&str>,
        error: &str,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("数据库互斥锁");
        let transaction = connection.transaction()?;
        if let Some(execution_id) = execution_id {
            transaction.execute(
                "UPDATE agent_executions
                 SET status = 'failed', error = ?1, finished_at = ?2
                 WHERE id = ?3 AND status = 'running'",
                params![error, now, execution_id],
            )?;
        }
        transaction.execute(
            "UPDATE agent_tasks SET status = 'paused', error = ?1, updated_at = ?2
             WHERE id = ?3 AND status NOT IN ('completed', 'stopped')",
            params![error, now, task_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn stop_agent_task(&self, task_id: &str) -> Result<AgentTask> {
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("数据库互斥锁");
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE agent_executions
             SET status = 'stopped', error = NULL, finished_at = ?1
             WHERE task_id = ?2 AND status = 'running'",
            params![now, task_id],
        )?;
        let changed = transaction.execute(
            "UPDATE agent_tasks SET status = 'stopped', error = NULL, updated_at = ?1
             WHERE id = ?2 AND status IN ('queued', 'running')",
            params![now, task_id],
        )?;
        transaction.commit()?;
        drop(connection);
        let task = self
            .agent_task(task_id)?
            .context("停止 Agent 任务后无法重新读取")?;
        if changed == 0 && task.status != "stopped" {
            bail!("只有正在排队或运行的 Agent 任务可以停止");
        }
        Ok(task)
    }

    pub fn retry_agent_task(&self, task_id: &str, runtime_id: &str) -> Result<AgentTask> {
        let now = Utc::now().timestamp_millis();
        let connection = self.connection.lock().expect("数据库互斥锁");
        let changed = connection.execute(
            "UPDATE agent_tasks
             SET status = 'queued', current_runtime_id = ?1, error = NULL, updated_at = ?2
             WHERE id = ?3 AND status = 'paused'",
            params![runtime_id, now, task_id],
        )?;
        if changed == 0 {
            bail!("只有已暂停的 Agent 任务可以重试");
        }
        drop(connection);
        self.agent_task(task_id)?
            .context("重试 Agent 任务后无法重新读取")
    }

    pub fn save_custom_agent_runtime(
        &self,
        name: &str,
        executable: &str,
        arguments: &[String],
    ) -> Result<CustomAgentRuntime> {
        let name = name.trim();
        let executable = executable.trim();
        if name.is_empty() || executable.is_empty() {
            bail!("运行时名称和可执行文件不能为空");
        }
        if !Path::new(executable).is_absolute() {
            bail!("自定义 Agent 必须使用绝对路径");
        }
        if arguments.len() > 32 || arguments.iter().any(|value| value.len() > 1_024) {
            bail!("自定义 Agent 参数过多或过长");
        }
        let id = format!("custom-{}", Uuid::new_v4());
        let now = Utc::now().timestamp_millis();
        let arguments_json = serde_json::to_string(arguments)?;
        let connection = self.connection.lock().expect("数据库互斥锁");
        connection.execute(
            "INSERT INTO agent_runtimes(id, name, executable, arguments_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, name, executable, arguments_json, now],
        )?;
        Ok(CustomAgentRuntime {
            id,
            name: name.to_string(),
            executable: executable.to_string(),
            arguments: arguments.to_vec(),
        })
    }

    pub fn custom_agent_runtimes(&self) -> Result<Vec<CustomAgentRuntime>> {
        let connection = self.connection.lock().expect("数据库互斥锁");
        let mut statement = connection.prepare(
            "SELECT id, name, executable, arguments_json
             FROM agent_runtimes ORDER BY created_at ASC",
        )?;
        let rows = statement.query_map([], |row| {
            let arguments_json: String = row.get(3)?;
            let arguments = serde_json::from_str(&arguments_json).unwrap_or_default();
            Ok(CustomAgentRuntime {
                id: row.get(0)?,
                name: row.get(1)?,
                executable: row.get(2)?,
                arguments,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("读取自定义 Agent 运行时失败")
    }

    pub fn delete_custom_agent_runtime(&self, runtime_id: &str) -> Result<bool> {
        let connection = self.connection.lock().expect("数据库互斥锁");
        Ok(connection.execute("DELETE FROM agent_runtimes WHERE id = ?1", [runtime_id])? > 0)
    }

    pub fn annotation_count(&self, book_id: &str) -> Result<usize> {
        let connection = self.connection.lock().expect("数据库互斥锁");
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM annotations WHERE book_id = ?1",
            [book_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        let connection = self.connection.lock().expect("数据库互斥锁");
        connection
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .context("读取设置失败")
    }

    pub fn save_setting(&self, key: &str, value: &str) -> Result<()> {
        let connection = self.connection.lock().expect("数据库互斥锁");
        connection.execute(
            "INSERT INTO settings(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn create_backup(&self, kind: &str) -> Result<BackupInfo> {
        let timestamp = Local::now().format("%Y%m%d-%H%M%S");
        let name = format!("{kind}-{timestamp}.sqlite3");
        let path = self.backups_dir.join(&name);
        let source = self.connection.lock().expect("数据库互斥锁");
        let mut destination =
            Connection::open(&path).with_context(|| format!("无法创建备份 {}", path.display()))?;
        let backup = Backup::new(&source, &mut destination)?;
        backup.run_to_completion(64, Duration::from_millis(20), None)?;
        drop(backup);
        drop(destination);
        drop(source);
        self.trim_backups()?;
        self.backup_info(&path)
    }

    pub fn ensure_daily_backup(&self) -> Result<()> {
        let today = Local::now().format("%Y-%m-%d").to_string();
        {
            let connection = self.connection.lock().expect("数据库互斥锁");
            let last: Option<String> = connection
                .query_row(
                    "SELECT value FROM settings WHERE key = 'last_auto_backup_date'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            if last.as_deref() == Some(today.as_str()) {
                return Ok(());
            }
        }
        self.create_backup("auto")?;
        let connection = self.connection.lock().expect("数据库互斥锁");
        connection.execute(
            "INSERT INTO settings(key, value) VALUES('last_auto_backup_date', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [today],
        )?;
        Ok(())
    }

    pub fn list_backups(&self) -> Result<Vec<BackupInfo>> {
        let mut backups = Vec::new();
        for entry in fs::read_dir(&self.backups_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("sqlite3") {
                continue;
            }
            backups.push(self.backup_info(&path)?);
        }
        backups.sort_by_key(|backup| std::cmp::Reverse(backup.created_at));
        Ok(backups)
    }

    pub fn restore_backup(&self, name: &str) -> Result<()> {
        if Path::new(name).components().count() != 1 || !name.ends_with(".sqlite3") {
            bail!("备份名称无效");
        }
        let source_path = self.backups_dir.join(name);
        if !source_path.is_file() {
            bail!("备份不存在");
        }

        let source =
            Connection::open_with_flags(&source_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let backup_version: i64 = source.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if backup_version > SCHEMA_VERSION {
            bail!(
                "备份来自更高版本 v{backup_version}，当前应用仅支持 v{SCHEMA_VERSION}，已拒绝恢复以免损坏活动库"
            );
        }

        self.create_backup("before-restore")?;
        let mut destination = self.connection.lock().expect("数据库互斥锁");
        let backup = Backup::new(&source, &mut destination)?;
        backup.run_to_completion(64, Duration::from_millis(20), None)?;
        drop(backup);
        drop(destination);
        self.initialize()?;
        Ok(())
    }

    fn trim_backups(&self) -> Result<()> {
        let mut paths = fs::read_dir(&self.backups_dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("sqlite3"))
            .collect::<Vec<_>>();
        paths.sort_by_key(|path| {
            fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok()
        });
        while paths.len() > BACKUP_LIMIT {
            let oldest = paths.remove(0);
            fs::remove_file(&oldest)
                .with_context(|| format!("无法删除旧备份 {}", oldest.display()))?;
        }
        Ok(())
    }

    fn backup_info(&self, path: &Path) -> Result<BackupInfo> {
        let metadata = fs::metadata(path)?;
        let created_at = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or_default();
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("备份文件名无效"))?
            .to_string();
        Ok(BackupInfo {
            name,
            created_at,
            size: metadata.len(),
        })
    }
}

fn validate_annotation(input: &CreateAnnotation) -> Result<()> {
    if input.chapter_id.trim().is_empty() || input.block_id.trim().is_empty() {
        bail!("章节和正文块 ID 不能为空");
    }
    if input.end_offset <= input.start_offset {
        bail!("标注范围无效");
    }
    if input.quote.trim().is_empty() {
        bail!("选中文字不能为空");
    }
    match input.kind {
        AnnotationKind::Highlight => {
            let color = input
                .color
                .as_deref()
                .ok_or_else(|| anyhow!("高亮必须选择颜色"))?;
            if !HIGHLIGHT_COLORS.contains(&color) {
                bail!("不支持的高亮颜色");
            }
            if input.note.is_some() {
                bail!("高亮不能携带笔记正文");
            }
        }
        AnnotationKind::Note => {
            if input
                .note
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
            {
                bail!("笔记内容不能为空");
            }
            if input.color.is_some() {
                bail!("笔记不能携带高亮颜色");
            }
        }
        AnnotationKind::Bookmark => {
            if input.color.is_some() || input.note.is_some() {
                bail!("书签不能携带高亮颜色或笔记正文");
            }
        }
    }
    Ok(())
}

fn map_progress(row: &rusqlite::Row<'_>) -> rusqlite::Result<Progress> {
    Ok(Progress {
        book_id: row.get(0)?,
        chapter_id: row.get(1)?,
        block_id: row.get(2)?,
        chapter_progress: row.get(3)?,
        overall_progress: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

fn map_annotation(row: &rusqlite::Row<'_>) -> rusqlite::Result<Annotation> {
    let kind: String = row.get(7)?;
    let kind = AnnotationKind::parse(&kind).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            7,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "未知标注类型",
            )),
        )
    })?;
    Ok(Annotation {
        id: row.get(0)?,
        book_id: row.get(1)?,
        chapter_id: row.get(2)?,
        block_id: row.get(3)?,
        start_offset: row.get(4)?,
        end_offset: row.get(5)?,
        quote: row.get(6)?,
        kind,
        color: row.get(8)?,
        note: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

fn map_agent_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentTask> {
    Ok(AgentTask {
        id: row.get(0)?,
        book_id: row.get(1)?,
        kind: row.get(2)?,
        status: row.get(3)?,
        goal: row.get(4)?,
        current_runtime_id: row.get(5)?,
        error: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        phase: None,
        partial_output: None,
        stream_sequence: None,
        execution_id: None,
        turn_id: None,
    })
}

fn map_ai_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<AiMessage> {
    Ok(AiMessage {
        id: row.get(0)?,
        book_id: row.get(1)?,
        task_id: row.get(2)?,
        role: row.get(3)?,
        content: row.get(4)?,
        runtime_id: row.get(5)?,
        created_at: row.get(6)?,
        duration_ms: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use rusqlite::Connection;

    use super::Database;
    use crate::models::{AnnotationKind, CreateAnnotation, SaveProgress};

    fn highlight(start: u32, end: u32) -> CreateAnnotation {
        CreateAnnotation {
            chapter_id: "ch1".to_string(),
            block_id: "ch1-p1".to_string(),
            start_offset: start,
            end_offset: end,
            quote: "测试文字".to_string(),
            kind: AnnotationKind::Highlight,
            color: Some("yellow".to_string()),
            note: None,
        }
    }

    #[test]
    fn persists_progress_and_annotations() {
        let temp = TempDir::new().expect("临时目录");
        {
            let database = Database::open(temp.path()).expect("打开数据库");
            database
                .save_progress(
                    "book",
                    &SaveProgress {
                        chapter_id: "ch1".to_string(),
                        block_id: Some("ch1-p1".to_string()),
                        chapter_progress: 0.5,
                        overall_progress: 0.25,
                    },
                )
                .expect("保存进度");
            database
                .create_annotation("book", &highlight(0, 4))
                .expect("保存高亮");
        }

        let reopened = Database::open(temp.path()).expect("重新打开数据库");
        assert_eq!(
            reopened
                .progress("book")
                .expect("读取进度")
                .unwrap()
                .overall_progress,
            0.25
        );
        assert_eq!(reopened.annotations("book").expect("读取标注").len(), 1);
    }

    #[test]
    fn rejects_overlapping_highlights_and_deduplicates_exact_range() {
        let temp = TempDir::new().expect("临时目录");
        let database = Database::open(temp.path()).expect("打开数据库");
        let first = database
            .create_annotation("book", &highlight(0, 4))
            .expect("创建高亮");
        let duplicate = database
            .create_annotation("book", &highlight(0, 4))
            .expect("重复高亮返回原记录");
        assert_eq!(first.id, duplicate.id);

        let error = database
            .create_annotation("book", &highlight(2, 6))
            .expect_err("重叠高亮必须失败");
        assert!(error.to_string().contains("重叠"));
    }

    #[test]
    fn forget_book_removes_state_only_when_explicit() {
        let temp = TempDir::new().expect("临时目录");
        let database = Database::open(temp.path()).expect("打开数据库");
        database
            .create_annotation("book", &highlight(0, 4))
            .expect("保存高亮");
        assert_eq!(database.annotation_count("book").expect("计数"), 1);
        database.forget_book("book").expect("永久忘记");
        assert_eq!(database.annotation_count("book").expect("计数"), 0);
    }

    #[test]
    fn persists_shared_ai_history_and_task_state() {
        let temp = TempDir::new().expect("临时目录");
        let database = Database::open(temp.path()).expect("打开数据库");
        let task = database
            .create_question_task("book", "codex", "所有权是什么？")
            .expect("创建问题任务");
        let execution = database
            .start_agent_execution(&task.id, "codex")
            .expect("启动执行");
        database
            .complete_agent_execution(&task.id, &execution, "codex", "所有权是一组内存管理规则。")
            .expect("完成执行");

        let messages = database.ai_messages("book").expect("读取 AI 历史");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].runtime_id.as_deref(), Some("codex"));
        assert!(messages[0].duration_ms.is_none());
        assert!(messages[1].duration_ms.is_some());
        assert_eq!(
            database.agent_task(&task.id).unwrap().unwrap().status,
            "completed"
        );
    }

    #[test]
    fn retries_paused_task_with_another_runtime_without_duplicating_history() {
        let temp = TempDir::new().expect("临时目录");
        let database = Database::open(temp.path()).expect("打开数据库");
        let task = database
            .create_question_task("book", "codex", "所有权是什么？")
            .expect("创建问题任务");
        database
            .pause_agent_execution(&task.id, None, "Codex 暂时不可用")
            .expect("暂停任务");

        let retried = database
            .retry_agent_task(&task.id, "claude")
            .expect("切换 Agent 重试");
        assert_eq!(retried.id, task.id);
        assert_eq!(retried.status, "queued");
        assert_eq!(retried.current_runtime_id, "claude");
        assert_eq!(database.ai_messages("book").expect("读取历史").len(), 1);
    }

    #[test]
    fn stopped_task_cannot_be_changed_back_to_paused_or_completed() {
        let temp = TempDir::new().expect("临时目录");
        let database = Database::open(temp.path()).expect("打开数据库");
        let task = database
            .create_question_task("book", "codex", "所有权是什么？")
            .expect("创建问题任务");
        let execution = database
            .start_agent_execution(&task.id, "codex")
            .expect("启动执行");

        let stopped = database.stop_agent_task(&task.id).expect("停止任务");
        assert_eq!(stopped.status, "stopped");
        database
            .pause_agent_execution(&task.id, Some(&execution), "进程被终止")
            .expect("迟到的失败不得覆盖停止状态");
        database
            .complete_agent_execution(&task.id, &execution, "codex", "迟到的回答")
            .expect("迟到的回答应被忽略");

        assert_eq!(
            database.agent_task(&task.id).unwrap().unwrap().status,
            "stopped"
        );
        assert_eq!(database.ai_messages("book").expect("读取历史").len(), 1);
    }

    #[test]
    fn clearing_ai_workspace_preserves_reading_annotations() {
        let temp = TempDir::new().expect("临时目录");
        let database = Database::open(temp.path()).expect("打开数据库");
        database
            .create_annotation("book", &highlight(0, 4))
            .expect("保存高亮");
        database
            .create_question_task("book", "codex", "总结")
            .expect("创建问题任务");

        assert_eq!(database.clear_ai_workspace("book").expect("清除 AI"), 1);
        assert_eq!(database.annotation_count("book").expect("计数"), 1);
        assert!(database.ai_messages("book").expect("读取历史").is_empty());
    }

    #[test]
    fn persists_provider_sessions_and_clears_them_with_the_ai_workspace() {
        let temp = TempDir::new().expect("临时目录");
        let database = Database::open(temp.path()).expect("打开数据库");
        database
            .save_agent_session("book", "codex", "thread-1", r#"{"model":"gpt"}"#)
            .expect("保存 Agent 会话");

        let session = database
            .agent_session("book", "codex")
            .expect("读取 Agent 会话")
            .expect("会话存在");
        assert_eq!(session.provider_session_id, "thread-1");
        assert_eq!(session.runtime_id, "codex");

        database.clear_ai_workspace("book").expect("清除 AI 工作区");
        assert!(database
            .agent_session("book", "codex")
            .expect("再次读取 Agent 会话")
            .is_none());
    }

    #[test]
    fn restores_a_consistent_backup() {
        let temp = TempDir::new().expect("临时目录");
        let database = Database::open(temp.path()).expect("打开数据库");
        database
            .create_annotation("book", &highlight(0, 4))
            .expect("保存高亮");
        let backup = database.create_backup("manual").expect("创建备份");
        database.forget_book("book").expect("清空状态");
        database.restore_backup(&backup.name).expect("恢复备份");
        assert_eq!(database.annotation_count("book").expect("计数"), 1);
    }

    #[test]
    fn restore_backup_rejects_newer_schema() {
        let temp = TempDir::new().expect("临时目录");
        let database = Database::open(temp.path()).expect("打开数据库");
        let backup = database.create_backup("future").expect("创建备份");
        let backup_path = temp.path().join("Backups").join(&backup.name);
        {
            let conn = Connection::open(&backup_path).expect("打开备份");
            conn.pragma_update(None, "user_version", 999_i64)
                .expect("抬高备份版本");
        }
        let error = database
            .restore_backup(&backup.name)
            .expect_err("更高版本备份必须被拒绝");
        assert!(
            error.to_string().contains("更高版本"),
            "未报告更高版本：{error}"
        );
    }

    #[test]
    fn rejects_second_active_question_task_for_the_same_book() {
        let temp = TempDir::new().expect("临时目录");
        let database = Database::open(temp.path()).expect("打开数据库");
        database
            .create_question_task("book", "codex", "第一个问题")
            .expect("创建第一个任务");
        let error = database
            .create_question_task("book", "codex", "第二个问题")
            .expect_err("同一本书的第二个活跃任务必须被拒绝");
        assert!(
            error.to_string().contains("已有正在运行"),
            "未报告已有运行任务：{error}"
        );
    }
}

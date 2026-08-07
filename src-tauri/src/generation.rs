use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use url::Url;
use uuid::Uuid;

use crate::agent::{
    classify_agent_failure, transient_agent_retry_delay, AgentCoordinator, AgentFailureClass,
    TranslationRun, MAX_TRANSIENT_AGENT_ATTEMPTS,
};
use crate::importer::import_html_directory;
use crate::library::{resolve_package_file, validate_package};
use crate::models::{
    ImportChapterCandidate, ImportPreflight, ImportQualityReport, ImportSourceKind,
    ImportTaskEvent, ImportTaskEventMetrics, ImportTaskEventProgress, ImportTaskEventRuntime,
    ImportTaskEventTiming, ImportTaskSummary, ImportedBookSummary, PdfImportMode,
    StartImportRequest,
};
use crate::pdf_composer::{PdfCropBox, PdfPageComposer, PdfPageSource, PdfSourceLine};

const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ONLINE_CHAPTERS: usize = 200;
const MAX_PDF_PAGES: usize = 5_000;
const MAX_TRANSLATION_BATCH_BLOCKS: usize = 80;
const MAX_TRANSLATION_BATCH_CHARS: usize = 12_000;
const TRANSLATION_BATCH_CONCURRENCY: usize = 2;
const BOOK_TITLE_TRANSLATION_ID: &str = "goodreader-metadata-book-title";

fn is_unfinished_import_status(status: &str) -> bool {
    !matches!(status, "completed" | "cancelled")
}

#[derive(Clone)]
pub struct ImportManager {
    root: PathBuf,
    books_dir: PathBuf,
    agent: Arc<AgentCoordinator>,
    generation_slot: Arc<Semaphore>,
    cancelled: Arc<Mutex<HashSet<String>>>,
    paused: Arc<Mutex<HashSet<String>>>,
    start_lock: Arc<Mutex<()>>,
    event_lock: Arc<Mutex<()>>,
    scheduler_running: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredPreflight {
    preflight: ImportPreflight,
    source_path: Option<String>,
    source_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredTask {
    summary: ImportTaskSummary,
    request: StartImportRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PreparedSource {
    directory: PathBuf,
    image_count: usize,
    warnings: Vec<String>,
}

#[derive(Default)]
struct EventContext {
    scope: Option<String>,
    state: Option<String>,
    progress: Option<ImportTaskEventProgress>,
    timing: Option<ImportTaskEventTiming>,
    runtime: Option<ImportTaskEventRuntime>,
    metrics: Option<ImportTaskEventMetrics>,
}

struct TranslationBatchExecution {
    translations: BTreeMap<String, String>,
    answer: String,
    session_id: Option<String>,
    model: Option<String>,
    elapsed_ms: u64,
    attempts: usize,
}

impl ImportManager {
    pub fn new(root: PathBuf, books_dir: PathBuf, agent: Arc<AgentCoordinator>) -> Result<Self> {
        fs::create_dir_all(&root)
            .with_context(|| format!("无法创建书籍生成任务目录 {}", root.display()))?;
        AgentCoordinator::cleanup_recorded_processes_under(&root);
        fs::create_dir_all(&books_dir)
            .with_context(|| format!("无法创建书库目录 {}", books_dir.display()))?;
        let manager = Self {
            root,
            books_dir,
            agent,
            generation_slot: Arc::new(Semaphore::new(1)),
            cancelled: Arc::new(Mutex::new(HashSet::new())),
            paused: Arc::new(Mutex::new(HashSet::new())),
            start_lock: Arc::new(Mutex::new(())),
            event_lock: Arc::new(Mutex::new(())),
            scheduler_running: Arc::new(AtomicBool::new(false)),
        };
        manager.recover_interrupted_tasks()?;
        Ok(manager)
    }

    #[cfg(test)]
    pub fn preflight_local(
        &self,
        kind: ImportSourceKind,
        source: &Path,
    ) -> Result<ImportPreflight> {
        self.preflight_local_with_pdf_mode(kind, source, PdfImportMode::Auto)
    }

    pub fn preflight_local_with_pdf_mode(
        &self,
        kind: ImportSourceKind,
        source: &Path,
        pdf_mode: PdfImportMode,
    ) -> Result<ImportPreflight> {
        match kind {
            ImportSourceKind::Html => self.preflight_html(source),
            ImportSourceKind::Pdf => self.preflight_pdf(source, pdf_mode),
            ImportSourceKind::Url => bail!("在线来源必须提供 URL"),
        }
    }

    pub fn preflight_url(&self, value: &str) -> Result<ImportPreflight> {
        let url = parse_public_url(value)?;
        let token = Uuid::new_v4().to_string();
        let workspace = self.root.join(&token);
        let snapshot = workspace.join("snapshot");
        fs::create_dir_all(&snapshot)?;
        let html = fetch_url(&url, false)?;
        fs::write(snapshot.join("entry.html"), &html)?;

        let text = visible_text(&html);
        let (language, confidence) = detect_language(&text);
        let title = document_title(&html)
            .unwrap_or_else(|| url.host_str().unwrap_or("在线书籍").to_string());
        let author = document_author(&html).unwrap_or_else(|| "未知作者".to_string());
        let candidates = discover_chapter_links(&url, &html);
        let dynamic = has_scripts(&html) && text.chars().count() < 240;
        let warnings = dynamic
            .then(|| "静态页面正文较少，生成时将使用隔离浏览器渲染稳定 DOM".to_string())
            .into_iter()
            .collect::<Vec<_>>();
        let preflight = ImportPreflight {
            token: token.clone(),
            kind: ImportSourceKind::Url,
            source_name: url.as_str().to_string(),
            title: title.clone(),
            original_title: title,
            author,
            language,
            language_confidence: confidence,
            page_count: None,
            chapter_candidates: candidates,
            image_count: image_reference_count(&html),
            character_count: text.chars().count(),
            requires_ocr_pages: Vec::new(),
            uncertain_pages: Vec::new(),
            pdf_mode: None,
            pdf_type: None,
            dynamic_rendering: dynamic,
            warnings,
        };
        write_json(
            &workspace.join("preflight.json"),
            &StoredPreflight {
                preflight: preflight.clone(),
                source_path: None,
                source_url: Some(url.as_str().to_string()),
            },
        )?;
        Ok(preflight)
    }

    pub fn start(self: &Arc<Self>, request: StartImportRequest) -> Result<ImportTaskSummary> {
        let _start_guard = self.start_lock.lock().expect("创建导入任务锁");
        if let Some(task) = self
            .list_tasks()?
            .into_iter()
            .find(|task| is_unfinished_import_status(&task.status))
        {
            bail!(
                "已有未完成的导入任务《{}》（{}%），请先继续或取消该任务",
                task.title,
                task.progress
            );
        }
        let preflight = self.load_preflight(&request.token)?;
        validate_start_request(&request, &preflight.preflight)?;
        if !preflight.preflight.requires_ocr_pages.is_empty() {
            bail!(
                "这份 PDF 有 {} 页需要本地 OCR。当前版本尚未配置 OCR 模型，任务已停在预检阶段。",
                preflight.preflight.requires_ocr_pages.len()
            );
        }
        let uses_agent = request.translate || preflight.preflight.kind == ImportSourceKind::Pdf;
        if uses_agent && request.runtime_id.as_deref().unwrap_or_default().is_empty() {
            if preflight.preflight.kind == ImportSourceKind::Pdf {
                bail!("PDF 制书必须选择一个可用 Agent");
            }
            bail!("翻译为简体中文必须选择一个可用 Agent");
        }

        let id = Uuid::new_v4().to_string();
        let now = Utc::now().timestamp_millis();
        let summary = ImportTaskSummary {
            id: id.clone(),
            status: "queued".to_string(),
            stage: "queued".to_string(),
            progress: 0,
            title: request.title.trim().to_string(),
            uses_agent,
            queue_order: now,
            detail: "等待生成槽位".to_string(),
            error: None,
            imported: None,
            quality: None,
            created_at: now,
            updated_at: now,
        };
        let task = StoredTask {
            summary: summary.clone(),
            request,
        };
        let task_dir = self.root.join(&id);
        fs::create_dir_all(&task_dir)?;
        write_json(&task_dir.join("task.json"), &task)?;
        self.append_event(&id, "stage", "任务已创建", "等待开始生成")?;
        self.kick_scheduler();
        Ok(summary)
    }

    pub fn list_tasks(&self) -> Result<Vec<ImportTaskSummary>> {
        let mut tasks = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path().join("task.json");
            if !path.is_file() {
                continue;
            }
            if let Ok(task) = read_json::<StoredTask>(&path) {
                tasks.push(task.summary);
            }
        }
        tasks.sort_by_key(|task| std::cmp::Reverse(task.created_at));
        Ok(tasks)
    }

    pub fn task(&self, id: &str) -> Result<ImportTaskSummary> {
        Ok(self.load_task(id)?.summary)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn events(&self, id: &str) -> Result<Vec<ImportTaskEvent>> {
        self.events_since(id, 0)
    }

    pub fn events_since(&self, id: &str, after_seq: u64) -> Result<Vec<ImportTaskEvent>> {
        let task = self.load_task(id)?;
        let mut events = self.load_events(id)?;
        if task.summary.status == "running" && task.summary.stage == "translating" {
            if let Some(event) = self.live_agent_output(id)? {
                events.push(event);
            }
        }
        Ok(events
            .into_iter()
            .filter(|event| event.seq == 0 || event.seq > after_seq)
            .collect())
    }

    pub fn pause(&self, id: &str) -> Result<ImportTaskSummary> {
        self.paused
            .lock()
            .expect("暂停任务锁")
            .insert(id.to_string());
        self.agent.cancel_generations_under(&self.root.join(id));
        let mut task = self.load_task(id)?;
        if matches!(task.summary.status.as_str(), "completed" | "cancelled") {
            bail!("已结束的任务不能暂停");
        }
        let progress = task.summary.progress;
        update_summary(
            &mut task.summary,
            "paused",
            "paused",
            progress,
            "已暂停，将从最近检查点继续",
            None,
        );
        self.save_task(&task)?;
        self.append_event(id, "stage", "任务已暂停", "将从最近检查点继续")?;
        Ok(task.summary)
    }

    pub fn resume(
        self: &Arc<Self>,
        id: &str,
        runtime_id: Option<&str>,
    ) -> Result<ImportTaskSummary> {
        let mut task = self.load_task(id)?;
        if task.summary.status != "paused" && task.summary.status != "failed" {
            bail!("只有暂停或失败的任务可以继续");
        }
        self.paused.lock().expect("暂停任务锁").remove(id);
        self.cancelled.lock().expect("取消任务锁").remove(id);
        if let Some(runtime_id) = runtime_id.map(str::trim).filter(|value| !value.is_empty()) {
            let preflight = self.load_preflight(&task.request.token)?;
            if !task.request.translate && preflight.preflight.kind != ImportSourceKind::Pdf {
                bail!("不需要 Agent 的任务不能切换运行时");
            }
            task.request.runtime_id = Some(runtime_id.to_string());
        }
        let progress = task.summary.progress;
        update_summary(
            &mut task.summary,
            "queued",
            "queued",
            progress,
            "等待从检查点继续",
            None,
        );
        self.save_task(&task)?;
        self.append_event(id, "stage", "任务继续", "等待从最近检查点恢复")?;
        self.kick_scheduler();
        Ok(task.summary)
    }

    pub fn move_queued(&self, id: &str, direction: i8) -> Result<Vec<ImportTaskSummary>> {
        if !matches!(direction, -1 | 1) {
            bail!("队列移动方向无效");
        }
        let mut tasks = Vec::<StoredTask>::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path().join("task.json");
            if let Ok(task) = read_json::<StoredTask>(&path) {
                if task.summary.status == "queued" {
                    tasks.push(task);
                }
            }
        }
        tasks.sort_by_key(|task| task.summary.queue_order);
        let index = tasks
            .iter()
            .position(|task| task.summary.id == id)
            .context("只有排队中的任务可以调整顺序")?;
        let target = if direction < 0 {
            index.checked_sub(1)
        } else if index + 1 < tasks.len() {
            Some(index + 1)
        } else {
            None
        };
        if let Some(target) = target {
            let order = tasks[index].summary.queue_order;
            tasks[index].summary.queue_order = tasks[target].summary.queue_order;
            tasks[target].summary.queue_order = order;
            self.save_task(&tasks[index])?;
            self.save_task(&tasks[target])?;
        }
        self.list_tasks()
    }

    pub fn cancel(&self, id: &str) -> Result<ImportTaskSummary> {
        self.cancelled
            .lock()
            .expect("取消任务锁")
            .insert(id.to_string());
        self.agent.cancel_generations_under(&self.root.join(id));
        let mut task = self.load_task(id)?;
        if task.summary.status == "completed" {
            bail!("已完成任务不能取消");
        }
        let progress = task.summary.progress;
        update_summary(
            &mut task.summary,
            "cancelled",
            "cancelled",
            progress,
            "任务已取消",
            None,
        );
        self.save_task(&task)?;
        self.append_event(id, "stage", "任务已取消", "生成输出已停止")?;
        self.cleanup_task_outputs(id)?;
        let snapshot_workspace = self.root.join(&task.request.token);
        if snapshot_workspace.exists() {
            fs::remove_dir_all(snapshot_workspace)?;
        }
        Ok(task.summary)
    }

    async fn run_task(self: Arc<Self>, id: String) {
        let permit = match self.generation_slot.acquire().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        if self.should_stop(&id) {
            drop(permit);
            return;
        }
        if let Err(error) = self.execute_task(&id).await {
            if !self.should_stop(&id) {
                if let Ok(mut task) = self.load_task(&id) {
                    let progress = task.summary.progress;
                    update_summary(
                        &mut task.summary,
                        "failed",
                        "failed",
                        progress,
                        "生成任务已暂停",
                        Some(friendly_error(&error)),
                    );
                    let _ = self.save_task(&task);
                    let _ = self.append_event(&id, "error", "生成失败", &friendly_error(&error));
                }
            }
        }
        drop(permit);
    }

    fn kick_scheduler(self: &Arc<Self>) {
        if self.scheduler_running.swap(true, Ordering::AcqRel) {
            return;
        }
        let manager = self.clone();
        tokio::spawn(async move {
            loop {
                let next = manager.next_queued_task().ok().flatten();
                let Some(id) = next else {
                    break;
                };
                manager.clone().run_task(id).await;
            }
            manager.scheduler_running.store(false, Ordering::Release);
            if manager.next_queued_task().ok().flatten().is_some() {
                manager.kick_scheduler();
            }
        });
    }

    fn next_queued_task(&self) -> Result<Option<String>> {
        let mut queued = self
            .list_tasks()?
            .into_iter()
            .filter(|task| task.status == "queued")
            .collect::<Vec<_>>();
        queued.sort_by_key(|task| task.queue_order);
        Ok(queued.first().map(|task| task.id.clone()))
    }

    async fn execute_task(&self, id: &str) -> Result<()> {
        let mut task = self.load_task(id)?;
        let preflight = self.load_preflight(&task.request.token)?;
        let resume_progress = task.summary.progress;
        update_summary(
            &mut task.summary,
            "running",
            "snapshot",
            resume_progress.max(8),
            "正在校验来源快照",
            None,
        );
        self.save_task(&task)?;
        self.append_event(
            id,
            "stage",
            "校验来源快照",
            "确认来源文件、章节选择和导入参数",
        )?;
        self.checkpoint(id)?;
        self.append_event(id, "script", "来源快照校验完成", "执行成功")?;

        let task_dir = self.root.join(id);
        let prepared_dir = task_dir.join("prepared-source");
        let candidate_books = task_dir.join("candidate-books");
        let existing_candidate = (resume_progress >= 42)
            .then(|| resumable_candidate_directory(&candidate_books).ok())
            .flatten()
            .filter(|candidate| validate_package(candidate).is_ok());
        let (prepared, candidate) = if let Some(candidate) = existing_candidate {
            let prepared = read_json::<PreparedSource>(&task_dir.join("prepared.json")).unwrap_or(
                PreparedSource {
                    directory: prepared_dir.clone(),
                    image_count: 0,
                    warnings: vec!["任务从已完成的书籍契约检查点恢复".to_string()],
                },
            );
            update_summary(
                &mut task.summary,
                "running",
                "contract",
                resume_progress.max(42),
                "已从书籍契约检查点恢复",
                None,
            );
            self.save_task(&task)?;
            self.append_event(id, "stage", "恢复书籍契约", "复用上次已通过校验的候选书籍")?;
            (prepared, candidate)
        } else {
            if prepared_dir.exists() && preflight.preflight.kind != ImportSourceKind::Url {
                fs::remove_dir_all(&prepared_dir)?;
            }
            if candidate_books.exists() {
                fs::remove_dir_all(&candidate_books)?;
            }
            fs::create_dir_all(&prepared_dir)?;
            fs::create_dir_all(&candidate_books)?;

            update_summary(
                &mut task.summary,
                "running",
                "converting",
                18,
                "正在转换为静态 HTML",
                None,
            );
            self.save_task(&task)?;
            let (script_title, script_detail) =
                conversion_script_description(&preflight.preflight.kind);
            self.append_event(id, "script", script_title, script_detail)?;
            let prepared = if preflight.preflight.kind == ImportSourceKind::Pdf {
                self.prepare_pdf_source_with_agent(id, &preflight, &task.request, &prepared_dir)
                    .await?
            } else {
                let request = task.request.clone();
                let preflight_for_conversion = preflight.clone();
                let prepared_dir_for_conversion = prepared_dir.clone();
                let progress_manager = self.clone();
                let progress_task_id = id.to_string();
                tokio::task::spawn_blocking(move || {
                    prepare_source(
                        &preflight_for_conversion,
                        &request,
                        &prepared_dir_for_conversion,
                        move |completed, total, reused| {
                            progress_manager.record_online_chapter_progress(
                                &progress_task_id,
                                completed,
                                total,
                                reused,
                            )
                        },
                    )
                })
                .await??
            };
            write_json(&task_dir.join("prepared.json"), &prepared)?;
            self.append_event(
                id,
                "script",
                "来源转换完成",
                &format!("执行成功，处理 {} 项图片资源", prepared.image_count),
            )?;
            self.checkpoint(id)?;

            update_summary(
                &mut task.summary,
                "running",
                "contract",
                42,
                "正在建立章节和正文块账本",
                None,
            );
            self.save_task(&task)?;
            self.append_event(
                id,
                "stage",
                "建立书籍契约",
                "生成章节清单、稳定正文块 ID 和统一阅读器元数据",
            )?;
            let prepared_source = prepared.directory.clone();
            let candidate_books_for_import = candidate_books.clone();
            tokio::task::spawn_blocking(move || {
                import_html_directory(&prepared_source, &candidate_books_for_import)
            })
            .await??;
            let candidate = only_child_directory(&candidate_books)?;
            rewrite_manifest_metadata(&candidate, &task.request, &preflight.preflight)?;
            self.append_event(id, "script", "书籍契约生成完成", "执行成功")?;
            self.checkpoint(id)?;
            (prepared, candidate)
        };

        let translation_checkpoint = candidate_books.join("translation.done");
        if task.request.translate && !translation_checkpoint.is_file() {
            update_summary(
                &mut task.summary,
                "running",
                "translating",
                56,
                "Agent 正在翻译为简体中文",
                None,
            );
            self.save_task(&task)?;
            self.append_event(
                id,
                "stage",
                "开始 Agent 翻译",
                "已生成正文块输入，正在等待 Agent 返回",
            )?;
            self.translate_candidate(id, &candidate, &task.request, &preflight.preflight)
                .await?;
            fs::write(&translation_checkpoint, b"completed")?;
            self.append_event(
                id,
                "stage",
                "Agent 翻译完成",
                "译文已通过正文块和占位符完整性检查",
            )?;
            self.checkpoint(id)?;
        }

        update_summary(
            &mut task.summary,
            "running",
            "validating",
            84,
            "正在执行完整性与安全校验",
            None,
        );
        self.save_task(&task)?;
        self.append_event(
            id,
            "stage",
            "质量与安全校验",
            "检查章节、正文块、图片引用、脚本与翻译对齐",
        )?;
        let package = validate_package(&candidate)?;
        let mut quality = quality_report(&candidate, &package.manifest)?;
        if prepared.image_count > quality.image_count {
            quality.errors.push(format!(
                "转换阶段生成了 {} 张语义图片，但书籍正文只引用了 {} 张",
                prepared.image_count, quality.image_count
            ));
        }
        quality
            .warnings
            .extend(preflight.preflight.warnings.clone());
        quality.warnings.extend(prepared.warnings);
        if !quality.errors.is_empty() {
            bail!("质量报告存在必须修复的错误：{}", quality.errors.join("；"));
        }
        self.append_event(
            id,
            "script",
            "质量与安全校验完成",
            &format!(
                "执行成功：{} 章、{} 个正文块、{} 张图片",
                quality.chapter_count, quality.block_count, quality.image_count
            ),
        )?;
        self.checkpoint(id)?;

        update_summary(
            &mut task.summary,
            "running",
            "publishing",
            94,
            "正在原子写入书架",
            None,
        );
        task.summary.quality = Some(quality.clone());
        self.save_task(&task)?;
        self.append_event(id, "stage", "写入书架", "正在原子发布候选书籍")?;
        let destination = self.books_dir.join(
            candidate
                .file_name()
                .ok_or_else(|| anyhow!("候选书籍目录名称无效"))?,
        );
        if destination.exists() {
            bail!("书库中已经存在同名候选目录");
        }
        self.checkpoint(id)?;
        fs::rename(&candidate, &destination).context("无法将候选书籍写入书架")?;

        let imported_summary = ImportedBookSummary {
            id: package.manifest.id,
            title: package.manifest.title,
            chapter_count: package.manifest.chapters.len(),
            warnings: quality.warnings.clone(),
        };
        update_summary(
            &mut task.summary,
            "completed",
            "completed",
            100,
            "书籍已经进入书架",
            None,
        );
        task.summary.imported = Some(imported_summary);
        task.summary.quality = Some(quality);
        self.save_task(&task)?;
        self.append_event(id, "stage", "生成完成", "书籍已经进入书架")?;
        let _ = fs::remove_dir_all(&prepared_dir);
        let _ = fs::remove_dir_all(&candidate_books);
        let _ = fs::remove_dir_all(self.root.join(id).join("pdf-layout"));
        let _ = fs::remove_dir_all(self.root.join(&task.request.token));
        Ok(())
    }

    async fn prepare_pdf_source_with_agent(
        &self,
        task_id: &str,
        stored: &StoredPreflight,
        request: &StartImportRequest,
        destination: &Path,
    ) -> Result<PreparedSource> {
        let runtime_id = request
            .runtime_id
            .as_deref()
            .context("PDF 制书必须选择一个可用 Agent")?;
        let pdf = PathBuf::from(
            stored
                .source_path
                .as_deref()
                .context("PDF 来源快照不存在")?,
        );
        let text_path = pdf.parent().context("PDF 快照目录无效")?.join("source.txt");
        let text = fs::read_to_string(&text_path).context("PDF 文本快照不存在")?;
        let page_count = stored.preflight.page_count.context("PDF 页数缺失")?;
        let pages = split_pdf_pages(&text, page_count);
        let selected = request
            .chapters
            .iter()
            .filter(|chapter| chapter.selected)
            .collect::<Vec<_>>();
        if selected.is_empty() {
            bail!("至少保留一个章节");
        }
        let mut selected_pages = BTreeSet::new();
        for chapter in &selected {
            let (start, end) = parse_page_range(&chapter.source, page_count)?;
            selected_pages.extend(start..=end);
        }
        fs::create_dir_all(destination.join("chapters"))?;
        fs::create_dir_all(destination.join("assets/figures"))?;
        let page_workspace_root = self.root.join(task_id).join("pdf-layout/pages");
        fs::create_dir_all(&page_workspace_root)?;
        let composer = PdfPageComposer::new(self.agent.clone());
        let repeated_lines = repeated_pdf_lines(&pages);
        let image_pages = pdf_image_pages(&pdf).unwrap_or_default();
        let total_pages = selected_pages.len();
        let mut page_html = BTreeMap::new();
        let mut image_count = 0usize;

        for (completed, page) in selected_pages.iter().copied().enumerate() {
            self.checkpoint(task_id)?;
            let workspace = page_workspace_root.join(format!("page-{page:04}"));
            let input_image = workspace.join("rendered-page.png");
            if !input_image.is_file() {
                fs::create_dir_all(&workspace)?;
                render_pdf_page(&pdf, page, &input_image, 144)?;
            }
            let (image_width, image_height) = png_dimensions(&input_image)?;
            let lines = pdf_source_lines(&pages[page - 1], &repeated_lines);
            self.append_event_with_context(
                task_id,
                "agent",
                &format!("Agent 正在排版 PDF 第 {page} 页"),
                "正在恢复阅读顺序、语义块和完整图片区域",
                EventContext {
                    scope: Some(format!("pdf.page.{page}")),
                    state: Some("running".to_string()),
                    progress: Some(ImportTaskEventProgress {
                        completed,
                        total: total_pages,
                        unit: "pages".to_string(),
                    }),
                    timing: Some(ImportTaskEventTiming {
                        started_at: Utc::now().timestamp_millis(),
                        elapsed_ms: 0,
                        eta_ms: None,
                    }),
                    runtime: Some(ImportTaskEventRuntime {
                        id: runtime_id.to_string(),
                        model: None,
                        session_id: None,
                        pid: None,
                    }),
                    metrics: None,
                },
            )?;
            let composed = composer
                .compose_with_retry(
                    runtime_id,
                    &workspace,
                    &PdfPageSource {
                        page,
                        image_path: input_image,
                        image_width,
                        image_height,
                        requires_figure: image_pages.contains(&page),
                        lines,
                    },
                    |retry| {
                        let delay_seconds = retry.delay_ms.saturating_add(999) / 1_000;
                        self.append_event_with_context(
                            task_id,
                            "agent",
                            &format!(
                                "PDF 第 {page} 页自动重试 {}/{}",
                                retry.next_attempt, retry.max_attempts
                            ),
                            &format!(
                                "第 {} 次执行失败：{}\n将在 {delay_seconds} 秒后自动重试",
                                retry.failed_attempt,
                                truncate_event_detail(&retry.reason, 2_000)
                            ),
                            EventContext {
                                scope: Some(format!("pdf.page.{page}")),
                                state: Some("retrying".to_string()),
                                progress: Some(ImportTaskEventProgress {
                                    completed,
                                    total: total_pages,
                                    unit: "pages".to_string(),
                                }),
                                timing: Some(ImportTaskEventTiming {
                                    started_at: Utc::now().timestamp_millis(),
                                    elapsed_ms: 0,
                                    eta_ms: Some(retry.delay_ms),
                                }),
                                runtime: Some(ImportTaskEventRuntime {
                                    id: runtime_id.to_string(),
                                    model: None,
                                    session_id: None,
                                    pid: None,
                                }),
                                metrics: None,
                            },
                        )
                    },
                    || self.checkpoint(task_id),
                )
                .await?;
            let mut html = composed.html;
            for (figure_index, figure) in composed.figures.iter().enumerate() {
                let file_name = format!("page-{page:04}-figure-{:02}.png", figure_index + 1);
                let figure_path = destination.join("assets/figures").join(&file_name);
                render_pdf_region(&pdf, page, &figure.crop, &figure_path, 144)?;
                let caption = if figure.caption.trim().is_empty() {
                    String::new()
                } else {
                    format!("<figcaption>{}</figcaption>", escape_html(&figure.caption))
                };
                html = html.replace(
                    &figure.marker,
                    &format!(
                        "<figure><img src=\"../assets/figures/{file_name}\" alt=\"{}\">{caption}</figure>",
                        escape_html(&figure.alt)
                    ),
                );
                image_count += 1;
            }
            page_html.insert(
                page,
                format!("<section class=\"pdf-page\" data-source-page=\"{page}\">{html}</section>"),
            );
            self.record_pdf_page_progress(
                task_id,
                completed + 1,
                total_pages,
                page,
                composed.reused,
                composed.attempts,
            )?;
        }

        for (chapter_index, chapter) in selected.iter().enumerate() {
            let (start, end) = parse_page_range(&chapter.source, page_count)?;
            let body = (start..=end)
                .filter_map(|page| page_html.get(&page))
                .cloned()
                .collect::<String>();
            fs::write(
                destination.join(format!("chapters/chapter-{:04}.html", chapter_index + 1)),
                html_document(&chapter.title, &request.author, &body),
            )?;
        }
        render_pdf_cover(&pdf, destination)?;
        fs::write(
            destination.join("index.html"),
            build_index_html(&request.title, &request.author, &selected),
        )?;
        Ok(PreparedSource {
            directory: destination.to_path_buf(),
            image_count,
            warnings: Vec::new(),
        })
    }

    async fn translate_candidate(
        &self,
        task_id: &str,
        candidate: &Path,
        request: &StartImportRequest,
        preflight: &ImportPreflight,
    ) -> Result<()> {
        let runtime_id = request.runtime_id.as_deref().context("缺少 Agent 运行时")?;
        let workspace = self.root.join(task_id).join("translation");
        let blocks = extract_translation_blocks(candidate)?;
        if blocks.is_empty() {
            bail!("没有找到可翻译正文块");
        }
        let stored_blocks =
            read_json::<BTreeMap<String, String>>(&workspace.join("input/blocks.json")).ok();
        if workspace.exists() && stored_blocks.as_ref() != Some(&blocks) {
            fs::remove_dir_all(&workspace)?;
        }
        fs::create_dir_all(workspace.join("input"))?;
        fs::create_dir_all(workspace.join("output"))?;
        write_json(&workspace.join("input/blocks.json"), &blocks)?;
        let total_blocks = blocks.len();
        let total_chars = translation_char_count(&blocks);
        let mut translations = collect_reusable_translations(&workspace, &blocks)?;
        let mut completed_blocks = translations.len();
        let mut completed_chars = translation_char_count_for_ids(&blocks, translations.keys());
        let reused_chars = completed_chars;
        if !translations.is_empty() {
            write_json(&workspace.join("output/translations.json"), &translations)?;
            self.update_translation_progress(
                task_id,
                completed_blocks,
                total_blocks,
                completed_chars,
                total_chars,
                None,
            )?;
            self.append_event(
                task_id,
                "stage",
                "恢复已完成译文",
                &format!("已按正文块校验并复用 {completed_blocks}/{total_blocks} 个译文"),
            )?;
        }
        let missing_blocks = blocks
            .iter()
            .filter(|(id, _)| !translations.contains_key(*id))
            .map(|(id, text)| (id.clone(), text.clone()))
            .collect::<BTreeMap<_, _>>();
        let batches = translation_batches(&missing_blocks);
        let total_batches = batches.len();
        let pending = batches.into_iter().enumerate().collect::<Vec<_>>();
        let translation_started = std::time::Instant::now();

        for group in pending.chunks(TRANSLATION_BATCH_CONCURRENCY) {
            self.checkpoint(task_id)?;
            let first = &group[0];
            let first_number = first.0 + 1;
            let first_root = workspace.join(format!("batches/batch-{first_number:04}"));
            self.prepare_translation_batch(
                task_id,
                &first_root,
                &first.1,
                first_number,
                total_batches,
                completed_blocks,
                total_blocks,
                completed_chars,
                total_chars,
                runtime_id,
            )?;
            let mut first_future = Box::pin(self.execute_translation_batch(
                task_id,
                runtime_id,
                &first_root,
                &first.1,
                preflight,
                first_number,
                total_batches,
            ));

            let mut failure;
            if let Some(second) = group.get(1) {
                let second_number = second.0 + 1;
                let second_root = workspace.join(format!("batches/batch-{second_number:04}"));
                self.prepare_translation_batch(
                    task_id,
                    &second_root,
                    &second.1,
                    second_number,
                    total_batches,
                    completed_blocks,
                    total_blocks,
                    completed_chars,
                    total_chars,
                    runtime_id,
                )?;
                let mut second_future = Box::pin(self.execute_translation_batch(
                    task_id,
                    runtime_id,
                    &second_root,
                    &second.1,
                    preflight,
                    second_number,
                    total_batches,
                ));
                let first_completed = tokio::select! {
                    result = &mut first_future => (true, result),
                    result = &mut second_future => (false, result),
                };
                if first_completed.0 {
                    failure = self.record_translation_batch_result(
                        task_id,
                        &workspace,
                        runtime_id,
                        first_number,
                        total_batches,
                        &first.1,
                        first_completed.1,
                        &mut translations,
                        &mut completed_blocks,
                        &mut completed_chars,
                        total_blocks,
                        total_chars,
                        reused_chars,
                        translation_started.elapsed(),
                    )?;
                    let second_result = second_future.await;
                    let second_failure = self.record_translation_batch_result(
                        task_id,
                        &workspace,
                        runtime_id,
                        second_number,
                        total_batches,
                        &second.1,
                        second_result,
                        &mut translations,
                        &mut completed_blocks,
                        &mut completed_chars,
                        total_blocks,
                        total_chars,
                        reused_chars,
                        translation_started.elapsed(),
                    )?;
                    failure = failure.or(second_failure);
                } else {
                    failure = self.record_translation_batch_result(
                        task_id,
                        &workspace,
                        runtime_id,
                        second_number,
                        total_batches,
                        &second.1,
                        first_completed.1,
                        &mut translations,
                        &mut completed_blocks,
                        &mut completed_chars,
                        total_blocks,
                        total_chars,
                        reused_chars,
                        translation_started.elapsed(),
                    )?;
                    let first_result = first_future.await;
                    let first_failure = self.record_translation_batch_result(
                        task_id,
                        &workspace,
                        runtime_id,
                        first_number,
                        total_batches,
                        &first.1,
                        first_result,
                        &mut translations,
                        &mut completed_blocks,
                        &mut completed_chars,
                        total_blocks,
                        total_chars,
                        reused_chars,
                        translation_started.elapsed(),
                    )?;
                    failure = failure.or(first_failure);
                }
            } else {
                failure = self.record_translation_batch_result(
                    task_id,
                    &workspace,
                    runtime_id,
                    first_number,
                    total_batches,
                    &first.1,
                    first_future.await,
                    &mut translations,
                    &mut completed_blocks,
                    &mut completed_chars,
                    total_blocks,
                    total_chars,
                    reused_chars,
                    translation_started.elapsed(),
                )?;
            }
            if let Some(error) = failure {
                return Err(error);
            }
        }
        validate_translation_map(&blocks, &translations)?;
        let source_language = match preflight.language.as_str() {
            "zh-CN" => "zh-CN",
            "mixed" => "mul",
            _ => "und",
        };
        apply_translations(
            candidate,
            &translations,
            request.preserve_original,
            source_language,
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn record_translation_batch_result(
        &self,
        task_id: &str,
        workspace: &Path,
        runtime_id: &str,
        batch_number: usize,
        total_batches: usize,
        batch: &BTreeMap<String, String>,
        result: Result<TranslationBatchExecution>,
        translations: &mut BTreeMap<String, String>,
        completed_blocks: &mut usize,
        completed_chars: &mut usize,
        total_blocks: usize,
        total_chars: usize,
        reused_chars: usize,
        elapsed: std::time::Duration,
    ) -> Result<Option<anyhow::Error>> {
        let execution = match result {
            Ok(execution) => execution,
            Err(error) => return Ok(Some(error)),
        };
        translations.extend(execution.translations);
        *completed_blocks += batch.len();
        *completed_chars += translation_char_count(batch);
        write_json(&workspace.join("output/translations.json"), translations)?;
        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let session_completed_chars = completed_chars.saturating_sub(reused_chars);
        let session_total_chars = total_chars.saturating_sub(reused_chars);
        let eta_ms = translation_eta_ms(session_completed_chars, session_total_chars, elapsed_ms);
        self.update_translation_progress(
            task_id,
            *completed_blocks,
            total_blocks,
            *completed_chars,
            total_chars,
            eta_ms,
        )?;
        let mut agent_context = translation_event_context(
            "completed",
            batch_number,
            total_batches,
            batch,
            *completed_blocks,
            total_blocks,
            *completed_chars,
            total_chars,
            runtime_id,
            execution.session_id.clone(),
            execution.elapsed_ms,
            eta_ms,
            execution.attempts,
        );
        if let Some(runtime) = agent_context.runtime.as_mut() {
            runtime.model = execution.model.clone();
        }
        self.append_event_with_context(
            task_id,
            "agent",
            &format!("Agent 返回 · 批次 {batch_number}/{total_batches}"),
            if execution.answer.trim().is_empty() {
                "结构化译文已返回"
            } else {
                &execution.answer
            },
            agent_context,
        )?;
        let mut stage_context = translation_event_context(
            "completed",
            batch_number,
            total_batches,
            batch,
            *completed_blocks,
            total_blocks,
            *completed_chars,
            total_chars,
            runtime_id,
            None,
            execution.elapsed_ms,
            eta_ms,
            execution.attempts,
        );
        if let Some(runtime) = stage_context.runtime.as_mut() {
            runtime.model = execution.model;
        }
        self.append_event_with_context(
            task_id,
            "stage",
            &format!("翻译批次 {batch_number}/{total_batches} 完成"),
            &format!("已校验并保存 {} 个正文块", batch.len()),
            stage_context,
        )?;
        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_translation_batch(
        &self,
        task_id: &str,
        batch_root: &Path,
        batch: &BTreeMap<String, String>,
        batch_number: usize,
        total_batches: usize,
        completed_blocks: usize,
        total_blocks: usize,
        completed_chars: usize,
        total_chars: usize,
        runtime_id: &str,
    ) -> Result<()> {
        if batch_root.exists() {
            fs::remove_dir_all(&batch_root)?;
        }
        fs::create_dir_all(batch_root.join("input"))?;
        fs::create_dir_all(batch_root.join("output"))?;
        write_json(&batch_root.join("input/blocks.json"), batch)?;
        self.append_event_with_context(
            task_id,
            "stage",
            &format!("翻译批次 {batch_number}/{total_batches}"),
            &format!(
                "正在翻译 {} 个正文块、{} 个字符",
                batch.len(),
                translation_char_count(batch)
            ),
            translation_event_context(
                "running",
                batch_number,
                total_batches,
                batch,
                completed_blocks,
                total_blocks,
                completed_chars,
                total_chars,
                runtime_id,
                None,
                0,
                None,
                1,
            ),
        )?;
        Ok(())
    }

    async fn execute_translation_batch(
        &self,
        task_id: &str,
        runtime_id: &str,
        batch_root: &Path,
        batch: &BTreeMap<String, String>,
        preflight: &ImportPreflight,
        batch_number: usize,
        total_batches: usize,
    ) -> Result<TranslationBatchExecution> {
        let run = self
            .run_translation_with_retry(
                task_id,
                runtime_id,
                batch_root,
                batch,
                &preflight.language,
                &format!("{batch_number}/{total_batches}"),
            )
            .await;
        match run {
            Ok((run, attempts)) => match validate_translation_map(batch, &run.translations) {
                Ok(()) => Ok(TranslationBatchExecution {
                    translations: run.translations,
                    answer: run.answer,
                    session_id: run.session_id,
                    model: run.model,
                    elapsed_ms: run.elapsed_ms,
                    attempts,
                }),
                Err(error) => {
                    self.retry_split_translation_batch(
                        task_id,
                        runtime_id,
                        batch_root,
                        batch,
                        preflight,
                        batch_number,
                        total_batches,
                        error,
                        attempts,
                    )
                    .await
                }
            },
            Err(error) if should_split_translation_error(&error) && batch.len() > 1 => {
                self.retry_split_translation_batch(
                    task_id,
                    runtime_id,
                    batch_root,
                    batch,
                    preflight,
                    batch_number,
                    total_batches,
                    error,
                    1,
                )
                .await
            }
            Err(error) => {
                Err(error).with_context(|| format!("翻译批次 {batch_number}/{total_batches} 失败"))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_translation_with_retry(
        &self,
        task_id: &str,
        runtime_id: &str,
        workspace: &Path,
        batch: &BTreeMap<String, String>,
        source_language: &str,
        scope_title: &str,
    ) -> Result<(TranslationRun, usize)> {
        let started = std::time::Instant::now();
        for attempt in 1..=MAX_TRANSIENT_AGENT_ATTEMPTS {
            self.checkpoint(task_id)?;
            match self
                .agent
                .run_translation(runtime_id, workspace, batch, source_language)
                .await
            {
                Ok(mut run) => {
                    run.elapsed_ms =
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    return Ok((run, attempt));
                }
                Err(error)
                    if !should_split_translation_error(&error)
                        && classify_agent_failure(&error) == AgentFailureClass::Transient
                        && attempt < MAX_TRANSIENT_AGENT_ATTEMPTS =>
                {
                    let delay = transient_agent_retry_delay(attempt);
                    let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
                    let delay_seconds = delay_ms.saturating_add(999) / 1_000;
                    self.append_event_with_context(
                        task_id,
                        "agent",
                        &format!(
                            "翻译批次 {scope_title} 自动重试 {}/{}",
                            attempt + 1,
                            MAX_TRANSIENT_AGENT_ATTEMPTS
                        ),
                        &format!(
                            "第 {attempt} 次执行遇到临时故障：{}\n将在 {delay_seconds} 秒后自动重试",
                            truncate_event_detail(&format!("{error:#}"), 2_000)
                        ),
                        EventContext {
                            scope: Some(format!("translation.batch.{scope_title}")),
                            state: Some("retrying".to_string()),
                            progress: None,
                            timing: Some(ImportTaskEventTiming {
                                started_at: Utc::now().timestamp_millis(),
                                elapsed_ms: 0,
                                eta_ms: Some(delay_ms),
                            }),
                            runtime: Some(ImportTaskEventRuntime {
                                id: runtime_id.to_string(),
                                model: None,
                                session_id: None,
                                pid: None,
                            }),
                            metrics: None,
                        },
                    )?;
                    self.wait_retry_delay(task_id, delay).await?;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("翻译重试循环必须返回结果")
    }

    #[allow(clippy::too_many_arguments)]
    async fn retry_split_translation_batch(
        &self,
        task_id: &str,
        runtime_id: &str,
        batch_root: &Path,
        batch: &BTreeMap<String, String>,
        preflight: &ImportPreflight,
        batch_number: usize,
        total_batches: usize,
        initial_error: anyhow::Error,
        initial_attempts: usize,
    ) -> Result<TranslationBatchExecution> {
        let (left, right) = split_translation_batch(batch);
        if right.is_empty() {
            return Err(initial_error)
                .with_context(|| format!("翻译批次 {batch_number}/{total_batches} 校验失败"));
        }
        self.append_event(
            task_id,
            "stage",
            &format!("翻译批次 {batch_number}/{total_batches} 自动拆分重试"),
            &format!("首次结果未通过校验：{initial_error:#}"),
        )?;
        let left_root = batch_root.join("retry-part-1");
        let right_root = batch_root.join("retry-part-2");
        prepare_retry_workspace(&left_root, &left)?;
        prepare_retry_workspace(&right_root, &right)?;
        let left_scope = format!("{batch_number}/{total_batches} · 前半");
        let right_scope = format!("{batch_number}/{total_batches} · 后半");
        let (left_result, right_result) = tokio::join!(
            self.run_translation_with_retry(
                task_id,
                runtime_id,
                &left_root,
                &left,
                &preflight.language,
                &left_scope,
            ),
            self.run_translation_with_retry(
                task_id,
                runtime_id,
                &right_root,
                &right,
                &preflight.language,
                &right_scope,
            )
        );
        let (left_result, left_attempts) = left_result.context("拆分后的前半批次翻译失败")?;
        let (right_result, right_attempts) = right_result.context("拆分后的后半批次翻译失败")?;
        validate_translation_map(&left, &left_result.translations)?;
        validate_translation_map(&right, &right_result.translations)?;
        let mut translations = left_result.translations;
        translations.extend(right_result.translations);
        validate_translation_map(batch, &translations)?;
        write_json(&batch_root.join("output/translations.json"), &translations)?;
        Ok(TranslationBatchExecution {
            translations,
            answer: "首次结果未通过校验，已自动拆分为两个子批次并完成".to_string(),
            session_id: left_result.session_id.or(right_result.session_id),
            model: left_result.model.or(right_result.model),
            elapsed_ms: left_result
                .elapsed_ms
                .saturating_add(right_result.elapsed_ms),
            attempts: initial_attempts
                .saturating_add(left_attempts)
                .saturating_add(right_attempts),
        })
    }

    fn update_translation_progress(
        &self,
        task_id: &str,
        completed_blocks: usize,
        total_blocks: usize,
        completed_chars: usize,
        total_chars: usize,
        eta_ms: Option<u64>,
    ) -> Result<()> {
        let mut task = self.load_task(task_id)?;
        let progress = 56
            + u8::try_from(completed_chars.saturating_mul(27) / total_chars.max(1)).unwrap_or(27);
        let eta = eta_ms
            .map(format_duration_ms)
            .map(|value| format!("，预计剩余 {value}"))
            .unwrap_or_default();
        update_summary(
            &mut task.summary,
            "running",
            "translating",
            progress.min(83),
            &format!(
                "已完成 {completed_blocks}/{total_blocks} 个正文块、{completed_chars}/{total_chars} 个字符{eta}"
            ),
            None,
        );
        self.save_task(&task)
    }

    fn record_online_chapter_progress(
        &self,
        task_id: &str,
        completed_chapters: usize,
        total_chapters: usize,
        reused: bool,
    ) -> Result<()> {
        let mut task = self.load_task(task_id)?;
        let progress = 18
            + u8::try_from(completed_chapters.saturating_mul(22) / total_chapters.max(1))
                .unwrap_or(22);
        update_summary(
            &mut task.summary,
            "running",
            "converting",
            progress.min(40),
            &format!("已完成 {completed_chapters}/{total_chapters} 个在线章节"),
            None,
        );
        self.save_task(&task)?;
        self.append_event(
            task_id,
            "script",
            &format!(
                "{}在线章节 {completed_chapters}/{total_chapters}{}",
                if reused { "恢复" } else { "" },
                if reused { "" } else { " 完成" }
            ),
            if reused {
                "已复用上次完成的章节快照"
            } else {
                "章节正文与资源已写入来源快照"
            },
        )
    }

    fn record_pdf_page_progress(
        &self,
        task_id: &str,
        completed_pages: usize,
        total_pages: usize,
        page: usize,
        reused: bool,
        attempts: usize,
    ) -> Result<()> {
        let mut task = self.load_task(task_id)?;
        let progress = 18
            + u8::try_from(completed_pages.saturating_mul(22) / total_pages.max(1)).unwrap_or(22);
        update_summary(
            &mut task.summary,
            "running",
            "converting",
            progress.min(40),
            &format!("Agent 已完成 {completed_pages}/{total_pages} 个 PDF 页面"),
            None,
        );
        self.save_task(&task)?;
        let detail = if reused {
            "页面结构与图片区域已通过校验，复用上次检查点".to_string()
        } else if attempts > 1 {
            format!(
                "经过 {attempts} 次执行后，Agent 页面结构与图片区域已通过 GoodReader 完整性校验"
            )
        } else {
            "Agent 页面结构与图片区域已通过 GoodReader 完整性校验".to_string()
        };
        self.append_event(
            task_id,
            "agent",
            &format!(
                "{} PDF 第 {page} 页（{completed_pages}/{total_pages}）",
                if reused { "恢复" } else { "Agent 完成" }
            ),
            &detail,
        )
    }

    fn preflight_html(&self, source: &Path) -> Result<ImportPreflight> {
        let source = source.canonicalize().context("无法读取 HTML 来源目录")?;
        if !source.is_dir() {
            bail!("请选择 HTML 书籍目录");
        }
        let token = Uuid::new_v4().to_string();
        let workspace = self.root.join(&token);
        let snapshot = workspace.join("snapshot/source");
        copy_directory(&source, &snapshot)?;
        let snapshot = snapshot.canonicalize().context("无法确认 HTML 来源快照")?;
        let mut html_files = collect_files_with_extensions(&snapshot, &["html", "htm"])?;
        html_files.sort();
        if html_files.is_empty() {
            bail!("所选目录中没有 HTML 文件");
        }
        let contract_manifest =
            read_json::<crate::models::BookManifest>(&snapshot.join("book.json")).ok();
        let entry = contract_manifest
            .as_ref()
            .and_then(|manifest| resolve_package_file(&snapshot, &manifest.entry).ok())
            .filter(|path| html_files.contains(path))
            .unwrap_or_else(|| choose_html_entry(&html_files));
        let entry_html = fs::read_to_string(&entry).context("HTML 必须使用 UTF-8")?;
        let all_text = html_files
            .iter()
            .filter_map(|path| fs::read_to_string(path).ok())
            .map(|html| visible_text(&html))
            .collect::<Vec<_>>()
            .join("\n");
        let (language, confidence) = detect_language(&all_text);
        let title = contract_manifest
            .as_ref()
            .map(|manifest| manifest.title.clone())
            .or_else(|| document_title(&entry_html))
            .unwrap_or_else(|| {
                source
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        let original_title = contract_manifest
            .as_ref()
            .and_then(|manifest| manifest.original_title.clone())
            .unwrap_or_else(|| title.clone());
        let author = contract_manifest
            .as_ref()
            .map(|manifest| manifest.author.clone())
            .or_else(|| document_author(&entry_html))
            .unwrap_or_else(|| "未知作者".to_string());
        let chapter_candidates = contract_manifest
            .as_ref()
            .map(|manifest| {
                manifest
                    .chapters
                    .iter()
                    .filter_map(|chapter| {
                        let path = resolve_package_file(&snapshot, &chapter.path).ok()?;
                        html_files
                            .contains(&path)
                            .then_some((chapter.title.clone(), path))
                    })
                    .enumerate()
                    .map(|(index, (title, path))| ImportChapterCandidate {
                        id: format!("candidate-{:04}", index + 1),
                        title,
                        source: path
                            .strip_prefix(&snapshot)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .to_string(),
                        selected: true,
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|chapters| !chapters.is_empty())
            .unwrap_or_else(|| {
                html_files
                    .iter()
                    .filter(|path| **path != entry)
                    .enumerate()
                    .map(|(index, path)| {
                        let html = fs::read_to_string(path).unwrap_or_default();
                        ImportChapterCandidate {
                            id: format!("candidate-{:04}", index + 1),
                            title: document_title(&html).unwrap_or_else(|| file_stem_title(path)),
                            source: path
                                .strip_prefix(&snapshot)
                                .unwrap_or(path)
                                .to_string_lossy()
                                .to_string(),
                            selected: true,
                        }
                    })
                    .collect()
            });
        let chapter_candidates = if chapter_candidates.is_empty() {
            vec![ImportChapterCandidate {
                id: "candidate-0001".to_string(),
                title: title.clone(),
                source: entry
                    .strip_prefix(&snapshot)
                    .unwrap_or(&entry)
                    .to_string_lossy()
                    .to_string(),
                selected: true,
            }]
        } else {
            chapter_candidates
        };
        let dynamic = html_files.iter().any(|path| {
            fs::read_to_string(path)
                .map(|html| has_scripts(&html) && visible_text(&html).chars().count() < 120)
                .unwrap_or(false)
        });
        let warnings = dynamic
            .then(|| "入口正文依赖脚本，生成时会在隔离临时配置中渲染后静态化".to_string())
            .into_iter()
            .collect::<Vec<_>>();
        let preflight = ImportPreflight {
            token: token.clone(),
            kind: ImportSourceKind::Html,
            source_name: source
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            title: title.clone(),
            original_title,
            author,
            language,
            language_confidence: confidence,
            page_count: None,
            chapter_candidates,
            image_count: collect_files_with_extensions(
                &snapshot,
                &["jpg", "jpeg", "png", "gif", "webp", "avif"],
            )?
            .len(),
            character_count: all_text.chars().count(),
            requires_ocr_pages: Vec::new(),
            uncertain_pages: Vec::new(),
            pdf_mode: None,
            pdf_type: None,
            dynamic_rendering: dynamic,
            warnings,
        };
        write_json(
            &workspace.join("preflight.json"),
            &StoredPreflight {
                preflight: preflight.clone(),
                source_path: Some(snapshot.display().to_string()),
                source_url: None,
            },
        )?;
        Ok(preflight)
    }

    fn preflight_pdf(&self, source: &Path, pdf_mode: PdfImportMode) -> Result<ImportPreflight> {
        let source = source.canonicalize().context("无法读取 PDF 文件")?;
        if source
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| value.eq_ignore_ascii_case("pdf"))
            != Some(true)
        {
            bail!("请选择 PDF 文件");
        }
        let token = Uuid::new_v4().to_string();
        let workspace = self.root.join(&token);
        let snapshot = workspace.join("snapshot");
        fs::create_dir_all(&snapshot)?;
        let snapshot_pdf = snapshot.join("source.pdf");
        fs::copy(&source, &snapshot_pdf)?;
        let info = command_text(&find_tool("pdfinfo")?, &[snapshot_pdf.as_os_str()])?;
        let page_count = parse_pdf_pages(&info)?;
        if page_count == 0 || page_count > MAX_PDF_PAGES {
            bail!("PDF 页数必须在 1 到 {MAX_PDF_PAGES} 之间");
        }
        let text_path = snapshot.join("source.txt");
        let status = Command::new(find_tool("pdftotext")?)
            .args(["-enc", "UTF-8"])
            .arg(&snapshot_pdf)
            .arg(&text_path)
            .status()?;
        if !status.success() {
            bail!("无法提取 PDF 文本层，文件可能已加密或损坏");
        }
        let text = fs::read_to_string(&text_path).unwrap_or_default();
        let pages = split_pdf_pages(&text, page_count);
        let image_pages = pdf_image_pages(&snapshot_pdf).unwrap_or_default();
        let detected_ocr_pages = pdf_pages_requiring_ocr(&pages, &image_pages);
        let pdf_type = if detected_ocr_pages.is_empty() {
            "digital"
        } else if detected_ocr_pages.len() == page_count {
            "scanned"
        } else {
            "mixed"
        };
        let (requires_ocr_pages, uncertain_pages) = match &pdf_mode {
            PdfImportMode::Auto => (detected_ocr_pages.clone(), Vec::new()),
            PdfImportMode::TextLayer => (Vec::new(), detected_ocr_pages.clone()),
            PdfImportMode::Ocr => ((1..=page_count).collect(), Vec::new()),
        };
        let (language, confidence) = detect_language(&text);
        let title = pdf_info_value(&info, "Title")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| file_stem_title(&source));
        let author = pdf_info_value(&info, "Author")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "未知作者".to_string());
        let mut warnings = if requires_ocr_pages.is_empty() {
            Vec::new()
        } else {
            vec![format!(
                "检测到 {} 个正文扫描页，需要配置本地 OCR 后才能继续",
                requires_ocr_pages.len()
            )]
        };
        if !uncertain_pages.is_empty() {
            warnings.push(format!(
                "已按用户选择强制使用 PDF 文本层；仍有 {} 页文本稀疏，请在生成后重点检查",
                uncertain_pages.len()
            ));
        }
        let preflight = ImportPreflight {
            token: token.clone(),
            kind: ImportSourceKind::Pdf,
            source_name: source
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            title: title.clone(),
            original_title: title.clone(),
            author,
            language,
            language_confidence: confidence,
            page_count: Some(page_count),
            chapter_candidates: detect_pdf_chapters(&pages, &title),
            image_count: image_pages.len(),
            character_count: text
                .chars()
                .filter(|character| !character.is_whitespace())
                .count(),
            requires_ocr_pages,
            uncertain_pages,
            pdf_mode: Some(pdf_mode),
            pdf_type: Some(pdf_type.to_string()),
            dynamic_rendering: false,
            warnings,
        };
        write_json(
            &workspace.join("preflight.json"),
            &StoredPreflight {
                preflight: preflight.clone(),
                source_path: Some(snapshot_pdf.display().to_string()),
                source_url: None,
            },
        )?;
        Ok(preflight)
    }

    fn load_preflight(&self, token: &str) -> Result<StoredPreflight> {
        validate_id(token)?;
        read_json(&self.root.join(token).join("preflight.json"))
            .context("导入预检已失效，请重新选择来源")
    }

    fn load_task(&self, id: &str) -> Result<StoredTask> {
        validate_id(id)?;
        read_json(&self.root.join(id).join("task.json")).context("书籍生成任务不存在")
    }

    fn save_task(&self, task: &StoredTask) -> Result<()> {
        write_json(&self.root.join(&task.summary.id).join("task.json"), task)
    }

    fn append_event(&self, id: &str, kind: &str, title: &str, detail: &str) -> Result<()> {
        self.append_event_with_context(id, kind, title, detail, EventContext::default())
    }

    fn append_event_with_context(
        &self,
        id: &str,
        kind: &str,
        title: &str,
        detail: &str,
        context: EventContext,
    ) -> Result<()> {
        validate_id(id)?;
        let _event_guard = self.event_lock.lock().expect("生成详情事件锁");
        let seq = self
            .load_events(id)?
            .into_iter()
            .map(|event| event.seq)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let event = ImportTaskEvent {
            id: Uuid::new_v4().to_string(),
            seq,
            kind: kind.to_string(),
            title: title.to_string(),
            detail: truncate_event_detail(detail, 20_000),
            created_at: Utc::now().timestamp_millis(),
            scope: context.scope,
            state: context.state,
            progress: context.progress,
            timing: context.timing,
            runtime: context.runtime,
            metrics: context.metrics,
        };
        let path = self.root.join(id).join("events.jsonl");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        serde_json::to_writer(&mut log, &event)?;
        std::io::Write::write_all(&mut log, b"\n")?;
        std::io::Write::flush(&mut log)?;
        Ok(())
    }

    fn load_events(&self, id: &str) -> Result<Vec<ImportTaskEvent>> {
        validate_id(id)?;
        let task_root = self.root.join(id);
        let legacy_path = task_root.join("events.json");
        let mut events = if legacy_path.is_file() {
            read_json::<Vec<ImportTaskEvent>>(&legacy_path).context("无法读取旧版生成进度详情")?
        } else {
            Vec::new()
        };
        for (index, event) in events.iter_mut().enumerate() {
            if event.seq == 0 {
                event.seq = u64::try_from(index + 1).unwrap_or(u64::MAX);
            }
        }
        let log_path = task_root.join("events.jsonl");
        if log_path.is_file() {
            let content = fs::read_to_string(&log_path).context("无法读取生成进度事件流")?;
            for line in content.lines().filter(|line| !line.trim().is_empty()) {
                let mut event: ImportTaskEvent =
                    serde_json::from_str(line).context("生成进度事件流包含损坏记录")?;
                if event.seq == 0 {
                    event.seq = events
                        .last()
                        .map(|previous| previous.seq.saturating_add(1))
                        .unwrap_or(1);
                }
                events.push(event);
            }
        }
        Ok(events)
    }

    fn live_agent_output(&self, id: &str) -> Result<Option<ImportTaskEvent>> {
        let task_root = self.root.join(id);
        let translation_batches = task_root.join("translation/batches");
        let pdf_pages = task_root.join("pdf-layout/pages");
        let agent_workspaces = if translation_batches.is_dir() {
            translation_batches
        } else {
            pdf_pages
        };
        if !agent_workspaces.is_dir() {
            return Ok(None);
        }
        let mut directories = fs::read_dir(agent_workspaces)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        directories.sort();
        let Some(current) = directories.last() else {
            return Ok(None);
        };
        let stdout_path = current.join("logs/stdout.log");
        let stderr_path = current.join("logs/stderr.log");
        let stdout = fs::read_to_string(&stdout_path).unwrap_or_default();
        let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
        let process_path = current.join("logs/process.pid");
        let pid = fs::read_to_string(&process_path)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok());
        if stdout.trim().is_empty() && stderr.trim().is_empty() && pid.is_none() {
            return Ok(None);
        }
        let detail = if !stdout.trim().is_empty() {
            summarize_agent_output(&stdout)
        } else if !stderr.trim().is_empty() {
            stderr
        } else if stdout.trim().is_empty() {
            "Agent 进程已启动，正在等待首个输出事件".to_string()
        } else {
            format!("{stdout}\n\n--- stderr ---\n{stderr}")
        };
        let created_at = [&stdout_path, &stderr_path, &process_path]
            .into_iter()
            .filter_map(|path| fs::metadata(path).ok()?.modified().ok())
            .filter_map(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
            .max()
            .unwrap_or_else(|| Utc::now().timestamp_millis());
        let task = self.load_task(id)?;
        let persisted = self.load_events(id)?.into_iter().rev().find(|event| {
            event.scope.as_deref().is_some_and(|scope| {
                scope.starts_with("translation.batch.") || scope.starts_with("pdf.page.")
            })
        });
        let started_at = persisted
            .as_ref()
            .and_then(|event| event.timing.as_ref())
            .map(|timing| timing.started_at)
            .unwrap_or(created_at);
        let elapsed_ms =
            u64::try_from(Utc::now().timestamp_millis().saturating_sub(started_at)).unwrap_or(0);
        Ok(Some(ImportTaskEvent {
            id: "live-agent-output".to_string(),
            seq: 0,
            kind: "agent".to_string(),
            title: "Agent 实时输出".to_string(),
            detail: truncate_event_detail(&detail, 20_000),
            created_at,
            scope: persisted.as_ref().and_then(|event| event.scope.clone()),
            state: Some("running".to_string()),
            progress: persisted.as_ref().and_then(|event| event.progress.clone()),
            timing: Some(ImportTaskEventTiming {
                started_at,
                elapsed_ms,
                eta_ms: persisted
                    .as_ref()
                    .and_then(|event| event.timing.as_ref())
                    .and_then(|timing| timing.eta_ms),
            }),
            runtime: Some(ImportTaskEventRuntime {
                id: task
                    .request
                    .runtime_id
                    .unwrap_or_else(|| "unknown".to_string()),
                model: agent_output_model(&stdout),
                session_id: None,
                pid,
            }),
            metrics: persisted.and_then(|event| event.metrics),
        }))
    }

    fn should_stop(&self, id: &str) -> bool {
        self.cancelled.lock().expect("取消任务锁").contains(id)
            || self.paused.lock().expect("暂停任务锁").contains(id)
    }

    fn checkpoint(&self, id: &str) -> Result<()> {
        if self.cancelled.lock().expect("取消任务锁").contains(id) {
            bail!("任务已取消");
        }
        if self.paused.lock().expect("暂停任务锁").contains(id) {
            bail!("任务已暂停");
        }
        Ok(())
    }

    async fn wait_retry_delay(&self, id: &str, delay: std::time::Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + delay;
        loop {
            self.checkpoint(id)?;
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(());
            }
            tokio::time::sleep(
                deadline
                    .saturating_duration_since(now)
                    .min(std::time::Duration::from_millis(250)),
            )
            .await;
        }
    }

    fn cleanup_task_outputs(&self, id: &str) -> Result<()> {
        let root = self.root.join(id);
        for name in [
            "prepared-source",
            "candidate-books",
            "translation",
            "pdf-layout",
        ] {
            let path = root.join(name);
            if path.exists() {
                fs::remove_dir_all(path)?;
            }
        }
        Ok(())
    }

    fn recover_interrupted_tasks(&self) -> Result<()> {
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path().join("task.json");
            if !path.is_file() {
                continue;
            }
            let Ok(mut task) = read_json::<StoredTask>(&path) else {
                continue;
            };
            if task.summary.status == "running" || task.summary.status == "queued" {
                let progress = task.summary.progress;
                update_summary(
                    &mut task.summary,
                    "paused",
                    "paused",
                    progress,
                    "应用上次退出后已安全暂停，可继续任务",
                    None,
                );
                write_json(&path, &task)?;
            }
        }
        Ok(())
    }
}

fn conversion_script_description(kind: &ImportSourceKind) -> (&'static str, &'static str) {
    match kind {
        ImportSourceKind::Html => (
            "执行 HTML 转换脚本",
            "复制静态资源、移除来源脚本并转换为 GoodReader 统一结构",
        ),
        ImportSourceKind::Pdf => (
            "启动逐页 Agent 排版",
            "渲染页面快照，由所选 Agent 恢复阅读顺序、语义块和完整图片区域",
        ),
        ImportSourceKind::Url => (
            "执行网页静态化脚本",
            "抓取已确认章节和资源；需要时由隔离浏览器生成稳定 DOM",
        ),
    }
}

fn prepare_source<F>(
    stored: &StoredPreflight,
    request: &StartImportRequest,
    destination: &Path,
    on_online_chapter: F,
) -> Result<PreparedSource>
where
    F: FnMut(usize, usize, bool) -> Result<()>,
{
    match stored.preflight.kind {
        ImportSourceKind::Html => prepare_html_source(stored, request, destination),
        ImportSourceKind::Pdf => bail!("PDF 来源必须通过异步逐页 Agent 转换器处理"),
        ImportSourceKind::Url => {
            prepare_url_source_with_progress(stored, request, destination, on_online_chapter)
        }
    }
}

fn prepare_html_source(
    stored: &StoredPreflight,
    request: &StartImportRequest,
    destination: &Path,
) -> Result<PreparedSource> {
    let source = PathBuf::from(
        stored
            .source_path
            .as_deref()
            .context("HTML 来源快照不存在")?,
    );
    copy_directory(&source, destination)?;
    let selected = request
        .chapters
        .iter()
        .filter(|chapter| chapter.selected)
        .map(|chapter| chapter.source.as_str())
        .collect::<HashSet<_>>();
    let html_files = collect_files_with_extensions(destination, &["html", "htm"])?;
    let entry = choose_html_entry(&html_files);
    for path in &html_files {
        let relative = path
            .strip_prefix(destination)
            .unwrap_or(path)
            .to_string_lossy();
        if *path != entry && !selected.contains(relative.as_ref()) {
            fs::remove_file(path)?;
        }
    }
    if stored.preflight.dynamic_rendering {
        for html_path in collect_files_with_extensions(destination, &["html", "htm"])? {
            let source = fs::read_to_string(&html_path).unwrap_or_default();
            if has_scripts(&source) && visible_text(&source).chars().count() < 120 {
                if let Ok(rendered) = render_local_html(&html_path) {
                    fs::write(&html_path, rendered)?;
                }
            }
        }
    }
    let image_count = request
        .chapters
        .iter()
        .filter(|chapter| chapter.selected)
        .filter_map(|chapter| fs::read_to_string(destination.join(&chapter.source)).ok())
        .map(|html| image_reference_count(&html))
        .sum();
    Ok(PreparedSource {
        directory: destination.to_path_buf(),
        image_count,
        warnings: Vec::new(),
    })
}

fn repeated_pdf_lines(pages: &[String]) -> HashSet<String> {
    let mut counts = HashMap::<String, usize>::new();
    for page in pages {
        let lines = page
            .lines()
            .map(normalize_pdf_line)
            .filter(|line| !line.is_empty() && line.chars().count() <= 120)
            .collect::<Vec<_>>();
        let unique = lines
            .iter()
            .take(3)
            .chain(lines.iter().rev().take(3))
            .cloned()
            .collect::<HashSet<_>>();
        for line in unique {
            *counts.entry(line).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .filter_map(|(line, count)| (count >= 3).then_some(line))
        .collect()
}

fn pdf_source_lines(text: &str, repeated_lines: &HashSet<String>) -> Vec<PdfSourceLine> {
    let page_number = Regex::new(r"(?i)^(?:[0-9]+|[ivxlcdm]+)$").expect("页码正则固定有效");
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
        .map(|(index, text)| {
            let normalized = normalize_pdf_line(text);
            PdfSourceLine {
                id: format!("l{:04}", index + 1),
                text: text.to_string(),
                removable: page_number.is_match(text) || repeated_lines.contains(&normalized),
            }
        })
        .collect()
}

fn normalize_pdf_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn render_pdf_page(pdf: &Path, page: usize, output: &Path, dpi: usize) -> Result<()> {
    let prefix = output.with_extension("");
    let status = Command::new(find_tool("pdftoppm")?)
        .args(["-f", &page.to_string(), "-l", &page.to_string()])
        .args(["-singlefile", "-r", &dpi.to_string(), "-png"])
        .arg(pdf)
        .arg(&prefix)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() || !output.is_file() {
        bail!("无法渲染 PDF 第 {page} 页供 Agent 排版");
    }
    Ok(())
}

fn render_pdf_region(
    pdf: &Path,
    page: usize,
    crop: &PdfCropBox,
    output: &Path,
    dpi: usize,
) -> Result<()> {
    let prefix = output.with_extension("");
    let status = Command::new(find_tool("pdftoppm")?)
        .args(["-f", &page.to_string(), "-l", &page.to_string()])
        .args([
            "-singlefile",
            "-r",
            &dpi.to_string(),
            "-x",
            &crop.x.to_string(),
            "-y",
            &crop.y.to_string(),
            "-W",
            &crop.width.to_string(),
            "-H",
            &crop.height.to_string(),
            "-png",
        ])
        .arg(pdf)
        .arg(&prefix)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() || !output.is_file() {
        bail!("无法渲染 PDF 第 {page} 页的完整图片区域");
    }
    Ok(())
}

fn png_dimensions(path: &Path) -> Result<(usize, usize)> {
    let bytes = fs::read(path).context("无法读取 PDF 页面图像")?;
    if bytes.len() < 24 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        bail!("PDF 页面渲染结果不是有效 PNG");
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("PNG 宽度字节"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("PNG 高度字节"));
    Ok((width as usize, height as usize))
}

fn render_pdf_cover(pdf: &Path, destination: &Path) -> Result<()> {
    let cover_base = destination.join("cover");
    let status = Command::new(find_tool("pdftoppm")?)
        .args([
            "-f",
            "1",
            "-l",
            "1",
            "-singlefile",
            "-scale-to-x",
            "900",
            "-scale-to-y",
            "-1",
            "-png",
        ])
        .arg(pdf)
        .arg(&cover_base)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        bail!("无法从 PDF 生成封面");
    }
    Ok(())
}

#[cfg(test)]
fn prepare_url_source(
    stored: &StoredPreflight,
    request: &StartImportRequest,
    destination: &Path,
) -> Result<PreparedSource> {
    prepare_url_source_with_progress(stored, request, destination, |_, _, _| Ok(()))
}

fn prepare_url_source_with_progress<F>(
    stored: &StoredPreflight,
    request: &StartImportRequest,
    destination: &Path,
    mut on_chapter: F,
) -> Result<PreparedSource>
where
    F: FnMut(usize, usize, bool) -> Result<()>,
{
    fs::create_dir_all(destination.join("chapters"))?;
    fs::create_dir_all(destination.join("assets"))?;
    let checkpoints = destination.join(".goodreader-checkpoints");
    fs::create_dir_all(&checkpoints)?;
    let base = Url::parse(stored.source_url.as_deref().context("在线来源 URL 缺失")?)?;
    let selected = request
        .chapters
        .iter()
        .filter(|chapter| chapter.selected)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        bail!("至少保留一个在线章节");
    }
    let mut images = fs::read_dir(destination.join("assets"))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .count();
    for (index, chapter) in selected.iter().enumerate() {
        let chapter_path = destination.join(format!("chapters/chapter-{:04}.html", index + 1));
        let checkpoint = checkpoints.join(format!("chapter-{:04}.done", index + 1));
        if checkpoint.is_file() && chapter_path.is_file() {
            on_chapter(index + 1, selected.len(), true)?;
            continue;
        }
        let url = Url::parse(&chapter.source).or_else(|_| base.join(&chapter.source))?;
        enforce_source_scope(&base, &url)?;
        let html = fetch_url(&url, stored.preflight.dynamic_rendering)?;
        let localized = localize_images(&html, &url, destination, &mut images)?;
        let body = extract_main_html(&localized);
        let document = html_document(&chapter.title, &request.author, &body);
        let temporary = chapter_path.with_extension("html.tmp");
        fs::write(&temporary, document)?;
        fs::rename(&temporary, &chapter_path)?;
        fs::write(checkpoint, chapter.source.as_bytes())?;
        on_chapter(index + 1, selected.len(), false)?;
    }
    let index = build_index_html(&request.title, &request.author, &selected);
    fs::write(destination.join("index.html"), index)?;
    Ok(PreparedSource {
        directory: destination.to_path_buf(),
        image_count: images,
        warnings: Vec::new(),
    })
}

fn validate_start_request(request: &StartImportRequest, preflight: &ImportPreflight) -> Result<()> {
    if request.title.trim().is_empty() || request.author.trim().is_empty() {
        bail!("书名和作者不能为空");
    }
    if request.title.chars().count() > 240 || request.author.chars().count() > 240 {
        bail!("书名或作者过长");
    }
    if request.chapters.is_empty() || !request.chapters.iter().any(|chapter| chapter.selected) {
        bail!("至少保留一个章节");
    }
    let allowed = preflight
        .chapter_candidates
        .iter()
        .map(|chapter| chapter.source.as_str())
        .collect::<HashSet<_>>();
    if request
        .chapters
        .iter()
        .any(|chapter| !allowed.contains(chapter.source.as_str()))
    {
        bail!("章节清单包含预检之外的来源");
    }
    if request.preserve_original && !request.translate {
        bail!("只有翻译书籍才能保留对照原文");
    }
    Ok(())
}

fn update_summary(
    summary: &mut ImportTaskSummary,
    status: &str,
    stage: &str,
    progress: u8,
    detail: &str,
    error: Option<String>,
) {
    summary.status = status.to_string();
    summary.stage = stage.to_string();
    summary.progress = progress.min(100);
    summary.detail = detail.to_string();
    summary.error = error;
    summary.updated_at = Utc::now().timestamp_millis();
}

fn friendly_error(error: &anyhow::Error) -> String {
    let value = format!("{error:#}");
    if value.chars().count() <= 1_500 {
        value
    } else {
        value.chars().take(1_500).collect::<String>() + "…"
    }
}

fn truncate_event_detail(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        value.chars().take(max_chars).collect::<String>() + "\n…内容过长，已截断"
    }
}

fn summarize_agent_output(output: &str) -> String {
    let mut messages = Vec::new();
    let lines = output.lines().collect::<Vec<_>>();
    for line in lines.iter().skip(lines.len().saturating_sub(80)) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            if !line.trim().is_empty() {
                messages.push(line.trim().to_string());
            }
            continue;
        };
        let event_type = value
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        match event_type {
            "system" if value.get("subtype").and_then(|value| value.as_str()) == Some("init") => {
                messages.push("Agent 会话已建立".to_string());
            }
            "assistant" => {
                if let Some(content) = value
                    .pointer("/message/content")
                    .and_then(|value| value.as_array())
                {
                    for item in content {
                        if let Some(text) = item.get("text").and_then(|value| value.as_str()) {
                            if !text.trim().is_empty() {
                                messages.push(text.trim().to_string());
                            }
                        }
                    }
                }
            }
            "stream_event" => {
                if let Some(text) = value
                    .pointer("/event/delta/text")
                    .and_then(|value| value.as_str())
                {
                    if !text.trim().is_empty() {
                        messages.push(text.trim().to_string());
                    }
                }
            }
            "thread.started" => messages.push("Codex 会话已建立".to_string()),
            "turn.started" => messages.push("Agent 正在生成结构化译文".to_string()),
            "item.completed" => {
                if let Some(text) = value.pointer("/item/text").and_then(|value| value.as_str()) {
                    messages.push(text.trim().to_string());
                }
            }
            "result" | "turn.completed" => messages.push("Agent 已返回结果，正在校验".to_string()),
            _ => {}
        }
    }
    if messages.is_empty() {
        "Agent 正在处理当前批次，尚未产生可展示文本".to_string()
    } else {
        messages.join("\n")
    }
}

fn agent_output_model(output: &str) -> Option<String> {
    output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|value| {
            value
                .get("model")
                .and_then(|model| model.as_str())
                .map(str::to_string)
        })
}

fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("无法读取 {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("{} 不是合法 JSON", path.display()))
}

fn validate_id(value: &str) -> Result<()> {
    Uuid::parse_str(value).context("任务标识无效")?;
    Ok(())
}

fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        fs::remove_dir_all(destination)?;
    }
    fs::create_dir_all(destination)?;
    let mut pending = vec![(source.to_path_buf(), destination.to_path_buf())];
    let mut files = 0usize;
    let mut bytes = 0u64;
    while let Some((from, to)) = pending.pop() {
        for entry in fs::read_dir(&from)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let source_path = entry.path();
            let destination_path = to.join(entry.file_name());
            if kind.is_symlink() {
                bail!("来源目录不得包含符号链接：{}", source_path.display());
            }
            if kind.is_dir() {
                fs::create_dir_all(&destination_path)?;
                pending.push((source_path, destination_path));
                continue;
            }
            if !kind.is_file() {
                bail!("来源目录包含不支持的文件类型：{}", source_path.display());
            }
            files += 1;
            bytes = bytes.saturating_add(entry.metadata()?.len());
            if files > 20_000 || bytes > 2 * 1024 * 1024 * 1024 {
                bail!("来源目录超过 20,000 个文件或 2 GiB 限制");
            }
            fs::copy(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

fn collect_files_with_extensions(root: &Path, extensions: &[&str]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                bail!("来源快照包含符号链接");
            }
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                let extension = path
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if extensions.contains(&extension.as_str()) {
                    files.push(path);
                }
            }
        }
    }
    Ok(files)
}

fn choose_html_entry(files: &[PathBuf]) -> PathBuf {
    files
        .iter()
        .find(|path| {
            path.file_name()
                .and_then(|value| value.to_str())
                .map(|value| {
                    value.eq_ignore_ascii_case("index.html")
                        || value.eq_ignore_ascii_case("index.htm")
                })
                .unwrap_or(false)
        })
        .cloned()
        .or_else(|| files.first().cloned())
        .unwrap_or_default()
}

fn parse_public_url(value: &str) -> Result<Url> {
    let url = Url::parse(value.trim()).context("请输入完整的 http 或 https 链接")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("只支持公开的 http 或 https 链接");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("在线来源链接不能包含账号或密码");
    }
    let host = url.host_str().unwrap_or_default();
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        bail!("在线来源必须是公开地址，不能使用 localhost");
    }
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        let private = match address {
            std::net::IpAddr::V4(address) => {
                address.is_private()
                    || address.is_loopback()
                    || address.is_link_local()
                    || address.is_unspecified()
                    || address.is_broadcast()
            }
            std::net::IpAddr::V6(address) => {
                address.is_loopback() || address.is_unspecified() || address.is_unique_local()
            }
        };
        if private {
            bail!("在线来源必须是公开地址，不能访问本机或私有网络地址");
        }
    }
    Ok(url)
}

fn enforce_source_scope(base: &Url, candidate: &Url) -> Result<()> {
    if candidate.scheme() != base.scheme()
        || candidate.host_str() != base.host_str()
        || candidate.port_or_known_default() != base.port_or_known_default()
    {
        bail!("在线章节必须与起始链接同源");
    }
    let base_path = base.path();
    let prefix = if base_path.ends_with('/') {
        base_path.to_string()
    } else {
        base_path
            .rsplit_once('/')
            .map(|(parent, _)| format!("{parent}/"))
            .unwrap_or_else(|| "/".to_string())
    };
    if !candidate.path().starts_with(&prefix) && candidate.path() != base.path() {
        bail!("在线章节超出起始链接的路径范围");
    }
    Ok(())
}

fn fetch_url(url: &Url, dynamic: bool) -> Result<String> {
    if dynamic {
        if let Ok(rendered) = render_online_html(url) {
            if visible_text(&rendered).chars().count() >= 80 {
                return Ok(rendered);
            }
        }
    }
    let output = Command::new("/usr/bin/curl")
        .args([
            "--location",
            "--max-redirs",
            "5",
            "--connect-timeout",
            "10",
            "--max-time",
            "60",
            "--fail",
            "--silent",
            "--show-error",
            "--proto",
            "=http,https",
        ])
        .arg(url.as_str())
        .output()
        .context("无法启动在线内容下载")?;
    if !output.status.success() {
        bail!(
            "无法下载在线来源：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if output.stdout.len() as u64 > MAX_DOWNLOAD_BYTES {
        bail!("在线页面超过 64 MiB 限制");
    }
    String::from_utf8(output.stdout).context("在线页面不是 UTF-8 HTML")
}

fn render_online_html(url: &Url) -> Result<String> {
    let chrome = find_chrome()?;
    let profile = std::env::temp_dir().join(format!("goodreader-web-{}", Uuid::new_v4()));
    fs::create_dir_all(&profile)?;
    let output = Command::new(chrome)
        .args([
            "--headless=new",
            "--disable-extensions",
            "--disable-sync",
            "--disable-background-networking",
            "--no-first-run",
            "--no-default-browser-check",
            "--virtual-time-budget=8000",
            "--dump-dom",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(url.as_str())
        .output();
    let _ = fs::remove_dir_all(&profile);
    let output = output?;
    if !output.status.success() || output.stdout.is_empty() {
        bail!("隔离浏览器无法生成稳定页面");
    }
    String::from_utf8(output.stdout).context("隔离浏览器返回了无效 HTML")
}

fn render_local_html(path: &Path) -> Result<String> {
    let chrome = find_chrome()?;
    let profile = std::env::temp_dir().join(format!("goodreader-local-{}", Uuid::new_v4()));
    fs::create_dir_all(&profile)?;
    let url = Url::from_file_path(path).map_err(|_| anyhow!("无法生成本地 HTML URL"))?;
    let output = Command::new(chrome)
        .args([
            "--headless=new",
            "--disable-extensions",
            "--disable-sync",
            "--disable-background-networking",
            "--disable-features=NetworkService",
            "--no-first-run",
            "--virtual-time-budget=5000",
            "--dump-dom",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(url.as_str())
        .output();
    let _ = fs::remove_dir_all(&profile);
    let output = output?;
    if !output.status.success() || output.stdout.is_empty() {
        bail!("隔离浏览器无法渲染本地 HTML");
    }
    String::from_utf8(output.stdout).context("隔离浏览器返回了无效 HTML")
}

fn find_chrome() -> Result<PathBuf> {
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
    .context("找不到可用于隔离动态渲染的 Chromium 浏览器")
}

fn discover_chapter_links(base: &Url, html: &str) -> Vec<ImportChapterCandidate> {
    let link_regex =
        Regex::new(r#"(?is)<a\b[^>]*\bhref\s*=\s*[\"']([^\"'#]+)[\"'][^>]*>(.*?)</a\s*>"#)
            .expect("链接正则固定有效");
    let mut seen = HashSet::new();
    let mut candidates = vec![ImportChapterCandidate {
        id: "candidate-0001".to_string(),
        title: document_title(html).unwrap_or_else(|| "当前页面".to_string()),
        source: base.as_str().to_string(),
        selected: true,
    }];
    seen.insert(normalized_url(base));
    for captures in link_regex.captures_iter(html) {
        if candidates.len() >= MAX_ONLINE_CHAPTERS {
            break;
        }
        let Some(href) = captures.get(1).map(|value| value.as_str()) else {
            continue;
        };
        let Ok(url) = base.join(href) else {
            continue;
        };
        if enforce_source_scope(base, &url).is_err() {
            continue;
        }
        let normalized = normalized_url(&url);
        if !seen.insert(normalized) {
            continue;
        }
        let title = captures
            .get(2)
            .map(|value| visible_text(value.as_str()))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                url.path_segments()
                    .and_then(|mut segments| segments.next_back())
                    .unwrap_or("章节")
                    .to_string()
            });
        candidates.push(ImportChapterCandidate {
            id: format!("candidate-{:04}", candidates.len() + 1),
            title,
            source: url.as_str().to_string(),
            selected: true,
        });
    }
    candidates
}

fn normalized_url(url: &Url) -> String {
    let mut value = url.clone();
    value.set_fragment(None);
    value.to_string()
}

fn document_title(html: &str) -> Option<String> {
    let title = Regex::new(r"(?is)<title\b[^>]*>(.*?)</title\s*>").ok()?;
    let heading = Regex::new(r"(?is)<h1\b[^>]*>(.*?)</h1\s*>").ok()?;
    title
        .captures(html)
        .or_else(|| heading.captures(html))
        .and_then(|captures| captures.get(1))
        .map(|value| visible_text(value.as_str()))
        .filter(|value| !value.is_empty())
}

fn document_author(html: &str) -> Option<String> {
    extract_meta_content(html, "author")
}

fn extract_meta_content(html: &str, name: &str) -> Option<String> {
    let escaped = regex::escape(name);
    let forward = Regex::new(&format!(r#"(?is)<meta\b[^>]*(?:name|property)\s*=\s*[\"']{escaped}[\"'][^>]*content\s*=\s*[\"']([^\"']+)[\"'][^>]*>"#)).ok()?;
    let reverse = Regex::new(&format!(r#"(?is)<meta\b[^>]*content\s*=\s*[\"']([^\"']+)[\"'][^>]*(?:name|property)\s*=\s*[\"']{escaped}[\"'][^>]*>"#)).ok()?;
    forward
        .captures(html)
        .or_else(|| reverse.captures(html))
        .and_then(|captures| captures.get(1))
        .map(|value| decode_entities(value.as_str()).trim().to_string())
}

fn visible_text(html: &str) -> String {
    let without_active = Regex::new(r"(?is)<script\b[^>]*>.*?</script\s*>|<style\b[^>]*>.*?</style\s*>|<noscript\b[^>]*>.*?</noscript\s*>|<template\b[^>]*>.*?</template\s*>")
        .expect("主动内容正则固定有效")
        .replace_all(html, " ");
    let text = Regex::new(r"(?is)<[^>]+>")
        .expect("标签正则固定有效")
        .replace_all(&without_active, " ");
    decode_entities(&text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_entities(value: &str) -> String {
    value
        .replace("&nbsp;", " ")
        .replace("&#160;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn detect_language(text: &str) -> (String, String) {
    let mut han = 0usize;
    let mut latin = 0usize;
    for character in text.chars() {
        if matches!(character as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF) {
            han += 1;
        } else if character.is_ascii_alphabetic() {
            latin += 1;
        }
    }
    let total = han + latin;
    if total < 80 {
        return ("mixed".to_string(), "low".to_string());
    }
    if han * 100 >= total * 35 {
        (
            "zh-CN".to_string(),
            if han * 100 >= total * 60 {
                "high"
            } else {
                "medium"
            }
            .to_string(),
        )
    } else if latin * 100 >= total * 75 {
        ("non-zh".to_string(), "high".to_string())
    } else {
        ("mixed".to_string(), "medium".to_string())
    }
}

fn has_scripts(html: &str) -> bool {
    html.to_ascii_lowercase().contains("<script")
}

fn image_reference_count(html: &str) -> usize {
    Regex::new(r"(?is)<img\b")
        .expect("图片正则固定有效")
        .find_iter(html)
        .count()
}

fn file_stem_title(path: &Path) -> String {
    path.file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("未命名章节")
        .replace(['_', '-'], " ")
}

fn find_tool(name: &str) -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|directory| directory.join(name)));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin").join(name));
    candidates.push(PathBuf::from("/usr/local/bin").join(name));
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .with_context(|| format!("找不到 PDF 转换工具 {name}，请安装 Poppler"))
}

fn command_text(command: &Path, arguments: &[&std::ffi::OsStr]) -> Result<String> {
    let output = Command::new(command).args(arguments).output()?;
    if !output.status.success() {
        bail!(
            "{} 执行失败：{}",
            command.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_pdf_pages(info: &str) -> Result<usize> {
    pdf_info_value(info, "Pages")
        .context("pdfinfo 没有返回页数")?
        .parse::<usize>()
        .context("PDF 页数无效")
}

fn pdf_info_value(info: &str, key: &str) -> Option<String> {
    info.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case(key)
            .then(|| value.trim().to_string())
    })
}

fn split_pdf_pages(text: &str, page_count: usize) -> Vec<String> {
    let mut pages = text
        .split('\u{000c}')
        .map(str::to_string)
        .collect::<Vec<_>>();
    while pages.last().is_some_and(|page| page.trim().is_empty()) && pages.len() > page_count {
        pages.pop();
    }
    pages.resize(page_count, String::new());
    pages.truncate(page_count);
    pages
}

fn meaningful_char_count(text: &str) -> usize {
    text.chars()
        .filter(|character| character.is_alphanumeric())
        .count()
}

fn pdf_pages_requiring_ocr(pages: &[String], image_pages: &BTreeSet<usize>) -> Vec<usize> {
    pages
        .iter()
        .enumerate()
        .filter_map(|(index, page)| {
            let page_number = index + 1;
            (meaningful_char_count(page) < 20 && image_pages.contains(&page_number))
                .then_some(page_number)
        })
        .collect()
}

fn pdf_image_pages(pdf: &Path) -> Result<BTreeSet<usize>> {
    let output = command_text(
        &find_tool("pdfimages")?,
        &[std::ffi::OsStr::new("-list"), pdf.as_os_str()],
    )?;
    let mut pages = BTreeSet::new();
    for line in output.lines() {
        let columns = line.split_whitespace().collect::<Vec<_>>();
        if columns.len() < 4 || columns[0] == "page" || columns[0].starts_with('-') {
            continue;
        }
        if let Ok(page) = columns[0].parse::<usize>() {
            let width = columns
                .get(3)
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or_default();
            let height = columns
                .get(4)
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or_default();
            if width >= 120 && height >= 80 {
                pages.insert(page);
            }
        }
    }
    Ok(pages)
}

fn parse_page_range(value: &str, page_count: usize) -> Result<(usize, usize)> {
    let value = value
        .strip_prefix("pages:")
        .context("PDF 章节页码范围无效")?;
    let (start, end) = value.split_once('-').context("PDF 章节页码范围无效")?;
    let start = start.parse::<usize>()?;
    let end = end.parse::<usize>()?;
    if start == 0 || end < start || end > page_count {
        bail!("PDF 章节页码超出范围");
    }
    Ok((start, end))
}

fn detect_pdf_chapters(pages: &[String], fallback_title: &str) -> Vec<ImportChapterCandidate> {
    let heading = Regex::new(
        r"(?i)^(?:chapter\s+[0-9ivxlcdm]+\b.*|第\s*[一二三四五六七八九十百0-9]+\s*章\b.*)$",
    )
    .expect("PDF 章节标题正则固定有效");
    let mut detected = Vec::<(usize, String, usize)>::new();
    for (index, page) in pages.iter().enumerate() {
        let candidate = page
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .take(40)
            .find(|line| {
                line.chars().count() <= 100
                    && heading.is_match(line)
                    && is_complete_pdf_heading(line)
            });
        if let Some(title) = candidate {
            if let Some(ordinal) = chapter_ordinal(title) {
                if detected
                    .last()
                    .is_none_or(|(_, previous, _)| previous != title)
                {
                    detected.push((index + 1, title.to_string(), ordinal));
                }
            }
        }
    }
    let mut starts = coherent_chapter_sequence(&detected);
    if starts.len() < 3 {
        starts = detected
            .into_iter()
            .map(|(page, title, _)| (page, title))
            .collect();
    }
    if starts.is_empty() {
        return vec![ImportChapterCandidate {
            id: "candidate-0001".to_string(),
            title: fallback_title.to_string(),
            source: format!("pages:1-{}", pages.len()),
            selected: true,
        }];
    }
    if starts[0].0 > 1 {
        starts.insert(0, (1, "前置内容".to_string()));
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, (start, title))| {
            let end = starts
                .get(index + 1)
                .map(|(page, _)| page.saturating_sub(1))
                .unwrap_or(pages.len());
            ImportChapterCandidate {
                id: format!("candidate-{:04}", index + 1),
                title: title.clone(),
                source: format!("pages:{start}-{end}"),
                selected: true,
            }
        })
        .collect()
}

fn is_complete_pdf_heading(value: &str) -> bool {
    let pairs = [('(', ')'), ('（', '）'), ('[', ']'), ('【', '】')];
    pairs.iter().all(|(open, close)| {
        value.chars().filter(|character| character == open).count()
            == value.chars().filter(|character| character == close).count()
    })
}

fn coherent_chapter_sequence(detected: &[(usize, String, usize)]) -> Vec<(usize, String)> {
    if detected.is_empty() {
        return Vec::new();
    }
    let mut lengths = vec![1usize; detected.len()];
    let mut next = vec![None; detected.len()];
    for index in (0..detected.len()).rev() {
        for candidate in index + 1..detected.len() {
            if detected[candidate].2 == detected[index].2 + 1
                && lengths[candidate] + 1 > lengths[index]
            {
                lengths[index] = lengths[candidate] + 1;
                next[index] = Some(candidate);
            }
        }
    }
    let best = (0..detected.len()).max_by(|left, right| {
        lengths[*left]
            .cmp(&lengths[*right])
            .then_with(|| detected[*left].0.cmp(&detected[*right].0))
    });
    let Some(mut index) = best else {
        return Vec::new();
    };
    let mut sequence = Vec::new();
    loop {
        sequence.push((detected[index].0, detected[index].1.clone()));
        let Some(candidate) = next[index] else {
            break;
        };
        index = candidate;
    }
    sequence
}

fn chapter_ordinal(title: &str) -> Option<usize> {
    let english = Regex::new(r"(?i)^chapter\s+([0-9ivxlcdm]+)\b").expect("英文章号正则固定有效");
    if let Some(token) = english
        .captures(title)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str())
    {
        return token.parse().ok().or_else(|| parse_roman_number(token));
    }
    let chinese =
        Regex::new(r"^第\s*([一二三四五六七八九十百0-9]+)\s*章").expect("中文章号正则固定有效");
    chinese
        .captures(title)
        .and_then(|captures| captures.get(1))
        .and_then(|value| parse_chinese_number(value.as_str()))
}

fn parse_roman_number(value: &str) -> Option<usize> {
    let mut total = 0usize;
    let mut previous = 0usize;
    for character in value.to_ascii_uppercase().chars().rev() {
        let current = match character {
            'I' => 1,
            'V' => 5,
            'X' => 10,
            'L' => 50,
            'C' => 100,
            'D' => 500,
            'M' => 1_000,
            _ => return None,
        };
        if current < previous {
            total = total.checked_sub(current)?;
        } else {
            total = total.checked_add(current)?;
            previous = current;
        }
    }
    (total > 0).then_some(total)
}

fn parse_chinese_number(value: &str) -> Option<usize> {
    if let Ok(number) = value.parse() {
        return Some(number);
    }
    let digit = |character| match character {
        '一' => Some(1usize),
        '二' => Some(2),
        '三' => Some(3),
        '四' => Some(4),
        '五' => Some(5),
        '六' => Some(6),
        '七' => Some(7),
        '八' => Some(8),
        '九' => Some(9),
        _ => None,
    };
    let mut total = 0usize;
    let mut pending = 0usize;
    for character in value.chars() {
        match character {
            '百' => {
                total += pending.max(1) * 100;
                pending = 0;
            }
            '十' => {
                total += pending.max(1) * 10;
                pending = 0;
            }
            _ => pending = digit(character)?,
        }
    }
    total += pending;
    (total > 0).then_some(total)
}

fn html_document(title: &str, author: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"author\" content=\"{}\"><title>{}</title><style>body{{max-width:760px;margin:auto;padding:48px 28px;line-height:1.8}}img{{max-width:100%;height:auto}}figure{{margin:2em 0}}figcaption{{color:#777;text-align:center}}.pdf-toc ol{{list-style:none;margin:0;padding:0}}.pdf-toc li{{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:1rem;padding:.35rem 0;border-bottom:1px dotted color-mix(in srgb,currentColor 24%,transparent)}}.pdf-toc-title{{min-width:0;overflow-wrap:anywhere}}.pdf-toc-page{{font-variant-numeric:tabular-nums;color:#777}}</style></head><body><h1>{}</h1>{}</body></html>",
        escape_html(author),
        escape_html(title),
        escape_html(title),
        body
    )
}

fn build_index_html(title: &str, author: &str, chapters: &[&ImportChapterCandidate]) -> String {
    let links = chapters
        .iter()
        .enumerate()
        .map(|(index, chapter)| {
            format!(
                "<li><a href=\"chapters/chapter-{:04}.html\">{}</a></li>",
                index + 1,
                escape_html(&chapter.title)
            )
        })
        .collect::<String>();
    html_document(
        title,
        author,
        &format!("<p>{}</p><ol>{links}</ol>", escape_html(author)),
    )
}

fn extract_main_html(html: &str) -> String {
    let body = Regex::new(r"(?is)<body\b[^>]*>(.*?)</body\s*>")
        .expect("正文正则固定有效")
        .captures(html)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str())
        .unwrap_or(html);
    Regex::new(r"(?is)<nav\b[^>]*>.*?</nav\s*>|<header\b[^>]*>.*?</header\s*>|<footer\b[^>]*>.*?</footer\s*>|<aside\b[^>]*>.*?</aside\s*>|<form\b[^>]*>.*?</form\s*>")
        .expect("非正文正则固定有效")
        .replace_all(body, "")
        .into_owned()
}

fn localize_images(
    html: &str,
    page_url: &Url,
    destination: &Path,
    image_count: &mut usize,
) -> Result<String> {
    let image_regex = Regex::new(r#"(?is)(<img\b[^>]*\bsrc\s*=\s*[\"'])([^\"']+)([\"'][^>]*>)"#)
        .expect("图片来源正则固定有效");
    let mut result = String::with_capacity(html.len());
    let mut last = 0usize;
    for captures in image_regex.captures_iter(html) {
        let Some(matched) = captures.get(0) else {
            continue;
        };
        result.push_str(&html[last..matched.start()]);
        let prefix = captures
            .get(1)
            .map(|value| value.as_str())
            .unwrap_or_default();
        let source = captures
            .get(2)
            .map(|value| value.as_str())
            .unwrap_or_default();
        let suffix = captures
            .get(3)
            .map(|value| value.as_str())
            .unwrap_or_default();
        let replacement = if let Ok(url) = page_url.join(source) {
            if matches!(url.scheme(), "http" | "https") {
                let extension = Path::new(url.path())
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or("jpg")
                    .to_ascii_lowercase();
                let extension = if ["jpg", "jpeg", "png", "gif", "webp", "avif"]
                    .contains(&extension.as_str())
                {
                    extension
                } else {
                    "jpg".to_string()
                };
                let next_index = *image_count + 1;
                let relative = format!("../assets/image-{next_index:05}.{extension}");
                let output = destination.join(relative.trim_start_matches("../"));
                if download_binary(&url, &output).is_ok() {
                    *image_count = next_index;
                    relative
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        if replacement.is_empty() {
            result.push_str("<span class=\"missing-image\">[图片资源不可用]</span>");
        } else {
            result.push_str(prefix);
            result.push_str(&replacement);
            result.push_str(suffix);
        }
        last = matched.end();
    }
    result.push_str(&html[last..]);
    Ok(result)
}

fn download_binary(url: &Url, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let status = Command::new("/usr/bin/curl")
        .args([
            "--location",
            "--max-redirs",
            "3",
            "--connect-timeout",
            "10",
            "--max-time",
            "60",
            "--fail",
            "--silent",
            "--show-error",
            "--proto",
            "=http,https",
            "--output",
        ])
        .arg(destination)
        .arg(url.as_str())
        .status()?;
    if !status.success() {
        bail!("图片下载失败");
    }
    let size = fs::metadata(destination)?.len();
    if size == 0 || size > MAX_DOWNLOAD_BYTES {
        let _ = fs::remove_file(destination);
        bail!("图片为空或超过 64 MiB");
    }
    Ok(())
}

fn rewrite_manifest_metadata(
    candidate: &Path,
    request: &StartImportRequest,
    preflight: &ImportPreflight,
) -> Result<()> {
    let path = candidate.join("book.json");
    let mut manifest: crate::models::BookManifest = read_json(&path)?;
    manifest.title = request.title.trim().to_string();
    manifest.author = request.author.trim().to_string();
    let source_language = match preflight.language.as_str() {
        "zh-CN" => "zh-CN",
        "mixed" => "mul",
        _ => "und",
    };
    manifest.language = Some(
        if request.translate {
            "zh-CN"
        } else {
            source_language
        }
        .to_string(),
    );
    manifest.source_language = Some(source_language.to_string());
    manifest.target_language = request.translate.then(|| "zh-CN".to_string());
    manifest.original_title = request.translate.then(|| preflight.original_title.clone());
    for (chapter, selected) in manifest
        .chapters
        .iter_mut()
        .zip(request.chapters.iter().filter(|chapter| chapter.selected))
    {
        chapter.title = selected.title.trim().to_string();
    }
    write_json(&path, &manifest)
}

fn only_child_directory(root: &Path) -> Result<PathBuf> {
    let directories = fs::read_dir(root)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && !path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with('.'))
        })
        .collect::<Vec<_>>();
    if directories.len() != 1 {
        bail!("候选书籍目录数量异常");
    }
    Ok(directories[0].clone())
}

fn resumable_candidate_directory(root: &Path) -> Result<PathBuf> {
    let directories = fs::read_dir(root)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && !path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with('.') || name == "translation"
                })
        })
        .collect::<Vec<_>>();
    if directories.len() != 1 {
        bail!("候选书籍目录数量异常");
    }
    Ok(directories[0].clone())
}

fn translation_batches(blocks: &BTreeMap<String, String>) -> Vec<BTreeMap<String, String>> {
    let mut batches = Vec::new();
    let mut current = BTreeMap::new();
    let mut current_chars = 0usize;
    for (id, text) in blocks {
        let entry_chars = id.chars().count() + text.chars().count();
        if !current.is_empty()
            && (current.len() >= MAX_TRANSLATION_BATCH_BLOCKS
                || current_chars.saturating_add(entry_chars) > MAX_TRANSLATION_BATCH_CHARS)
        {
            batches.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        current.insert(id.clone(), text.clone());
        current_chars = current_chars.saturating_add(entry_chars);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

fn translation_char_count(blocks: &BTreeMap<String, String>) -> usize {
    blocks
        .iter()
        .map(|(id, text)| id.chars().count() + text.chars().count())
        .sum()
}

fn translation_char_count_for_ids<'a, I>(blocks: &BTreeMap<String, String>, ids: I) -> usize
where
    I: IntoIterator<Item = &'a String>,
{
    ids.into_iter()
        .filter_map(|id| blocks.get_key_value(id))
        .map(|(id, text)| id.chars().count() + text.chars().count())
        .sum()
}

fn collect_reusable_translations(
    workspace: &Path,
    blocks: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let mut output_paths = Vec::new();
    collect_translation_output_paths(workspace, &mut output_paths);
    let mut reusable = BTreeMap::new();
    for path in output_paths {
        let Ok(stored) = read_json::<BTreeMap<String, String>>(&path) else {
            continue;
        };
        for (id, translation) in stored {
            let Some(source) = blocks.get(&id) else {
                continue;
            };
            let single_source = BTreeMap::from([(id.clone(), source.clone())]);
            let single_translation = BTreeMap::from([(id.clone(), translation.clone())]);
            if validate_translation_map(&single_source, &single_translation).is_ok() {
                reusable.insert(id, translation);
            }
        }
    }
    Ok(reusable)
}

fn collect_translation_output_paths(root: &Path, paths: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_translation_output_paths(&path, paths);
        } else if path
            .file_name()
            .is_some_and(|name| name == "translations.json")
            && path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "output")
        {
            paths.push(path);
        }
    }
}

fn split_translation_batch(
    batch: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let target = translation_char_count(batch) / 2;
    let mut left = BTreeMap::new();
    let mut right = BTreeMap::new();
    let mut accumulated = 0usize;
    for (id, text) in batch {
        let entry_chars = id.chars().count() + text.chars().count();
        if !left.is_empty() && accumulated >= target {
            right.insert(id.clone(), text.clone());
        } else {
            accumulated = accumulated.saturating_add(entry_chars);
            left.insert(id.clone(), text.clone());
        }
    }
    (left, right)
}

fn prepare_retry_workspace(root: &Path, batch: &BTreeMap<String, String>) -> Result<()> {
    if root.exists() {
        fs::remove_dir_all(root)?;
    }
    fs::create_dir_all(root.join("input"))?;
    fs::create_dir_all(root.join("output"))?;
    write_json(&root.join("input/blocks.json"), batch)
}

fn should_split_translation_error(error: &anyhow::Error) -> bool {
    let detail = format!("{error:#}");
    [
        "Schema",
        "结构化翻译结果",
        "无效结构化",
        "结构化工具输入",
        "译文",
        "正文块",
        "超过 8 分钟",
        "超过 30 分钟",
    ]
    .iter()
    .any(|needle| detail.contains(needle))
}

fn translation_eta_ms(completed_chars: usize, total_chars: usize, elapsed_ms: u64) -> Option<u64> {
    if completed_chars == 0 || completed_chars >= total_chars || elapsed_ms == 0 {
        return None;
    }
    let remaining = total_chars.saturating_sub(completed_chars);
    Some(
        elapsed_ms.saturating_mul(u64::try_from(remaining).unwrap_or(u64::MAX))
            / u64::try_from(completed_chars).unwrap_or(1).max(1),
    )
}

fn format_duration_ms(duration_ms: u64) -> String {
    let seconds = duration_ms / 1_000;
    let minutes = seconds / 60;
    let hours = minutes / 60;
    if hours > 0 {
        format!("{hours}小时{}分钟", minutes % 60)
    } else if minutes > 0 {
        format!("{minutes}分{}秒", seconds % 60)
    } else {
        format!("{seconds}秒")
    }
}

#[allow(clippy::too_many_arguments)]
fn translation_event_context(
    state: &str,
    batch_number: usize,
    total_batches: usize,
    batch: &BTreeMap<String, String>,
    completed_blocks: usize,
    total_blocks: usize,
    completed_chars: usize,
    total_chars: usize,
    runtime_id: &str,
    session_id: Option<String>,
    elapsed_ms: u64,
    eta_ms: Option<u64>,
    attempt: usize,
) -> EventContext {
    let now = Utc::now().timestamp_millis();
    let elapsed_i64 = i64::try_from(elapsed_ms).unwrap_or(i64::MAX);
    EventContext {
        scope: Some(format!("translation.batch.{batch_number}")),
        state: Some(state.to_string()),
        progress: Some(ImportTaskEventProgress {
            completed: completed_chars,
            total: total_chars,
            unit: "chars".to_string(),
        }),
        timing: Some(ImportTaskEventTiming {
            started_at: now.saturating_sub(elapsed_i64),
            elapsed_ms,
            eta_ms,
        }),
        runtime: Some(ImportTaskEventRuntime {
            id: runtime_id.to_string(),
            model: None,
            session_id,
            pid: None,
        }),
        metrics: Some(ImportTaskEventMetrics {
            batch: batch_number,
            batches: total_batches,
            blocks: batch.len(),
            chars: translation_char_count(batch),
            completed_blocks,
            total_blocks,
            completed_chars,
            total_chars,
            attempt,
        }),
    }
}

fn extract_translation_blocks(candidate: &Path) -> Result<BTreeMap<String, String>> {
    let manifest: crate::models::BookManifest = read_json(&candidate.join("book.json"))?;
    let mut blocks = BTreeMap::new();
    if needs_chinese_translation(&manifest.title) {
        blocks.insert(
            BOOK_TITLE_TRANSLATION_ID.to_string(),
            manifest.title.clone(),
        );
    }
    for chapter in &manifest.chapters {
        if needs_chinese_translation(&chapter.title) {
            blocks.insert(
                chapter_title_translation_id(&chapter.id),
                chapter.title.clone(),
            );
        }
        let html = fs::read_to_string(candidate.join(&chapter.path))?;
        for (id, inner, tag) in chapter_blocks(&html) {
            let value = if tag.eq_ignore_ascii_case("pre") {
                visible_text(&inner)
            } else {
                protect_block_text(&inner).0
            };
            blocks.insert(id, value);
        }
    }
    Ok(blocks)
}

fn chapter_title_translation_id(chapter_id: &str) -> String {
    format!("goodreader-metadata-chapter-title:{chapter_id}")
}

fn needs_chinese_translation(value: &str) -> bool {
    !value.chars().any(|character| {
        matches!(
            character as u32,
            0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
        )
    })
}

fn chapter_blocks(html: &str) -> Vec<(String, String, String)> {
    let mut blocks = Vec::new();
    for tag in [
        "p",
        "li",
        "blockquote",
        "pre",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "td",
        "th",
    ] {
        let regex = Regex::new(&format!(r#"(?is)<{tag}\b[^>]*\bdata-goodreader-block\s*=\s*[\"']([^\"']+)[\"'][^>]*>(.*?)</{tag}\s*>"#)).expect("正文块正则固定有效");
        for captures in regex.captures_iter(html) {
            if let (Some(id), Some(inner)) = (captures.get(1), captures.get(2)) {
                blocks.push((
                    id.as_str().to_string(),
                    inner.as_str().to_string(),
                    tag.to_string(),
                ));
            }
        }
    }
    blocks
}

fn protect_block_text(inner: &str) -> (String, Vec<String>) {
    let protected = Regex::new(r"(?is)<code\b[^>]*>.*?</code\s*>|<kbd\b[^>]*>.*?</kbd\s*>|<samp\b[^>]*>.*?</samp\s*>|<img\b[^>]*>").expect("受保护正文正则固定有效");
    let mut fragments = Vec::new();
    let replaced = protected.replace_all(inner, |captures: &regex::Captures<'_>| {
        let index = fragments.len();
        fragments.push(
            captures
                .get(0)
                .map(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
        );
        format!("{{{{GR{index}}}}}")
    });
    (visible_text(&replaced), fragments)
}

fn validate_translation_map(
    source: &BTreeMap<String, String>,
    translated: &BTreeMap<String, String>,
) -> Result<()> {
    if source.keys().collect::<Vec<_>>() != translated.keys().collect::<Vec<_>>() {
        bail!("Agent 返回的正文块账本与来源不一致");
    }
    let placeholder = Regex::new(r"\{\{GR\d+\}\}").expect("占位符正则固定有效");
    for (id, source_text) in source {
        let translated_text = translated.get(id).context("译文正文块缺失")?;
        if translated_text.trim().is_empty() {
            bail!("正文块 {id} 的译文为空");
        }
        let mut source_tokens = placeholder
            .find_iter(source_text)
            .map(|value| value.as_str())
            .collect::<Vec<_>>();
        let mut translated_tokens = placeholder
            .find_iter(translated_text)
            .map(|value| value.as_str())
            .collect::<Vec<_>>();
        source_tokens.sort_unstable();
        translated_tokens.sort_unstable();
        if source_tokens != translated_tokens {
            bail!("正文块 {id} 的受保护内容占位符被修改");
        }
    }
    Ok(())
}

fn apply_translations(
    candidate: &Path,
    translations: &BTreeMap<String, String>,
    preserve_original: bool,
    source_language: &str,
) -> Result<()> {
    let manifest_path = candidate.join("book.json");
    let mut manifest: crate::models::BookManifest = read_json(&manifest_path)?;
    let original_book_title = manifest.title.clone();
    if let Some(translated) = translations.get(BOOK_TITLE_TRANSLATION_ID) {
        manifest.title = translated.trim().to_string();
    }
    if preserve_original {
        fs::create_dir_all(candidate.join("parallel"))?;
    }
    let mut translated_chapter_titles = Vec::new();
    for chapter in &mut manifest.chapters {
        let original_chapter_title = chapter.title.clone();
        if let Some(translated) = translations.get(&chapter_title_translation_id(&chapter.id)) {
            chapter.title = translated.trim().to_string();
        }
        let path = candidate.join(&chapter.path);
        let mut html = fs::read_to_string(&path)?;
        let mut originals = BTreeMap::new();
        for (id, inner, _) in chapter_blocks(&html) {
            if preserve_original {
                originals.insert(id, visible_text(&inner));
            }
        }
        for tag in [
            "p",
            "blockquote",
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
            "h6",
            "td",
            "th",
            "li",
        ] {
            let block_regex = Regex::new(&format!(r#"(?is)(<{tag}\b[^>]*\bdata-goodreader-block\s*=\s*[\"']([^\"']+)[\"'][^>]*>)(.*?)(</{tag}\s*>)"#)).expect("正文替换正则固定有效");
            html = block_regex
                .replace_all(&html, |captures: &regex::Captures<'_>| {
                    let opening = captures
                        .get(1)
                        .map(|value| value.as_str())
                        .unwrap_or_default();
                    let id = captures
                        .get(2)
                        .map(|value| value.as_str())
                        .unwrap_or_default();
                    let inner = captures
                        .get(3)
                        .map(|value| value.as_str())
                        .unwrap_or_default();
                    let closing = captures
                        .get(4)
                        .map(|value| value.as_str())
                        .unwrap_or_default();
                    if inner.contains("data-goodreader-block") {
                        return captures
                            .get(0)
                            .map(|value| value.as_str())
                            .unwrap_or_default()
                            .to_string();
                    }
                    let (_, fragments) = protect_block_text(inner);
                    let mut translated = translations
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| visible_text(inner));
                    translated = escape_html(&translated);
                    for (index, fragment) in fragments.iter().enumerate() {
                        translated = translated.replace(&format!("{{{{GR{index}}}}}"), fragment);
                    }
                    format!("{opening}{translated}{closing}")
                })
                .into_owned();
        }
        html = replace_document_title(&html, &original_chapter_title, &chapter.title);
        html = replace_untracked_h1(&html, &original_chapter_title, &chapter.title);
        fs::write(&path, html.as_bytes())?;
        translated_chapter_titles.push((
            chapter.path.clone(),
            original_chapter_title,
            chapter.title.clone(),
        ));
        if preserve_original && !originals.is_empty() {
            let relative = format!("parallel/{}.original.json", chapter.id);
            write_json(
                &candidate.join(&relative),
                &crate::models::ParallelText {
                    schema_version: 1,
                    language: source_language.to_string(),
                    blocks: originals,
                },
            )?;
            chapter.parallel_text = Some(relative);
        } else {
            chapter.parallel_text = None;
        }
    }
    let entry_path = candidate.join(&manifest.entry);
    if let Ok(mut entry) = fs::read_to_string(&entry_path) {
        entry = replace_document_title(&entry, &original_book_title, &manifest.title);
        entry = replace_untracked_h1(&entry, &original_book_title, &manifest.title);
        for (path, original, translated) in &translated_chapter_titles {
            entry = replace_plain_anchor_title(&entry, path, original, translated);
        }
        fs::write(&entry_path, entry.as_bytes())?;
    }
    write_json(&manifest_path, &manifest)
}

fn replace_document_title(html: &str, original: &str, translated: &str) -> String {
    replace_plain_element(html, "title", original, translated, false)
}

fn replace_untracked_h1(html: &str, original: &str, translated: &str) -> String {
    replace_plain_element(html, "h1", original, translated, true)
}

fn replace_plain_element(
    html: &str,
    tag: &str,
    original: &str,
    translated: &str,
    exclude_goodreader_blocks: bool,
) -> String {
    if original == translated {
        return html.to_string();
    }
    let pattern = format!(
        r#"(?is)(<{tag}\b[^>]*>\s*){}(\s*</{tag}\s*>)"#,
        regex::escape(&escape_html(original))
    );
    let Ok(regex) = Regex::new(&pattern) else {
        return html.to_string();
    };
    regex
        .replace(html, |captures: &regex::Captures<'_>| {
            if exclude_goodreader_blocks
                && captures[1]
                    .to_ascii_lowercase()
                    .contains("data-goodreader-block")
            {
                return captures[0].to_string();
            }
            format!(
                "{}{}{}",
                &captures[1],
                escape_html(translated),
                &captures[2]
            )
        })
        .into_owned()
}

fn replace_plain_anchor_title(html: &str, path: &str, original: &str, translated: &str) -> String {
    if original == translated {
        return html.to_string();
    }
    let pattern = format!(
        r#"(?is)(<a\b[^>]*\bhref\s*=\s*[\"']{}[\"'][^>]*>\s*){}(\s*</a\s*>)"#,
        regex::escape(path),
        regex::escape(&escape_html(original))
    );
    let Ok(regex) = Regex::new(&pattern) else {
        return html.to_string();
    };
    regex
        .replace(html, |captures: &regex::Captures<'_>| {
            format!(
                "{}{}{}",
                &captures[1],
                escape_html(translated),
                &captures[2]
            )
        })
        .into_owned()
}

fn quality_report(
    candidate: &Path,
    manifest: &crate::models::BookManifest,
) -> Result<ImportQualityReport> {
    let mut report = ImportQualityReport {
        chapter_count: manifest.chapters.len(),
        ..ImportQualityReport::default()
    };
    let block = Regex::new(r#"data-goodreader-block\s*=\s*[\"']([^\"']+)[\"']"#)
        .expect("正文块计数正则固定有效");
    let image = Regex::new(r#"(?is)<img\b[^>]*\bsrc\s*=\s*[\"']([^\"']+)[\"']"#)
        .expect("图片校验正则固定有效");
    for chapter in &manifest.chapters {
        let path = candidate.join(&chapter.path);
        let html = fs::read_to_string(&path)?;
        report.block_count += block.find_iter(&html).count();
        for captures in image.captures_iter(&html) {
            let Some(source) = captures.get(1).map(|value| value.as_str()) else {
                continue;
            };
            if source.starts_with("data:") {
                report.image_count += 1;
                continue;
            }
            let image_path = path.parent().unwrap_or(candidate).join(source);
            if image_path.is_file() {
                report.image_count += 1;
            } else {
                report.errors.push(format!(
                    "章节《{}》引用的图片不存在：{}",
                    chapter.title, source
                ));
            }
        }
        if let Some(parallel) = &chapter.parallel_text {
            let text: crate::models::ParallelText = read_json(&candidate.join(parallel))?;
            report.original_block_count += text.blocks.len();
        }
    }
    if manifest.target_language.as_deref() == Some("zh-CN") {
        report.translated_block_count = report.block_count;
    }
    if report.block_count == 0 {
        report.errors.push("生成书籍没有正文块".to_string());
    }
    if report.original_block_count > 0 && report.original_block_count < report.block_count {
        report.warnings.push(format!(
            "对照原文覆盖 {}/{} 个正文块",
            report.original_block_count, report.block_count
        ));
    }
    Ok(report)
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::{
        detect_language, detect_pdf_chapters, discover_chapter_links, pdf_pages_requiring_ocr,
        validate_translation_map, ImportManager,
    };
    use crate::agent::AgentCoordinator;
    use crate::db::Database;
    use crate::models::{
        ImportChapterCandidate, ImportPreflight, ImportSourceKind, PdfImportMode,
        StartImportRequest,
    };

    #[test]
    fn detects_the_main_narrative_language() {
        let chinese =
            "这是一本关于本地阅读器设计的中文书籍。它包含完整章节、正文和图片说明。".repeat(8);
        let english = "This is an English book about a local reading application with complete chapters and figures. ".repeat(8);
        assert_eq!(detect_language(&chinese).0, "zh-CN");
        assert_eq!(detect_language(&english).0, "non-zh");
    }

    #[test]
    fn permits_protected_fragments_to_follow_translated_word_order() {
        let source = BTreeMap::from([(
            "chapter-0002-block-0058".to_string(),
            "{{GR0}} is used like {{GR1}}. Instead of {{GR2}}, {{GR3}} uses one location."
                .to_string(),
        )]);
        let translated = BTreeMap::from([(
            "chapter-0002-block-0058".to_string(),
            "{{GR0}} 的用法与 {{GR1}} 相同。{{GR3}} 使用单一位置，而不是 {{GR2}}。".to_string(),
        )]);

        validate_translation_map(&source, &translated)
            .expect("受保护片段允许按照目标语言语序重新排列");
    }

    #[test]
    fn groups_large_books_into_reliable_translation_batches() {
        let blocks = (0..4_576)
            .map(|index| {
                (
                    format!("block-{index:04}"),
                    "A representative English paragraph for translation batching. ".repeat(2),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let batches = super::translation_batches(&blocks);

        assert!(batches.len() >= 50, "大书必须拆成可靠的小批次");
        assert!(batches.iter().all(|batch| batch.len() <= 80));
        assert!(batches
            .iter()
            .all(|batch| super::translation_char_count(batch) <= 12_000));
    }

    #[test]
    fn splits_repeated_structured_output_failures() {
        let error = anyhow::anyhow!("Agent 执行失败：Claude 连续 2 次返回无效结构化工具输入");
        assert!(super::should_split_translation_error(&error));
    }

    #[test]
    fn reuses_valid_blocks_from_legacy_batch_boundaries() {
        let temp = TempDir::new().expect("临时目录");
        let workspace = temp.path().join("translation");
        let blocks = (0..160)
            .map(|index| {
                (
                    format!("block-{index:04}"),
                    format!("English paragraph {index} with {{{{GR0}}}}."),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let legacy = blocks
            .iter()
            .take(120)
            .map(|(id, text)| (id.clone(), text.replace("English", "中文")))
            .collect::<BTreeMap<_, _>>();
        let legacy_output = workspace.join("batches/batch-0001/output");
        fs::create_dir_all(&legacy_output).unwrap();
        super::write_json(&legacy_output.join("translations.json"), &legacy).unwrap();

        let reusable = super::collect_reusable_translations(&workspace, &blocks).unwrap();
        assert_eq!(reusable.len(), 120);
        let pending = blocks
            .iter()
            .filter(|(id, _)| !reusable.contains_key(*id))
            .map(|(id, text)| (id.clone(), text.clone()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(super::translation_batches(&pending).len(), 1);
        assert_eq!(pending.len(), 40);
    }

    #[test]
    fn online_discovery_stays_in_the_confirmed_scope() {
        let base = url::Url::parse("https://example.com/book/index.html").unwrap();
        let html = r#"<title>示例</title><a href="chapter-1.html">第一章</a><a href="/other/page.html">越界</a><a href="https://outside.example/a">外站</a>"#;
        let candidates = discover_chapter_links(&base, html);
        assert_eq!(candidates.len(), 2);
        assert!(candidates[1].source.ends_with("/book/chapter-1.html"));
    }

    #[test]
    fn pdf_chapter_detection_rejects_truncated_table_headings() {
        let pages = vec![
            "1.3 本章小结\n第二章（上下文工\n其他表格内容".to_string(),
            "第 2 章 上下文工程\n本章正文".to_string(),
            "第 3 章 用户记忆和知识库\n本章正文".to_string(),
            "第 4 章 工具\n本章正文".to_string(),
        ];

        let chapters = detect_pdf_chapters(&pages, "示例书");
        assert!(!chapters
            .iter()
            .any(|chapter| chapter.title == "第二章（上下文工"));
        assert!(chapters
            .iter()
            .any(|chapter| chapter.title == "第 2 章 上下文工程"));
    }

    #[test]
    fn mixed_pdf_reports_each_sparse_image_page_for_ocr() {
        let pages = vec![
            "第一页有完整的数字文本内容，足以证明文本层可用。".to_string(),
            String::new(),
            "第三页也有完整的数字文本内容，不能因为扫描页占比低就忽略。".to_string(),
        ];
        let image_pages = std::collections::BTreeSet::from([2usize]);

        assert_eq!(pdf_pages_requiring_ocr(&pages, &image_pages), vec![2]);
    }

    #[test]
    fn chinese_pdf_still_requires_a_page_layout_agent() {
        let temp = TempDir::new().expect("临时目录");
        let database = Arc::new(Database::open(&temp.path().join("Data")).unwrap());
        let agent =
            Arc::new(AgentCoordinator::new(temp.path().join("AgentTasks"), database).unwrap());
        let manager = Arc::new(
            ImportManager::new(
                temp.path().join("ImportTasks"),
                temp.path().join("Books"),
                agent,
            )
            .unwrap(),
        );
        let token = uuid::Uuid::new_v4().to_string();
        let chapter = ImportChapterCandidate {
            id: "candidate-0001".to_string(),
            title: "第一章".to_string(),
            source: "pages:1-1".to_string(),
            selected: true,
        };
        let stored = super::StoredPreflight {
            preflight: ImportPreflight {
                token: token.clone(),
                kind: ImportSourceKind::Pdf,
                source_name: "示例.pdf".to_string(),
                title: "示例".to_string(),
                original_title: "示例".to_string(),
                author: "作者".to_string(),
                language: "zh-CN".to_string(),
                language_confidence: "high".to_string(),
                page_count: Some(1),
                chapter_candidates: vec![chapter.clone()],
                image_count: 0,
                character_count: 100,
                requires_ocr_pages: Vec::new(),
                uncertain_pages: Vec::new(),
                pdf_mode: Some(PdfImportMode::TextLayer),
                pdf_type: Some("digital".to_string()),
                dynamic_rendering: false,
                warnings: Vec::new(),
            },
            source_path: Some("/tmp/example.pdf".to_string()),
            source_url: None,
        };
        let workspace = temp.path().join("ImportTasks").join(&token);
        fs::create_dir_all(&workspace).unwrap();
        super::write_json(&workspace.join("preflight.json"), &stored).unwrap();

        let error = manager
            .start(StartImportRequest {
                token,
                title: "示例".to_string(),
                author: "作者".to_string(),
                chapters: vec![chapter],
                translate: false,
                preserve_original: false,
                runtime_id: None,
            })
            .unwrap_err();
        assert!(error.to_string().contains("PDF 制书必须选择一个可用 Agent"));
    }

    #[test]
    fn finds_candidate_beside_legacy_translation_workspace() {
        let temp = TempDir::new().expect("临时目录");
        let candidate = temp.path().join("candidate-book");
        fs::create_dir_all(&candidate).unwrap();
        fs::create_dir_all(temp.path().join("translation")).unwrap();
        assert_eq!(
            super::resumable_candidate_directory(temp.path()).unwrap(),
            candidate
        );
    }

    #[tokio::test]
    async fn generates_a_local_html_book_through_the_persistent_task() {
        let temp = TempDir::new().expect("临时目录");
        let source = temp.path().join("source");
        let books = temp.path().join("Books");
        fs::create_dir_all(source.join("chapters")).unwrap();
        fs::write(
            source.join("index.html"),
            r#"<!doctype html><title>任务测试书</title><meta name="author" content="测试作者"><a href="chapters/one.html">第一章</a><a href="chapters/two.html">第二章</a>"#,
        )
        .unwrap();
        fs::write(
            source.join("chapters/two.html"),
            r#"<!doctype html><title>第二章</title><body><h1>第二章</h1><p>这章在确认页中被排除。</p><img src="unused.png"></body>"#,
        )
        .unwrap();
        fs::write(source.join("chapters/unused.png"), b"unused image fixture").unwrap();
        fs::write(
            source.join("chapters/one.html"),
            r#"<!doctype html><title>第一章</title><body><h1>第一章</h1><p>这是足够完整的中文正文，用来验证持久化书籍生成任务。</p></body>"#,
        )
        .unwrap();

        let database = Arc::new(Database::open(&temp.path().join("Data")).unwrap());
        let agent =
            Arc::new(AgentCoordinator::new(temp.path().join("AgentTasks"), database).unwrap());
        let manager = Arc::new(
            ImportManager::new(temp.path().join("ImportTasks"), books.clone(), agent).unwrap(),
        );
        let preflight = manager
            .preflight_local(ImportSourceKind::Html, &source)
            .unwrap();
        let mut chapters = preflight.chapter_candidates;
        for chapter in &mut chapters {
            if chapter.source.ends_with("two.html") {
                chapter.selected = false;
            }
        }
        let task = manager
            .start(StartImportRequest {
                token: preflight.token,
                title: preflight.title,
                author: preflight.author,
                chapters,
                translate: false,
                preserve_original: false,
                runtime_id: None,
            })
            .unwrap();

        let mut completed = None;
        for _ in 0..100 {
            let current = manager.task(&task.id).unwrap();
            if matches!(current.status.as_str(), "completed" | "failed") {
                completed = Some(current);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let completed = completed.expect("任务应结束");
        assert_eq!(completed.status, "completed", "{:?}", completed.error);
        assert_eq!(fs::read_dir(&books).unwrap().count(), 1);
        assert!(completed.imported.is_some());
        let events = manager.events(&task.id).unwrap();
        assert!(events.iter().any(|event| event.kind == "script"));
        assert!(events.iter().any(|event| event.kind == "stage"));
        assert_eq!(
            events.last().map(|event| event.title.as_str()),
            Some("生成完成")
        );
        let last_seq = events.last().map(|event| event.seq).unwrap_or(0);
        manager
            .append_event(&task.id, "stage", "增量事件", "只返回新增记录")
            .unwrap();
        let incremental = manager.events_since(&task.id, last_seq).unwrap();
        assert_eq!(incremental.len(), 1);
        assert_eq!(incremental[0].title, "增量事件");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_a_second_unfinished_import_task() {
        let temp = TempDir::new().expect("临时目录");
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("index.html"),
            r#"<!doctype html><title>单任务测试</title><body><h1>第一章</h1><p>用于验证同一时间只允许一个未完成导入任务。</p></body>"#,
        )
        .unwrap();

        let database = Arc::new(Database::open(&temp.path().join("Data")).unwrap());
        let agent =
            Arc::new(AgentCoordinator::new(temp.path().join("AgentTasks"), database).unwrap());
        let manager = Arc::new(
            ImportManager::new(
                temp.path().join("ImportTasks"),
                temp.path().join("Books"),
                agent,
            )
            .unwrap(),
        );
        let first = manager
            .preflight_local(ImportSourceKind::Html, &source)
            .unwrap();
        let second = manager
            .preflight_local(ImportSourceKind::Html, &source)
            .unwrap();
        let request = |preflight: crate::models::ImportPreflight| StartImportRequest {
            token: preflight.token,
            title: preflight.title,
            author: preflight.author,
            chapters: preflight.chapter_candidates,
            translate: false,
            preserve_original: false,
            runtime_id: None,
        };

        manager.start(request(first)).unwrap();
        let error = manager.start(request(second)).unwrap_err();
        assert!(
            error.to_string().contains("已有未完成的导入任务"),
            "实际错误：{error}"
        );
    }

    #[tokio::test]
    async fn translates_with_a_custom_agent_and_preserves_block_originals() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("临时目录");
        let source = temp.path().join("source");
        let books = temp.path().join("Books");
        fs::create_dir_all(source.join("chapters")).unwrap();
        fs::write(
            source.join("index.html"),
            r#"<!doctype html><title>English Test Book</title><meta name="author" content="Test Author"><a href="chapters/one.html">Chapter One</a>"#,
        )
        .unwrap();
        fs::write(
            source.join("chapters/one.html"),
            r#"<!doctype html><title>Chapter One</title><body><h1>Chapter One</h1><p>This English paragraph verifies translated content and preserved original text in the generated book.</p></body>"#,
        )
        .unwrap();

        let calls = temp.path().join("translation-agent-calls.txt");
        let fake_agent = temp.path().join("fake-agent.sh");
        fs::write(
            &fake_agent,
            format!(
                r#"#!/bin/sh
set -eu
cat >/dev/null
calls=0
if [ -f '{}' ]; then
  calls=$(cat '{}')
fi
calls=$((calls + 1))
printf '%s' "$calls" > '{}'
if [ "$calls" -lt 3 ]; then
  echo 'Selected model is at capacity' >&2
  exit 3
fi
mkdir -p output
sed 's/English/中文/g; s/Chapter One/第一章/g; s/This/这段/g; s/paragraph/正文/g' input/blocks.json > output/translations.json
printf 'done\n'
"#,
                calls.display(),
                calls.display(),
                calls.display(),
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_agent).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake_agent, permissions).unwrap();

        let database = Arc::new(Database::open(&temp.path().join("Data")).unwrap());
        let runtime = database
            .save_custom_agent_runtime("测试翻译 Agent", fake_agent.to_str().unwrap(), &[])
            .unwrap();
        let agent =
            Arc::new(AgentCoordinator::new(temp.path().join("AgentTasks"), database).unwrap());
        let manager = Arc::new(
            ImportManager::new(temp.path().join("ImportTasks"), books.clone(), agent).unwrap(),
        );
        let preflight = manager
            .preflight_local(ImportSourceKind::Html, &source)
            .unwrap();
        assert_eq!(preflight.language, "non-zh");
        let task = manager
            .start(StartImportRequest {
                token: preflight.token,
                title: "English Test Book".to_string(),
                author: preflight.author,
                chapters: preflight.chapter_candidates,
                translate: true,
                preserve_original: true,
                runtime_id: Some(runtime.id),
            })
            .unwrap();

        let mut completed = None;
        for _ in 0..100 {
            let current = manager.task(&task.id).unwrap();
            if matches!(current.status.as_str(), "completed" | "failed") {
                completed = Some(current);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let completed = completed.expect("翻译任务应结束");
        assert_eq!(completed.status, "completed", "{:?}", completed.error);
        let candidate = super::only_child_directory(&books).unwrap();
        let package = crate::library::validate_package(&candidate).unwrap();
        assert_eq!(package.manifest.language.as_deref(), Some("zh-CN"));
        assert_eq!(package.manifest.source_language.as_deref(), Some("und"));
        assert_eq!(package.manifest.target_language.as_deref(), Some("zh-CN"));
        assert_eq!(package.manifest.title, "中文 Test Book");
        let chapter = package.manifest.chapters.first().expect("书籍章节");
        assert_eq!(chapter.title, "第一章");
        let parallel = chapter.parallel_text.as_ref().expect("保留块级原文");
        assert!(candidate.join(parallel).is_file());
        let translated = fs::read_to_string(candidate.join(&chapter.path)).unwrap();
        assert!(translated.contains("<title>第一章</title>"));
        assert!(translated.contains("中文") || translated.contains("这段"));
        let entry = fs::read_to_string(candidate.join(&package.manifest.entry)).unwrap();
        assert!(entry.contains(">第一章</a>"));
        assert!(!entry.contains(">Chapter One</a>"));
        let events = manager.events(&task.id).unwrap();
        let agent_event = events
            .iter()
            .find(|event| event.kind == "agent" && event.state.as_deref() == Some("completed"))
            .expect("应记录 Agent 返回内容");
        assert!(agent_event.detail.contains("done"));
        assert!(agent_event.seq > 0);
        assert_eq!(agent_event.state.as_deref(), Some("completed"));
        assert!(agent_event.metrics.as_ref().is_some_and(|metrics| {
            metrics.completed_blocks == metrics.total_blocks && metrics.total_chars > 0
        }));
        assert!(agent_event
            .timing
            .as_ref()
            .is_some_and(|timing| timing.elapsed_ms > 0));
        assert_eq!(fs::read_to_string(calls).unwrap(), "3");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.state.as_deref() == Some("retrying"))
                .count(),
            2,
            "两次容量不足必须自动重试，不得要求用户恢复任务"
        );
        assert_eq!(
            agent_event.metrics.as_ref().map(|metrics| metrics.attempt),
            Some(3)
        );
    }

    #[tokio::test]
    async fn resumes_large_translation_from_completed_batches() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("临时目录");
        let source = temp.path().join("source");
        let books = temp.path().join("Books");
        fs::create_dir_all(source.join("chapters")).unwrap();
        fs::write(
            source.join("index.html"),
            r#"<!doctype html><title>Large English Book</title><meta name="author" content="Test Author"><a href="chapters/one.html">Chapter One</a>"#,
        )
        .unwrap();
        let paragraphs = (1..=600)
            .map(|index| {
                format!(
                    "<p>English paragraph {index} contains enough narrative text to exercise resumable translation batches.</p>"
                )
            })
            .collect::<String>();
        fs::write(
            source.join("chapters/one.html"),
            format!(
                "<!doctype html><title>Chapter One</title><body><h1>Chapter One</h1>{paragraphs}</body>"
            ),
        )
        .unwrap();

        let calls = temp.path().join("agent-calls.txt");
        let failed_once = temp.path().join("failed-once");
        let fake_agent = temp.path().join("fake-batch-agent.sh");
        fs::write(
            &fake_agent,
            format!(
                r#"#!/bin/sh
set -eu
cat >/dev/null
count=$(grep -c '^  "' input/blocks.json)
batch=$(basename "$PWD")
printf '%s:%s\n' "$batch" "$count" >> '{}'
if [ "$count" -gt 200 ]; then
  echo 'batch too large' >&2
  exit 2
fi
if [ "$batch" = "batch-0002" ] && [ ! -f '{}' ]; then
  touch '{}'
  echo 'transient second batch failure' >&2
  exit 3
fi
mkdir -p output
cp input/blocks.json output/translations.json
printf 'translated %s blocks\n' "$count"
"#,
                calls.display(),
                failed_once.display(),
                failed_once.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_agent).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake_agent, permissions).unwrap();

        let database = Arc::new(Database::open(&temp.path().join("Data")).unwrap());
        let runtime = database
            .save_custom_agent_runtime("分批翻译 Agent", fake_agent.to_str().unwrap(), &[])
            .unwrap();
        let agent =
            Arc::new(AgentCoordinator::new(temp.path().join("AgentTasks"), database).unwrap());
        let manager = Arc::new(
            ImportManager::new(temp.path().join("ImportTasks"), books.clone(), agent).unwrap(),
        );
        let preflight = manager
            .preflight_local(ImportSourceKind::Html, &source)
            .unwrap();
        let task = manager
            .start(StartImportRequest {
                token: preflight.token,
                title: "大型翻译测试书".to_string(),
                author: preflight.author,
                chapters: preflight.chapter_candidates,
                translate: true,
                preserve_original: true,
                runtime_id: Some(runtime.id.clone()),
            })
            .unwrap();

        let mut failed = None;
        for _ in 0..160 {
            let current = manager.task(&task.id).unwrap();
            if current.status == "failed" {
                failed = Some(current);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let failed = failed.expect("第二批失败后任务应暂停");
        assert!(failed.progress > 56, "首批完成后应更新真实进度");
        let events = manager.events(&task.id).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.title.starts_with("翻译批次 1/") && event.title.ends_with("完成")
                })
                .count(),
            1,
            "首批检查点应持久化"
        );

        manager.resume(&task.id, Some(&runtime.id)).unwrap();
        let mut completed = None;
        for _ in 0..200 {
            let current = manager.task(&task.id).unwrap();
            if matches!(current.status.as_str(), "completed" | "failed") {
                completed = Some(current);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let completed = completed.expect("恢复后的任务应结束");
        assert_eq!(completed.status, "completed", "{:?}", completed.error);
        let batch_sizes = fs::read_to_string(&calls).unwrap();
        assert!(
            batch_sizes.lines().all(|value| value
                .split(':')
                .nth(1)
                .unwrap()
                .parse::<usize>()
                .unwrap()
                <= 200),
            "每次 Agent 输入都必须受批次上限约束"
        );
        let events = manager.events(&task.id).unwrap();
        assert!(events.iter().any(|event| {
            event.title == "恢复已完成译文" && event.detail.contains("已按正文块校验并复用")
        }));
        assert_eq!(fs::read_dir(&books).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn persists_the_first_completed_parallel_batch_without_waiting_for_its_sibling() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("临时目录");
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let paragraphs = (1..=100)
            .map(|index| format!("<p>English paragraph {index} for parallel progress.</p>"))
            .collect::<String>();
        fs::write(
            source.join("index.html"),
            format!("<!doctype html><title>Parallel Book</title><body>{paragraphs}</body>"),
        )
        .unwrap();
        let fake_agent = temp.path().join("parallel-agent.sh");
        fs::write(
            &fake_agent,
            r#"#!/bin/sh
set -eu
cat >/dev/null
if [ "$(basename "$PWD")" = "batch-0001" ]; then sleep 1; fi
mkdir -p output
cp input/blocks.json output/translations.json
printf 'done\n'
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_agent).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake_agent, permissions).unwrap();
        let database = Arc::new(Database::open(&temp.path().join("Data")).unwrap());
        let runtime = database
            .save_custom_agent_runtime("并发进度 Agent", fake_agent.to_str().unwrap(), &[])
            .unwrap();
        let agent =
            Arc::new(AgentCoordinator::new(temp.path().join("AgentTasks"), database).unwrap());
        let manager = Arc::new(
            ImportManager::new(
                temp.path().join("ImportTasks"),
                temp.path().join("Books"),
                agent,
            )
            .unwrap(),
        );
        let preflight = manager
            .preflight_local(ImportSourceKind::Html, &source)
            .unwrap();
        let task = manager
            .start(StartImportRequest {
                token: preflight.token,
                title: preflight.title,
                author: preflight.author,
                chapters: preflight.chapter_candidates,
                translate: true,
                preserve_original: false,
                runtime_id: Some(runtime.id),
            })
            .unwrap();

        let started = std::time::Instant::now();
        let mut saw_second_batch = false;
        while started.elapsed() < Duration::from_millis(800) {
            saw_second_batch = manager.events(&task.id).unwrap().iter().any(|event| {
                event.title.starts_with("翻译批次 2/") && event.title.ends_with("完成")
            });
            if saw_second_batch {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            saw_second_batch,
            "快速批次完成后必须立即落盘，不能等待慢批次"
        );
        assert_eq!(manager.task(&task.id).unwrap().status, "running");

        for _ in 0..100 {
            let current = manager.task(&task.id).unwrap();
            if matches!(current.status.as_str(), "completed" | "failed") {
                assert_eq!(current.status, "completed", "{:?}", current.error);
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("并发翻译任务未结束");
    }

    #[tokio::test]
    async fn exposes_agent_output_while_translation_is_running() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("临时目录");
        let source = temp.path().join("source");
        let books = temp.path().join("Books");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("index.html"),
            r#"<!doctype html><title>Live Agent Book</title><meta name="author" content="Test Author"><body><h1>Live Agent Book</h1><p>This paragraph verifies live Agent output during translation.</p></body>"#,
        )
        .unwrap();
        let fake_agent = temp.path().join("fake-live-agent.sh");
        fs::write(
            &fake_agent,
            "#!/bin/sh\nset -eu\ncat >/dev/null\nprintf '正在处理当前批次\\n'\nsleep 1\nmkdir -p output\ncp input/blocks.json output/translations.json\nprintf '完成\\n'\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_agent).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake_agent, permissions).unwrap();

        let database = Arc::new(Database::open(&temp.path().join("Data")).unwrap());
        let runtime = database
            .save_custom_agent_runtime("实时输出 Agent", fake_agent.to_str().unwrap(), &[])
            .unwrap();
        let agent =
            Arc::new(AgentCoordinator::new(temp.path().join("AgentTasks"), database).unwrap());
        let manager =
            Arc::new(ImportManager::new(temp.path().join("ImportTasks"), books, agent).unwrap());
        let preflight = manager
            .preflight_local(ImportSourceKind::Html, &source)
            .unwrap();
        let task = manager
            .start(StartImportRequest {
                token: preflight.token,
                title: "实时输出测试书".to_string(),
                author: preflight.author,
                chapters: preflight.chapter_candidates,
                translate: true,
                preserve_original: false,
                runtime_id: Some(runtime.id),
            })
            .unwrap();

        let mut saw_live_output = false;
        for _ in 0..30 {
            let events = manager.events(&task.id).unwrap();
            if events.iter().any(|event| {
                (event.title == "Agent 实时输出" || event.title.starts_with("Agent 返回"))
                    && event.detail.contains("正在处理当前批次")
            }) {
                saw_live_output = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(saw_live_output, "Agent 结束前应能查看持续输出");

        for _ in 0..100 {
            let current = manager.task(&task.id).unwrap();
            if matches!(current.status.as_str(), "completed" | "failed") {
                assert_eq!(current.status, "completed", "{:?}", current.error);
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("实时输出测试任务未结束");
    }

    #[tokio::test]
    async fn pausing_translation_terminates_the_agent_process_group() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("临时目录");
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("index.html"),
            r#"<!doctype html><title>Pause Agent Book</title><body><h1>Pause Agent Book</h1><p>This paragraph keeps the translation Agent running until GoodReader pauses it.</p></body>"#,
        )
        .unwrap();
        let fake_agent = temp.path().join("fake-slow-agent.sh");
        fs::write(
            &fake_agent,
            "#!/bin/sh\nset -eu\ncat >/dev/null\nsleep 30\nmkdir -p output\ncp input/blocks.json output/translations.json\nprintf 'done\\n'\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_agent).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake_agent, permissions).unwrap();

        let database = Arc::new(Database::open(&temp.path().join("Data")).unwrap());
        let runtime = database
            .save_custom_agent_runtime("暂停测试 Agent", fake_agent.to_str().unwrap(), &[])
            .unwrap();
        let agent =
            Arc::new(AgentCoordinator::new(temp.path().join("AgentTasks"), database).unwrap());
        let import_root = temp.path().join("ImportTasks");
        let manager = Arc::new(
            ImportManager::new(import_root.clone(), temp.path().join("Books"), agent).unwrap(),
        );
        let preflight = manager
            .preflight_local(ImportSourceKind::Html, &source)
            .unwrap();
        let task = manager
            .start(StartImportRequest {
                token: preflight.token,
                title: preflight.title,
                author: preflight.author,
                chapters: preflight.chapter_candidates,
                translate: true,
                preserve_original: false,
                runtime_id: Some(runtime.id),
            })
            .unwrap();
        let pid_path = import_root
            .join(&task.id)
            .join("translation/batches/batch-0001/logs/process.pid");
        let mut pid = None;
        for _ in 0..200 {
            pid = fs::read_to_string(&pid_path)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok());
            if pid.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let pid = pid.expect("应记录 Agent 进程标识");
        manager.pause(&task.id).unwrap();
        for _ in 0..80 {
            let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
            if !alive {
                assert_eq!(manager.task(&task.id).unwrap().status, "paused");
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("暂停任务后 Agent 进程仍然存在");
    }

    #[test]
    #[ignore = "需要未纳入版本管理的本地中文数字 PDF"]
    fn preflights_a_real_digital_pdf_without_requesting_ocr() {
        let temp = TempDir::new().expect("临时目录");
        let database = Arc::new(Database::open(&temp.path().join("Data")).unwrap());
        let agent =
            Arc::new(AgentCoordinator::new(temp.path().join("AgentTasks"), database).unwrap());
        let manager = ImportManager::new(
            temp.path().join("ImportTasks"),
            temp.path().join("Books"),
            agent,
        )
        .unwrap();
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../AI-Agents-in-Depth-zh-CN.pdf");
        let preflight = manager
            .preflight_local(ImportSourceKind::Pdf, &source)
            .unwrap();
        assert!(preflight.page_count.unwrap_or_default() > 10);
        assert_eq!(preflight.language, "zh-CN");
        assert!(preflight.requires_ocr_pages.is_empty());
        assert_eq!(preflight.chapter_candidates.len(), 11);
        assert_eq!(preflight.chapter_candidates[1].source, "pages:15-34");
        assert_eq!(preflight.chapter_candidates[10].source, "pages:274-306");
    }

    #[test]
    #[ignore = "需要未纳入版本管理的本地 RustForDummies 验收书籍"]
    fn preserves_contract_metadata_and_chapter_order_during_html_preflight() {
        let temp = TempDir::new().expect("临时目录");
        let database = Arc::new(Database::open(&temp.path().join("Data")).unwrap());
        let agent =
            Arc::new(AgentCoordinator::new(temp.path().join("AgentTasks"), database).unwrap());
        let manager = ImportManager::new(
            temp.path().join("ImportTasks"),
            temp.path().join("Books"),
            agent,
        )
        .unwrap();
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../sample-books/rust-for-dummies");
        let preflight = manager
            .preflight_local(ImportSourceKind::Html, &source)
            .unwrap();
        assert_eq!(preflight.title, "Rust 大全（中文版）");
        assert_eq!(preflight.author, "Paul McFedries");
        assert_eq!(preflight.chapter_candidates.len(), 31);
        assert_eq!(preflight.chapter_candidates[0].title, "引言");
        assert_eq!(
            preflight.chapter_candidates[0].source,
            "chapters/introduction.html"
        );
        let stored = manager.load_preflight(&preflight.token).unwrap();
        let mut chapters = preflight.chapter_candidates;
        for (index, chapter) in chapters.iter_mut().enumerate() {
            chapter.selected = index == 0;
        }
        let request = StartImportRequest {
            token: preflight.token,
            title: preflight.title,
            author: preflight.author,
            chapters,
            translate: false,
            preserve_original: false,
            runtime_id: None,
        };
        let prepared =
            super::prepare_html_source(&stored, &request, &temp.path().join("prepared")).unwrap();
        let candidates = temp.path().join("candidates");
        let imported =
            crate::importer::import_html_directory(&prepared.directory, &candidates).unwrap();
        assert_eq!(imported.chapter_count, 1);
    }

    #[test]
    #[ignore = "需要未纳入版本管理的本地中文数字 PDF"]
    fn requires_an_agent_for_a_real_digital_pdf() {
        let temp = TempDir::new().expect("临时目录");
        let database = Arc::new(Database::open(&temp.path().join("Data")).unwrap());
        let agent =
            Arc::new(AgentCoordinator::new(temp.path().join("AgentTasks"), database).unwrap());
        let manager = Arc::new(
            ImportManager::new(
                temp.path().join("ImportTasks"),
                temp.path().join("Books"),
                agent,
            )
            .unwrap(),
        );
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../AI-Agents-in-Depth-zh-CN.pdf");
        let preflight = manager
            .preflight_local(ImportSourceKind::Pdf, &source)
            .unwrap();
        let request = StartImportRequest {
            token: preflight.token,
            title: preflight.title,
            author: preflight.author,
            chapters: preflight.chapter_candidates,
            translate: false,
            preserve_original: false,
            runtime_id: None,
        };
        let error = manager.start(request).unwrap_err();
        assert!(error.to_string().contains("PDF 制书必须选择一个可用 Agent"));
    }

    #[tokio::test]
    async fn resumes_online_conversion_from_completed_chapters() {
        use std::collections::HashMap;
        use std::io::{ErrorKind, Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Mutex;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let request_counts = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let server_counts = request_counts.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server = std::thread::spawn(move || {
            while !server_stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("测试服务异常：{error}"),
                };
                stream.set_nonblocking(false).unwrap();
                let mut request = [0u8; 2048];
                let length = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..length]);
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let count = {
                    let mut counts = server_counts.lock().unwrap();
                    let count = counts.entry(path.clone()).or_insert(0);
                    *count += 1;
                    *count
                };
                let should_fail = path == "/book/ch2.html" && count == 1;
                let body = match path.as_str() {
                    "/book/ch1.html" => "<!doctype html><title>第一章</title><body><main><h1>第一章</h1><p>在线正文一。</p></main></body>",
                    "/book/ch2.html" => "<!doctype html><title>第二章</title><body><main><h1>第二章</h1><p>在线正文二。</p></main></body>",
                    _ => "not found",
                };
                let status = if should_fail {
                    "500 Internal Server Error"
                } else {
                    "200 OK"
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });

        let temp = TempDir::new().unwrap();
        let database = Arc::new(Database::open(&temp.path().join("Data")).unwrap());
        let agent =
            Arc::new(AgentCoordinator::new(temp.path().join("AgentTasks"), database).unwrap());
        let manager = Arc::new(
            ImportManager::new(
                temp.path().join("ImportTasks"),
                temp.path().join("Books"),
                agent,
            )
            .unwrap(),
        );
        let base = format!("http://{address}/book/index.html");
        let chapters = vec![
            crate::models::ImportChapterCandidate {
                id: "candidate-0001".to_string(),
                title: "第一章".to_string(),
                source: format!("http://{address}/book/ch1.html"),
                selected: true,
            },
            crate::models::ImportChapterCandidate {
                id: "candidate-0002".to_string(),
                title: "第二章".to_string(),
                source: format!("http://{address}/book/ch2.html"),
                selected: true,
            },
        ];
        let token = uuid::Uuid::new_v4().to_string();
        let preflight = super::StoredPreflight {
            preflight: crate::models::ImportPreflight {
                token: token.clone(),
                kind: ImportSourceKind::Url,
                source_name: base.clone(),
                title: "可恢复在线测试书".to_string(),
                original_title: "可恢复在线测试书".to_string(),
                author: "测试作者".to_string(),
                language: "zh-CN".to_string(),
                language_confidence: "high".to_string(),
                page_count: None,
                chapter_candidates: chapters.clone(),
                image_count: 0,
                character_count: 20,
                requires_ocr_pages: Vec::new(),
                uncertain_pages: Vec::new(),
                pdf_mode: None,
                pdf_type: None,
                dynamic_rendering: false,
                warnings: Vec::new(),
            },
            source_path: None,
            source_url: Some(base),
        };
        super::write_json(
            &manager.root.join(&token).join("preflight.json"),
            &preflight,
        )
        .unwrap();
        let task = manager
            .start(StartImportRequest {
                token,
                title: preflight.preflight.title,
                author: preflight.preflight.author,
                chapters,
                translate: false,
                preserve_original: false,
                runtime_id: None,
            })
            .unwrap();

        let mut failed = None;
        for _ in 0..160 {
            let current = manager.task(&task.id).unwrap();
            if current.status == "failed" {
                failed = Some(current);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let failed = failed.expect("第二章首次下载应让任务暂停");
        assert!(failed.progress > 18, "在线章节完成后应更新真实进度");
        assert!(
            manager
                .events(&task.id)
                .unwrap()
                .iter()
                .any(|event| event.title == "在线章节 1/2 完成"),
            "完成的在线章节应立即形成可见检查点"
        );
        assert_eq!(
            request_counts
                .lock()
                .unwrap()
                .get("/book/ch1.html")
                .copied(),
            Some(1)
        );

        manager.resume(&task.id, None).unwrap();
        let mut completed = None;
        for _ in 0..200 {
            let current = manager.task(&task.id).unwrap();
            if matches!(current.status.as_str(), "completed" | "failed") {
                completed = Some(current);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        let completed = completed.expect("恢复后的在线任务应结束");
        assert_eq!(completed.status, "completed", "{:?}", completed.error);
        let counts = request_counts.lock().unwrap();
        assert_eq!(counts.get("/book/ch1.html").copied(), Some(1));
        assert_eq!(counts.get("/book/ch2.html").copied(), Some(2));
    }

    #[test]
    fn converts_confirmed_online_pages_and_localizes_images() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 2048];
                let length = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..length]);
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let (content_type, body) = match path {
                    "/book/ch1.html" => ("text/html; charset=utf-8", "<!doctype html><title>第一章</title><body><main><h1>第一章</h1><p>在线正文一。</p><img src=\"figure.png\" alt=\"示意图\"></main></body>".as_bytes().to_vec()),
                    "/book/ch2.html" => ("text/html; charset=utf-8", "<!doctype html><title>第二章</title><body><main><h1>第二章</h1><p>在线正文二。</p></main></body>".as_bytes().to_vec()),
                    "/book/figure.png" => ("image/png", include_bytes!("../assets/default-book-cover.png").to_vec()),
                    _ => ("text/plain", b"not found".to_vec()),
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });

        let base = format!("http://{address}/book/index.html");
        let stored = super::StoredPreflight {
            preflight: crate::models::ImportPreflight {
                token: uuid::Uuid::new_v4().to_string(),
                kind: ImportSourceKind::Url,
                source_name: base.clone(),
                title: "在线测试书".to_string(),
                original_title: "在线测试书".to_string(),
                author: "测试作者".to_string(),
                language: "zh-CN".to_string(),
                language_confidence: "high".to_string(),
                page_count: None,
                chapter_candidates: vec![
                    crate::models::ImportChapterCandidate {
                        id: "candidate-0001".to_string(),
                        title: "第一章".to_string(),
                        source: format!("http://{address}/book/ch1.html"),
                        selected: true,
                    },
                    crate::models::ImportChapterCandidate {
                        id: "candidate-0002".to_string(),
                        title: "第二章".to_string(),
                        source: format!("http://{address}/book/ch2.html"),
                        selected: true,
                    },
                ],
                image_count: 1,
                character_count: 20,
                requires_ocr_pages: Vec::new(),
                uncertain_pages: Vec::new(),
                pdf_mode: None,
                pdf_type: None,
                dynamic_rendering: false,
                warnings: Vec::new(),
            },
            source_path: None,
            source_url: Some(base),
        };
        let request = StartImportRequest {
            token: stored.preflight.token.clone(),
            title: stored.preflight.title.clone(),
            author: stored.preflight.author.clone(),
            chapters: stored.preflight.chapter_candidates.clone(),
            translate: false,
            preserve_original: false,
            runtime_id: None,
        };
        let temp = TempDir::new().unwrap();
        let prepared =
            super::prepare_url_source(&stored, &request, &temp.path().join("prepared")).unwrap();
        server.join().unwrap();
        assert_eq!(prepared.image_count, 1);
        let books = temp.path().join("Books");
        let imported = crate::importer::import_html_directory(&prepared.directory, &books).unwrap();
        assert_eq!(imported.chapter_count, 2);
        let candidate = super::only_child_directory(&books).unwrap();
        crate::library::validate_package(&candidate).unwrap();
    }
}

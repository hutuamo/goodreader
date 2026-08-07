use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BookManifest {
    pub schema_version: u32,
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub original_title: Option<String>,
    pub author: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub source_language: Option<String>,
    #[serde(default)]
    pub target_language: Option<String>,
    pub cover: String,
    pub entry: String,
    pub chapters: Vec<ChapterManifest>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChapterManifest {
    pub id: String,
    pub title: String,
    pub path: String,
    #[serde(default)]
    pub parallel_text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BookPackage {
    pub root: PathBuf,
    pub manifest: BookManifest,
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub books: BTreeMap<String, BookPackage>,
    pub issues: Vec<ImportIssue>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImportIssue {
    pub path: String,
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookSummary {
    pub id: String,
    pub title: String,
    pub original_title: Option<String>,
    pub author: String,
    pub language: Option<String>,
    pub cover_url: String,
    pub entry_url: String,
    pub chapters: Vec<ChapterSummary>,
    pub progress: Option<Progress>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChapterSummary {
    pub id: String,
    pub title: String,
    pub url: String,
    pub has_parallel_text: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Progress {
    pub book_id: String,
    pub chapter_id: String,
    pub block_id: Option<String>,
    pub chapter_progress: f64,
    pub overall_progress: f64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SaveProgress {
    pub chapter_id: String,
    pub block_id: Option<String>,
    pub chapter_progress: f64,
    pub overall_progress: f64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AnnotationKind {
    Highlight,
    Note,
    Bookmark,
}

impl AnnotationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Highlight => "highlight",
            Self::Note => "note",
            Self::Bookmark => "bookmark",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "highlight" => Some(Self::Highlight),
            "note" => Some(Self::Note),
            "bookmark" => Some(Self::Bookmark),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Annotation {
    pub id: String,
    pub book_id: String,
    pub chapter_id: String,
    pub block_id: String,
    pub start_offset: u32,
    pub end_offset: u32,
    pub quote: String,
    pub kind: AnnotationKind,
    pub color: Option<String>,
    pub note: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateAnnotation {
    pub chapter_id: String,
    pub block_id: String,
    pub start_offset: u32,
    pub end_offset: u32,
    pub quote: String,
    pub kind: AnnotationKind,
    pub color: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateAnnotation {
    pub note: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ParallelText {
    pub schema_version: u32,
    pub language: String,
    pub blocks: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bootstrap {
    pub books: Vec<BookSummary>,
    pub issues: Vec<ImportIssue>,
    pub library_path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplaceCoverResponse {
    pub changed: bool,
    pub bootstrap: Bootstrap,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedBookSummary {
    pub id: String,
    pub title: String,
    pub chapter_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportBookResponse {
    pub cancelled: bool,
    pub imported: Option<ImportedBookSummary>,
    pub bootstrap: Bootstrap,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImportSourceKind {
    Html,
    Pdf,
    Url,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PdfImportMode {
    #[default]
    Auto,
    TextLayer,
    Ocr,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportPreflightRequest {
    pub kind: ImportSourceKind,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub pdf_mode: PdfImportMode,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreflight {
    pub token: String,
    pub kind: ImportSourceKind,
    pub source_name: String,
    pub title: String,
    pub original_title: String,
    pub author: String,
    pub language: String,
    pub language_confidence: String,
    pub page_count: Option<usize>,
    pub chapter_candidates: Vec<ImportChapterCandidate>,
    pub image_count: usize,
    pub character_count: usize,
    pub requires_ocr_pages: Vec<usize>,
    #[serde(default)]
    pub uncertain_pages: Vec<usize>,
    #[serde(default)]
    pub pdf_mode: Option<PdfImportMode>,
    #[serde(default)]
    pub pdf_type: Option<String>,
    pub dynamic_rendering: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImportChapterCandidate {
    pub id: String,
    pub title: String,
    pub source: String,
    pub selected: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartImportRequest {
    pub token: String,
    pub title: String,
    pub author: String,
    pub chapters: Vec<ImportChapterCandidate>,
    pub translate: bool,
    pub preserve_original: bool,
    #[serde(default)]
    pub runtime_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResumeImportRequest {
    #[serde(default)]
    pub runtime_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MoveImportTaskRequest {
    pub direction: i8,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportTaskSummary {
    pub id: String,
    pub status: String,
    pub stage: String,
    pub progress: u8,
    pub title: String,
    #[serde(default)]
    pub uses_agent: bool,
    #[serde(default)]
    pub queue_order: i64,
    pub detail: String,
    pub error: Option<String>,
    pub imported: Option<ImportedBookSummary>,
    pub quality: Option<ImportQualityReport>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportTaskEvent {
    pub id: String,
    #[serde(default)]
    pub seq: u64,
    pub kind: String,
    pub title: String,
    pub detail: String,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<ImportTaskEventProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<ImportTaskEventTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<ImportTaskEventRuntime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<ImportTaskEventMetrics>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportTaskEventProgress {
    pub completed: usize,
    pub total: usize,
    pub unit: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportTaskEventTiming {
    pub started_at: i64,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportTaskEventRuntime {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportTaskEventMetrics {
    pub batch: usize,
    pub batches: usize,
    pub blocks: usize,
    pub chars: usize,
    pub completed_blocks: usize,
    pub total_blocks: usize,
    pub completed_chars: usize,
    pub total_chars: usize,
    pub attempt: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ImportQualityReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub chapter_count: usize,
    pub block_count: usize,
    pub image_count: usize,
    pub translated_block_count: usize,
    pub original_block_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupInfo {
    pub name: String,
    pub created_at: i64,
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveSetting {
    pub value: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentRuntime {
    pub id: String,
    pub name: String,
    pub executable: Option<String>,
    pub available: bool,
    pub version: Option<String>,
    pub detail: Option<String>,
    pub built_in: bool,
    pub capabilities: AgentRuntimeCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentRuntimeCapabilities {
    pub streaming: bool,
    pub native_resume: bool,
    pub structured_output: bool,
    pub permission_mapping: bool,
    pub tool_use: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateCustomAgentRuntime {
    pub name: String,
    pub executable: String,
    #[serde(default)]
    pub arguments: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AiMessage {
    pub id: String,
    pub book_id: String,
    pub task_id: String,
    pub role: String,
    pub content: String,
    pub runtime_id: Option<String>,
    pub created_at: i64,
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentTask {
    pub id: String,
    pub book_id: String,
    pub kind: String,
    pub status: String,
    pub goal: String,
    pub current_runtime_id: String,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub phase: Option<String>,
    pub partial_output: Option<String>,
    pub stream_sequence: Option<u64>,
    pub execution_id: Option<String>,
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub book_id: String,
    pub runtime_id: String,
    pub provider_session_id: String,
    pub provider_state_json: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateQuestion {
    pub content: String,
    pub runtime_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SwitchAgentRuntime {
    pub runtime_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookAiWorkspace {
    pub book_id: String,
    pub messages: Vec<AiMessage>,
    pub active_tasks: Vec<AgentTask>,
}

#[derive(Debug, Clone)]
pub struct CustomAgentRuntime {
    pub id: String,
    pub name: String,
    pub executable: String,
    pub arguments: Vec<String>,
}

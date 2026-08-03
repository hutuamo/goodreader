use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use uuid::Uuid;

use crate::models::{BookManifest, BookPackage, Catalog, ImportIssue, ParallelText};

const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_HTML_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PARALLEL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;

pub fn scan_books(books_dir: &Path) -> Catalog {
    if let Err(error) = fs::create_dir_all(books_dir) {
        return Catalog {
            issues: vec![ImportIssue {
                path: books_dir.display().to_string(),
                title: "无法创建书库目录".to_string(),
                detail: error.to_string(),
            }],
            ..Catalog::default()
        };
    }

    let mut candidates = Vec::new();
    let entries = match fs::read_dir(books_dir) {
        Ok(entries) => entries,
        Err(error) => {
            return Catalog {
                issues: vec![ImportIssue {
                    path: books_dir.display().to_string(),
                    title: "无法读取书库目录".to_string(),
                    detail: error.to_string(),
                }],
                ..Catalog::default()
            };
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') || !path.is_dir() {
            continue;
        }
        candidates.push(path);
    }
    candidates.sort();

    let mut valid = Vec::new();
    let mut issues = Vec::new();
    for path in candidates {
        match validate_package(&path) {
            Ok(package) => valid.push(package),
            Err(error) => issues.push(ImportIssue {
                path: path.display().to_string(),
                title: "书籍包无效".to_string(),
                detail: format!("{error:#}"),
            }),
        }
    }

    let mut by_id: HashMap<String, Vec<BookPackage>> = HashMap::new();
    for package in valid {
        by_id
            .entry(package.manifest.id.clone())
            .or_default()
            .push(package);
    }

    let mut books = BTreeMap::new();
    for (id, packages) in by_id {
        if packages.len() == 1 {
            let package = packages.into_iter().next().expect("长度已检查");
            books.insert(id, package);
            continue;
        }

        let paths = packages
            .iter()
            .map(|package| package.root.display().to_string())
            .collect::<Vec<_>>()
            .join("；");
        for package in packages {
            issues.push(ImportIssue {
                path: package.root.display().to_string(),
                title: format!("导入冲突：{}", package.manifest.title),
                detail: format!("书籍标识 {id} 同时出现在：{paths}"),
            });
        }
    }
    issues.sort_by(|left, right| left.path.cmp(&right.path));

    Catalog { books, issues }
}

pub fn validate_package(root: &Path) -> Result<BookPackage> {
    let root = root
        .canonicalize()
        .with_context(|| format!("无法解析书籍目录 {}", root.display()))?;
    validate_package_tree(&root)?;
    let manifest_path = root.join("book.json");
    ensure_regular_file(&manifest_path, MAX_MANIFEST_BYTES)?;

    let manifest_text = fs::read_to_string(&manifest_path)
        .with_context(|| format!("无法读取 {}", manifest_path.display()))?;
    let manifest: BookManifest =
        serde_json::from_str(&manifest_text).context("book.json 不是合法的 V1 书籍清单")?;

    if manifest.schema_version != 1 {
        bail!(
            "不支持 schemaVersion={}，V1 只接受 1",
            manifest.schema_version
        );
    }
    Uuid::parse_str(&manifest.id).context("id 必须是合法 UUID")?;
    require_text("title", &manifest.title)?;
    if let Some(original_title) = &manifest.original_title {
        require_text("originalTitle", original_title)?;
    }
    require_text("author", &manifest.author)?;
    for (field, language) in [
        ("language", manifest.language.as_deref()),
        ("sourceLanguage", manifest.source_language.as_deref()),
        ("targetLanguage", manifest.target_language.as_deref()),
    ] {
        if let Some(language) = language {
            require_language_code(field, language)?;
        }
    }
    if manifest.chapters.is_empty() {
        bail!("chapters 不能为空");
    }

    let cover = resolve_package_file(&root, &manifest.cover)?;
    ensure_regular_file(&cover, MAX_HTML_BYTES)?;
    let entry = resolve_package_file(&root, &manifest.entry)?;
    ensure_regular_file(&entry, MAX_HTML_BYTES)?;
    let entry_html = fs::read_to_string(&entry).context("entry 必须是 UTF-8 HTML")?;
    validate_passive_html(&entry_html, &manifest.entry)?;

    let mut chapter_ids = HashSet::new();
    let mut block_ids = HashSet::new();
    for chapter in &manifest.chapters {
        require_text("chapter.id", &chapter.id)?;
        require_safe_id("chapter.id", &chapter.id)?;
        require_text("chapter.title", &chapter.title)?;
        if !chapter_ids.insert(chapter.id.clone()) {
            bail!("章节 ID 重复：{}", chapter.id);
        }

        let chapter_path = resolve_package_file(&root, &chapter.path)?;
        ensure_regular_file(&chapter_path, MAX_HTML_BYTES)?;
        let html = fs::read_to_string(&chapter_path)
            .with_context(|| format!("章节 {} 必须是 UTF-8 HTML", chapter.id))?;
        let chapter_blocks = validate_chapter_html(&html, &chapter.id, &chapter.path)?;
        for block_id in &chapter_blocks {
            if !block_ids.insert(block_id.clone()) {
                bail!("正文块 ID 在全书内重复：{block_id}");
            }
        }

        if let Some(parallel_path) = &chapter.parallel_text {
            let path = resolve_package_file(&root, parallel_path)?;
            ensure_regular_file(&path, MAX_PARALLEL_BYTES)?;
            let text = fs::read_to_string(&path)
                .with_context(|| format!("对照文本 {parallel_path} 必须是 UTF-8 JSON"))?;
            let parallel: ParallelText =
                serde_json::from_str(&text).context("对照文本 JSON 格式无效")?;
            if parallel.schema_version != 1 {
                bail!("对照文本 schemaVersion 必须为 1");
            }
            require_text("parallel.language", &parallel.language)?;
            for block_id in parallel.blocks.keys() {
                if !chapter_blocks.contains(block_id) {
                    bail!("对照文本引用了本章不存在的正文块：{block_id}");
                }
            }
        }
    }

    Ok(BookPackage { root, manifest })
}

pub fn resolve_package_file(root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.trim().is_empty() {
        bail!("包内路径不能为空");
    }
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("包内路径不能越过书籍目录：{relative}");
    }

    let candidate = root.join(relative_path);
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("包内文件不存在：{relative}"))?;
    if !canonical.starts_with(root) {
        bail!("包内路径通过符号链接越过书籍目录：{relative}");
    }
    Ok(canonical)
}

fn validate_chapter_html(html: &str, chapter_id: &str, path: &str) -> Result<HashSet<String>> {
    validate_passive_html(html, path)?;
    if !html.contains("data-goodreader-content") {
        bail!("章节 {chapter_id} 缺少 data-goodreader-content");
    }

    let chapter_pattern = format!(
        r#"data-goodreader-chapter\s*=\s*["']{}["']"#,
        regex::escape(chapter_id)
    );
    if !Regex::new(&chapter_pattern)?.is_match(html) {
        bail!("章节页面声明的 data-goodreader-chapter 与 {chapter_id} 不一致");
    }

    let block_regex = block_id_regex();
    let blocks = block_regex
        .captures_iter(html)
        .filter_map(|capture| {
            capture
                .get(1)
                .map(|value| value.as_str().trim().to_string())
        })
        .collect::<HashSet<_>>();
    if blocks.is_empty() {
        bail!("章节 {chapter_id} 没有 data-goodreader-block");
    }
    if blocks.iter().any(|block| block.is_empty()) {
        bail!("章节 {chapter_id} 存在空正文块 ID");
    }
    for block in &blocks {
        require_safe_id("data-goodreader-block", block)?;
    }
    Ok(blocks)
}

fn validate_package_tree(root: &Path) -> Result<()> {
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("无法读取书籍目录 {}", directory.display()))?
        {
            let entry = entry.context("无法读取书籍包目录项")?;
            let file_type = entry.file_type().context("无法读取书籍包文件类型")?;
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap_or(&path);

            if file_type.is_symlink() {
                bail!("书籍包不得包含符号链接：{}", relative.display());
            }
            if file_type.is_dir() {
                directories.push(path);
                continue;
            }
            if !file_type.is_file() {
                bail!("书籍包包含不支持的文件类型：{}", relative.display());
            }

            ensure_regular_file(&path, MAX_ASSET_BYTES)?;
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            match extension.as_str() {
                "html" | "htm" => {
                    let html = fs::read_to_string(&path)
                        .with_context(|| format!("HTML 必须使用 UTF-8：{}", relative.display()))?;
                    validate_passive_html(&html, &relative.display().to_string())?;
                }
                "css" => {
                    let css = fs::read_to_string(&path)
                        .with_context(|| format!("CSS 必须使用 UTF-8：{}", relative.display()))?;
                    validate_passive_css(&css, &relative.display().to_string())?;
                }
                "json" | "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "ico" | "woff"
                | "woff2" | "ttf" | "otf" => {}
                "js" | "mjs" | "cjs" | "wasm" => {
                    bail!("书籍包不得包含可执行脚本：{}", relative.display());
                }
                "svg" => bail!("书籍包不得包含可执行 SVG：{}", relative.display()),
                _ => bail!("书籍包包含 V1 不支持的文件：{}", relative.display()),
            }
        }
    }
    Ok(())
}

fn validate_passive_html(html: &str, path: &str) -> Result<()> {
    let lower = html.to_ascii_lowercase();
    let forbidden = [
        ("<script", "脚本标签"),
        ("<iframe", "嵌入页面"),
        ("<object", "object 主动内容"),
        ("<embed", "embed 主动内容"),
        ("<form", "表单"),
        ("<svg", "内联 SVG"),
    ];
    for (needle, label) in forbidden {
        if lower.contains(needle) {
            bail!("{path} 包含不允许的{label}");
        }
    }
    if inline_event_regex().is_match(html) {
        bail!("{path} 包含不允许的行内事件");
    }
    if javascript_url_regex().is_match(html) {
        bail!("{path} 包含不允许的 javascript URL");
    }
    if meta_refresh_regex().is_match(html) {
        bail!("{path} 包含不允许的页面自动刷新");
    }
    if external_resource_regex().is_match(html) {
        bail!("{path} 包含未经允许的外部网络资源");
    }
    if lower.contains(".svg\"") || lower.contains(".svg'") {
        bail!("{path} 引用了 V1 不允许的 SVG 资源");
    }
    Ok(())
}

fn validate_passive_css(css: &str, path: &str) -> Result<()> {
    let lower = css.to_ascii_lowercase();
    if lower.contains("@import") {
        bail!("{path} 包含 V1 不允许的 CSS @import");
    }
    if lower.contains("expression(") || lower.contains("javascript:") {
        bail!("{path} 包含可执行 CSS");
    }
    if external_css_url_regex().is_match(css) {
        bail!("{path} 包含未经允许的外部网络资源");
    }
    Ok(())
}

fn ensure_regular_file(path: &Path, max_bytes: u64) -> Result<()> {
    let metadata = fs::metadata(path).with_context(|| format!("缺少文件 {}", path.display()))?;
    if !metadata.is_file() {
        bail!("不是普通文件：{}", path.display());
    }
    if metadata.len() > max_bytes {
        bail!("文件过大：{}", path.display());
    }
    Ok(())
}

fn require_text(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(anyhow!("{field} 不能为空"))
    } else {
        Ok(())
    }
}

fn require_safe_id(field: &str, value: &str) -> Result<()> {
    if safe_id_regex().is_match(value) {
        Ok(())
    } else {
        bail!("{field} 只能使用 1 至 128 个英文字母、数字、点、下划线、冒号或连字符，且首字符必须是字母或数字")
    }
}

fn require_language_code(field: &str, value: &str) -> Result<()> {
    let pattern = Regex::new(r"^[A-Za-z]{2,8}(?:-[A-Za-z0-9]{1,8})*$")?;
    if pattern.is_match(value) {
        Ok(())
    } else {
        bail!("{field} 必须使用标准语言代码")
    }
}

fn safe_id_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$").expect("ID 正则固定有效")
    })
}

fn block_id_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"data-goodreader-block\s*=\s*["']([^"']+)["']"#).expect("正文块正则固定有效")
    })
}

fn inline_event_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"(?is)<[a-z][^>]*\son[a-z]+\s*="#).expect("行内事件正则固定有效")
    })
}

fn external_resource_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r#"(?is)(?:<[a-z][^>]*\b(?:src|poster|action)\s*=\s*["']\s*https?://|<link\b[^>]*\bhref\s*=\s*["']\s*https?://)"#,
        )
        .expect("外部资源正则固定有效")
    })
}

fn javascript_url_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"(?is)<[a-z][^>]*\b(?:href|src|action)\s*=\s*["']\s*javascript:"#)
            .expect("javascript URL 正则固定有效")
    })
}

fn meta_refresh_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"(?is)<meta\b[^>]*\bhttp-equiv\s*=\s*["']\s*refresh\s*["']"#)
            .expect("自动刷新正则固定有效")
    })
}

fn external_css_url_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"(?i)url\(\s*["']?\s*(?:https?:|//)"#).expect("CSS 外部资源正则固定有效")
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{scan_books, validate_package};

    fn write_book(root: &std::path::Path, id: &str, script: bool) {
        fs::create_dir_all(root.join("chapters")).expect("创建测试目录");
        fs::write(root.join("cover.jpg"), b"cover").expect("写封面");
        let script_tag = if script {
            "<script>alert(1)</script>"
        } else {
            ""
        };
        fs::write(
            root.join("index.html"),
            format!("<!doctype html><title>首页</title>{script_tag}"),
        )
        .expect("写入口");
        fs::write(
            root.join("chapters/ch1.html"),
            r#"<!doctype html><main data-goodreader-content data-goodreader-chapter="ch1"><p data-goodreader-block="ch1-p1">正文</p></main>"#,
        )
        .expect("写章节");
        fs::write(
            root.join("book.json"),
            format!(
                r#"{{
                  "schemaVersion": 1,
                  "id": "{id}",
                  "title": "测试书",
                  "author": "作者",
                  "cover": "cover.jpg",
                  "entry": "index.html",
                  "chapters": [{{"id":"ch1","title":"第一章","path":"chapters/ch1.html"}}]
                }}"#
            ),
        )
        .expect("写清单");
    }

    #[test]
    fn accepts_a_valid_contract_book() {
        let temp = TempDir::new().expect("临时目录");
        let root = temp.path().join("book");
        write_book(&root, "d2e8d4de-4e2d-4fa0-9a9d-61f9774dced2", false);

        let book = validate_package(&root).expect("合法书籍应通过");
        assert_eq!(book.manifest.title, "测试书");
    }

    #[test]
    fn rejects_book_owned_scripts() {
        let temp = TempDir::new().expect("临时目录");
        let root = temp.path().join("book");
        write_book(&root, "10ab5f08-fce8-42b5-a314-f65e14f16f0e", true);

        let error = validate_package(&root).expect_err("脚本必须被拒绝");
        assert!(format!("{error:#}").contains("脚本标签"));
    }

    #[test]
    fn rejects_executable_files_even_when_unreferenced() {
        let temp = TempDir::new().expect("临时目录");
        let root = temp.path().join("book");
        write_book(&root, "b3e8eaf8-d811-42bc-a195-2b6c2ee84ae3", false);
        fs::write(root.join("unused.js"), "alert(1)").expect("写脚本");

        let error = validate_package(&root).expect_err("未引用脚本也必须被拒绝");
        assert!(format!("{error:#}").contains("可执行脚本"));
    }

    #[test]
    fn permits_code_examples_but_rejects_real_inline_events() {
        let temp = TempDir::new().expect("临时目录");
        let root = temp.path().join("book");
        write_book(&root, "874b6843-327d-453d-b6ae-d96c19b240f8", false);
        let chapter = root.join("chapters/ch1.html");
        fs::write(
            &chapter,
            r#"<!doctype html><main data-goodreader-content data-goodreader-chapter="ch1"><p data-goodreader-block="ch1-p1"><code>&lt;button onclick="alert(1)"&gt;</code></p></main>"#,
        )
        .expect("写代码示例");
        validate_package(&root).expect("转义后的代码文字不是可执行事件");

        fs::write(
            &chapter,
            r#"<!doctype html><main data-goodreader-content data-goodreader-chapter="ch1"><p data-goodreader-block="ch1-p1" onclick="alert(1)">正文</p></main>"#,
        )
        .expect("写真正事件");
        let error = validate_package(&root).expect_err("真实行内事件必须被拒绝");
        assert!(format!("{error:#}").contains("行内事件"));
    }

    #[test]
    fn rejects_unsafe_chapter_and_block_ids() {
        let temp = TempDir::new().expect("临时目录");
        let root = temp.path().join("book");
        write_book(&root, "075de22b-fea0-494b-8820-08f9f706c90e", false);
        let manifest_path = root.join("book.json");
        let manifest = fs::read_to_string(&manifest_path)
            .expect("读取清单")
            .replace("\"id\":\"ch1\"", "\"id\":\"ch1\\\" onclick=\\\"alert(1)\"");
        fs::write(&manifest_path, manifest).expect("写清单");

        let error = validate_package(&root).expect_err("不安全 ID 必须被拒绝");
        assert!(format!("{error:#}").contains("chapter.id"));
    }

    #[test]
    fn excludes_every_duplicate_uuid() {
        let temp = TempDir::new().expect("临时目录");
        let id = "e8f0babc-24b9-4f50-89ad-561d3fe6ca56";
        write_book(&temp.path().join("one"), id, false);
        write_book(&temp.path().join("two"), id, false);

        let catalog = scan_books(temp.path());
        assert!(catalog.books.is_empty());
        assert_eq!(catalog.issues.len(), 2);
        assert!(catalog
            .issues
            .iter()
            .all(|issue| issue.title.contains("导入冲突")));
    }

    #[test]
    #[ignore = "需要未纳入版本管理的本地 RustForDummies 验收书籍"]
    fn validates_the_converted_rust_acceptance_book() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../sample-books/rust-for-dummies");
        let book = validate_package(&root).expect("转换后的 RustForDummies 必须满足接入契约");
        assert_eq!(book.manifest.chapters.len(), 31);
        assert_eq!(book.manifest.id, "b093f472-df81-41aa-9903-305070ca8054");
    }
}

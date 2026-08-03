use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use regex::{Captures, Regex};
use uuid::Uuid;

use crate::library::validate_package;
use crate::models::{BookManifest, ChapterManifest, ParallelText};

const DEFAULT_COVER: &[u8] = include_bytes!("../assets/default-book-cover.png");
const MAX_SOURCE_FILES: usize = 20_000;
const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_SINGLE_FILE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ImportedBook {
    pub id: String,
    pub title: String,
    pub chapter_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
struct SourceFile {
    absolute: PathBuf,
    relative: PathBuf,
    size: u64,
}

#[derive(Debug, Default)]
struct ConversionStats {
    converted_parallel_texts: usize,
    skipped_scripts: usize,
    skipped_unsupported: usize,
    sanitized_html: usize,
    sanitized_css: usize,
    used_default_cover: bool,
}

pub fn import_html_directory(source: &Path, books_dir: &Path) -> Result<ImportedBook> {
    let source = source
        .canonicalize()
        .with_context(|| format!("无法读取导入目录 {}", source.display()))?;
    if !source.is_dir() {
        bail!("请选择一个 HTML 书籍目录");
    }

    fs::create_dir_all(books_dir)
        .with_context(|| format!("无法创建书库目录 {}", books_dir.display()))?;
    let books_dir = books_dir
        .canonicalize()
        .with_context(|| format!("无法解析书库目录 {}", books_dir.display()))?;
    if source.starts_with(&books_dir) || books_dir.starts_with(&source) {
        bail!("导入目录不能位于 GoodReader 书库中，也不能包含整个书库");
    }

    let files = collect_source_files(&source)?;
    let mut html_files = files
        .iter()
        .filter(|file| is_html(&file.relative))
        .map(|file| file.relative.clone())
        .collect::<Vec<_>>();
    if html_files.is_empty() {
        bail!("所选目录中没有 HTML 文件");
    }
    html_files.sort_by(|left, right| natural_path_cmp(left, right));

    let entry = choose_entry(&html_files);
    let chapter_sources = choose_chapters(&html_files, &entry);
    let id = Uuid::new_v4().to_string();
    let short_id = &id[..8];
    let folder_slug = destination_slug(
        source
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("book"),
    );
    let final_root = books_dir.join(format!("{folder_slug}-{short_id}"));
    let staging_root = books_dir.join(format!(".importing-{id}"));

    fs::create_dir(&staging_root).context("无法创建导入暂存目录")?;
    let result = convert_into_staging(
        &source,
        &files,
        &entry,
        &chapter_sources,
        &id,
        &staging_root,
    );

    match result.and_then(|imported| {
        validate_package(&staging_root).context("转换结果未通过 GoodReader 接入契约")?;
        fs::rename(&staging_root, &final_root).context("无法将转换结果写入书库")?;
        Ok(imported)
    }) {
        Ok(imported) => Ok(imported),
        Err(error) => {
            let _ = fs::remove_dir_all(&staging_root);
            Err(error)
        }
    }
}

fn convert_into_staging(
    source: &Path,
    files: &[SourceFile],
    entry: &Path,
    chapter_sources: &[PathBuf],
    id: &str,
    staging_root: &Path,
) -> Result<ImportedBook> {
    let mut stats = ConversionStats::default();
    let legacy_parallel = collect_legacy_parallel_sources(files, chapter_sources)?;
    let mut converted_parallel = BTreeMap::new();
    let entry_source = source.join(entry);
    let entry_original = fs::read_to_string(&entry_source)
        .with_context(|| format!("首页必须是 UTF-8 HTML：{}", entry.display()))?;
    let entry_clean = sanitize_html(&entry_original, &mut stats);
    let title = document_title(&entry_clean).unwrap_or_else(|| {
        source
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    let title = if title.trim().is_empty() {
        "未命名书籍".to_string()
    } else {
        title.trim().to_string()
    };
    let author = document_author(&entry_clean).unwrap_or_else(|| "未知作者".to_string());

    let mut copied_images = Vec::new();
    for file in files {
        if file.relative == Path::new("book.json") {
            continue;
        }
        let destination = staging_root.join(&file.relative);
        let extension = extension(&file.relative);
        match extension.as_str() {
            "html" | "htm" => {
                let html = fs::read_to_string(&file.absolute)
                    .with_context(|| format!("HTML 必须使用 UTF-8：{}", file.relative.display()))?;
                let mut clean = sanitize_html(&html, &mut stats);
                if let Some(index) = chapter_sources
                    .iter()
                    .position(|chapter| chapter == &file.relative)
                {
                    let chapter_id = format!("chapter-{:04}", index + 1);
                    if let Some(original_blocks) = legacy_parallel.get(&file.relative) {
                        let transformed = transform_legacy_parallel_chapter(&clean, &chapter_id)
                            .with_context(|| {
                                format!("无法转换双语章节 {}", file.relative.display())
                            })?;
                        if original_blocks.len() != transformed.block_ids.len() {
                            bail!(
                                "{} 可标注正文块 {} 个，但旧版原文 {} 段",
                                file.relative.display(),
                                transformed.block_ids.len(),
                                original_blocks.len()
                            );
                        }
                        let parallel_path =
                            PathBuf::from("parallel").join(format!("{chapter_id}.en.json"));
                        let blocks = transformed
                            .block_ids
                            .into_iter()
                            .zip(original_blocks.iter().cloned())
                            .collect();
                        let parallel = ParallelText {
                            schema_version: 1,
                            language: "en".to_string(),
                            blocks,
                        };
                        write_text_file(
                            &staging_root.join(&parallel_path),
                            &(serde_json::to_string_pretty(&parallel)
                                .context("无法生成对照原文 JSON")?
                                + "\n"),
                        )?;
                        converted_parallel.insert(file.relative.clone(), web_path(&parallel_path)?);
                        stats.converted_parallel_texts += 1;
                        clean = transformed.html;
                    } else {
                        clean = transform_chapter(&clean, &chapter_id)
                            .with_context(|| format!("无法转换章节 {}", file.relative.display()))?;
                    }
                }
                write_text_file(&destination, &clean)?;
            }
            "css" => {
                let css = fs::read_to_string(&file.absolute)
                    .with_context(|| format!("CSS 必须使用 UTF-8：{}", file.relative.display()))?;
                let clean = sanitize_css(&css, &mut stats);
                write_text_file(&destination, &clean)?;
            }
            "json" | "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "ico" | "woff"
            | "woff2" | "ttf" | "otf" => {
                copy_regular_file(file, &destination)?;
                if is_image(&file.relative) {
                    copied_images.push(file.relative.clone());
                }
            }
            "js" | "mjs" | "cjs" | "wasm" => {
                let is_converted_parallel = legacy_parallel.keys().any(|chapter| {
                    legacy_parallel_path(chapter).is_some_and(|path| path == file.relative)
                });
                if !is_converted_parallel {
                    stats.skipped_scripts += 1;
                }
            }
            _ => {
                stats.skipped_unsupported += 1;
            }
        }
    }

    let entry_path = web_path(entry)?;
    let mut chapters = chapter_sources
        .iter()
        .enumerate()
        .map(|(index, relative)| {
            let html = fs::read_to_string(staging_root.join(relative))
                .with_context(|| format!("无法读取转换章节 {}", relative.display()))?;
            Ok(ChapterManifest {
                id: format!("chapter-{:04}", index + 1),
                title: document_title(&html)
                    .unwrap_or_else(|| humanize_file_name(relative, index + 1)),
                path: web_path(relative)?,
                parallel_text: converted_parallel.get(relative).cloned(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    if chapters.is_empty() {
        let chapter_id = "chapter-0001";
        let chapter_path = PathBuf::from("goodreader-chapter.html");
        let chapter_html = transform_chapter(&entry_clean, chapter_id)
            .context("单页 HTML 无法转换为 GoodReader 正文")?;
        write_text_file(&staging_root.join(&chapter_path), &chapter_html)?;
        chapters.push(ChapterManifest {
            id: chapter_id.to_string(),
            title: title.clone(),
            path: web_path(&chapter_path)?,
            parallel_text: None,
        });
    }

    let cover = choose_cover(&copied_images).map_or_else(
        || {
            stats.used_default_cover = true;
            let path = PathBuf::from("goodreader-cover.png");
            fs::write(staging_root.join(&path), DEFAULT_COVER).context("无法写入默认封面")?;
            Ok::<PathBuf, anyhow::Error>(path)
        },
        |path| Ok(path.clone()),
    )?;

    let manifest = BookManifest {
        schema_version: 1,
        id: id.to_string(),
        title: title.clone(),
        original_title: None,
        author,
        language: None,
        source_language: None,
        target_language: None,
        cover: web_path(&cover)?,
        entry: entry_path,
        chapters,
    };
    let manifest_json =
        serde_json::to_string_pretty(&manifest).context("无法生成 book.json")? + "\n";
    fs::write(staging_root.join("book.json"), manifest_json).context("无法写入 book.json")?;

    Ok(ImportedBook {
        id: id.to_string(),
        title,
        chapter_count: manifest.chapters.len(),
        warnings: conversion_warnings(&stats),
    })
}

#[derive(Debug)]
struct LegacyParallelChapter {
    html: String,
    block_ids: Vec<String>,
}

fn collect_legacy_parallel_sources(
    files: &[SourceFile],
    chapter_sources: &[PathBuf],
) -> Result<BTreeMap<PathBuf, Vec<String>>> {
    let mut parallel = BTreeMap::new();
    for chapter in chapter_sources {
        let Some(candidate) = legacy_parallel_path(chapter) else {
            continue;
        };
        let Some(file) = files.iter().find(|file| file.relative == candidate) else {
            continue;
        };
        let source = fs::read_to_string(&file.absolute)
            .with_context(|| format!("旧版对照原文必须使用 UTF-8：{}", file.relative.display()))?;
        if !source.contains("window.EN_TEXT") {
            continue;
        }
        parallel.insert(
            chapter.clone(),
            parse_legacy_parallel_text(&source, &file.relative)?,
        );
    }
    Ok(parallel)
}

fn legacy_parallel_path(chapter: &Path) -> Option<PathBuf> {
    is_html(chapter).then(|| chapter.with_extension("en.js"))
}

fn parse_legacy_parallel_text(source: &str, source_name: &Path) -> Result<Vec<String>> {
    let marker = "window.EN_TEXT";
    let marker_index = source
        .find(marker)
        .with_context(|| format!("{} 缺少 window.EN_TEXT 数组", source_name.display()))?;
    let array_start = source[marker_index..]
        .find('[')
        .map(|index| marker_index + index)
        .with_context(|| format!("{} 缺少 window.EN_TEXT 数组", source_name.display()))?;
    let bytes = source.as_bytes();
    let mut blocks = Vec::new();
    let mut index = array_start + 1;

    loop {
        skip_legacy_array_separators(source, &mut index, source_name)?;
        match bytes.get(index) {
            Some(b']') => return Ok(blocks),
            Some(b'`') => {}
            Some(_) => bail!("{} 只允许反引号纯文本数组", source_name.display()),
            None => bail!("{} 数组未闭合", source_name.display()),
        }

        index += 1;
        let value_start = index;
        let mut escaped = false;
        while let Some(character) = bytes.get(index) {
            if *character == b'`' && !escaped {
                break;
            }
            escaped = *character == b'\\' && !escaped;
            if *character != b'\\' {
                escaped = false;
            }
            index += 1;
        }
        if bytes.get(index) != Some(&b'`') {
            bail!("{} 字符串未闭合", source_name.display());
        }
        let raw = &source[value_start..index];
        blocks.push(decode_legacy_template_literal(raw, source_name)?);
        index += 1;
    }
}

fn skip_legacy_array_separators(source: &str, index: &mut usize, source_name: &Path) -> Result<()> {
    let bytes = source.as_bytes();
    loop {
        while bytes
            .get(*index)
            .is_some_and(|value| value.is_ascii_whitespace() || *value == b',')
        {
            *index += 1;
        }
        if source[*index..].starts_with("//") {
            *index = source[*index..]
                .find('\n')
                .map_or(source.len(), |offset| *index + offset + 1);
            continue;
        }
        if source[*index..].starts_with("/*") {
            let end = source[*index + 2..]
                .find("*/")
                .map(|offset| *index + 2 + offset + 2)
                .with_context(|| format!("{} 块注释未闭合", source_name.display()))?;
            *index = end;
            continue;
        }
        return Ok(());
    }
}

fn decode_legacy_template_literal(raw: &str, source_name: &Path) -> Result<String> {
    if raw.contains("${") {
        bail!("{} 含模板表达式，拒绝执行式转换", source_name.display());
    }
    let mut value = String::new();
    let mut characters = raw.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            value.push(character);
            continue;
        }
        let escaped = characters
            .next()
            .with_context(|| format!("{} 含不完整转义", source_name.display()))?;
        match escaped {
            'n' => value.push('\n'),
            'r' => value.push('\r'),
            't' => value.push('\t'),
            '`' => value.push('`'),
            '\\' => value.push('\\'),
            other => {
                value.push('\\');
                value.push(other);
            }
        }
    }
    Ok(value)
}

fn transform_legacy_parallel_chapter(
    html: &str,
    chapter_id: &str,
) -> Result<LegacyParallelChapter> {
    let main = chapter_body_main_open_regex()
        .find(html)
        .context("旧版双语章节缺少 chapter-body 主正文")?;
    let tail = &html[main.end()..];
    let closing = main_close_regex()
        .find(tail)
        .context("旧版双语章节主正文未闭合")?;
    let main_end = main.end() + closing.start();
    let mut block_index = 0_usize;
    let main_content = mark_legacy_parallel_blocks(
        &html[main.end()..main_end],
        chapter_id,
        false,
        &mut block_index,
    );

    let scope_start = in_this_chapter_open_regex()
        .find_iter(&html[..main.start()])
        .last()
        .map_or(main.start(), |opening| opening.start());
    let pre_main = mark_legacy_parallel_blocks(
        &html[scope_start..main.start()],
        chapter_id,
        true,
        &mut block_index,
    );
    if block_index == 0 {
        bail!("旧版双语章节没有可标注正文块");
    }

    let main_closing_end = main.end() + closing.end();
    let html = format!(
        "{}<div data-goodreader-content data-goodreader-chapter=\"{}\">{}{}{}{}</div>{}",
        &html[..scope_start],
        chapter_id,
        pre_main,
        main.as_str(),
        main_content,
        &html[main_end..main_closing_end],
        &html[main_closing_end..]
    );
    let block_ids = (1..=block_index)
        .map(|index| format!("{chapter_id}-b{index:04}"))
        .collect();
    Ok(LegacyParallelChapter { html, block_ids })
}

fn mark_legacy_parallel_blocks(
    fragment: &str,
    chapter_id: &str,
    list_items_only: bool,
    block_index: &mut usize,
) -> String {
    let mut capturing: Option<String> = None;
    legacy_parallel_tag_regex()
        .replace_all(fragment, |capture: &Captures<'_>| {
            let closing = capture.get(1).is_some_and(|value| value.as_str() == "/");
            let tag = capture.get(2).expect("旧版正文标签捕获存在").as_str();
            let normalized = tag.to_ascii_lowercase();
            if closing {
                if capturing.as_deref() == Some(normalized.as_str()) {
                    capturing = None;
                }
                return capture
                    .get(0)
                    .expect("完整标签捕获存在")
                    .as_str()
                    .to_string();
            }
            if capturing.is_some() || (list_items_only && normalized != "li") {
                return capture
                    .get(0)
                    .expect("完整标签捕获存在")
                    .as_str()
                    .to_string();
            }
            let attributes = capture.get(3).map_or("", |value| value.as_str());
            if normalized == "blockquote" && !epigraph_class_regex().is_match(attributes) {
                return capture
                    .get(0)
                    .expect("完整标签捕获存在")
                    .as_str()
                    .to_string();
            }
            *block_index += 1;
            capturing = Some(normalized);
            format!(
                "<{tag}{attributes} data-goodreader-block=\"{chapter_id}-b{:04}\">",
                *block_index
            )
        })
        .into_owned()
}

fn collect_source_files(root: &Path) -> Result<Vec<SourceFile>> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    let mut total_bytes = 0_u64;

    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory)
            .with_context(|| format!("无法读取目录 {}", directory.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let file_type = entry.file_type().context("无法读取目录项类型")?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .context("无法计算导入相对路径")?
                .to_path_buf();

            if file_type.is_symlink() {
                bail!("导入目录不得包含符号链接：{}", relative.display());
            }
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if !file_type.is_file() {
                bail!("导入目录包含不支持的文件类型：{}", relative.display());
            }

            let size = entry.metadata()?.len();
            if size > MAX_SINGLE_FILE_BYTES {
                bail!("文件超过 64 MB：{}", relative.display());
            }
            total_bytes = total_bytes.saturating_add(size);
            if total_bytes > MAX_SOURCE_BYTES {
                bail!("导入目录总大小超过 2 GB");
            }
            files.push(SourceFile {
                absolute: path,
                relative,
                size,
            });
            if files.len() > MAX_SOURCE_FILES {
                bail!("导入目录文件数超过 {MAX_SOURCE_FILES}");
            }
        }
    }
    Ok(files)
}

fn choose_entry(html_files: &[PathBuf]) -> PathBuf {
    html_files
        .iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.eq_ignore_ascii_case("index.html")
                        || name.eq_ignore_ascii_case("index.htm")
                })
        })
        .min_by_key(|path| path.components().count())
        .cloned()
        .unwrap_or_else(|| html_files[0].clone())
}

fn choose_chapters(html_files: &[PathBuf], entry: &Path) -> Vec<PathBuf> {
    let chapter_directory_files = html_files
        .iter()
        .filter(|path| {
            path != &entry
                && path.components().any(|component| {
                    component.as_os_str().to_str().is_some_and(|value| {
                        matches!(
                            value.to_ascii_lowercase().as_str(),
                            "chapter" | "chapters" | "content" | "contents"
                        )
                    })
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    if !chapter_directory_files.is_empty() {
        chapter_directory_files
    } else {
        html_files
            .iter()
            .filter(|path| path.as_path() != entry)
            .cloned()
            .collect()
    }
}

fn sanitize_html(html: &str, stats: &mut ConversionStats) -> String {
    let original = html;
    let mut clean = script_regex().replace_all(html, "").into_owned();
    clean = active_element_regex().replace_all(&clean, "").into_owned();
    clean = active_open_tag_regex().replace_all(&clean, "").into_owned();
    clean = form_tag_regex().replace_all(&clean, "").into_owned();
    clean = base_tag_regex().replace_all(&clean, "").into_owned();
    clean = meta_refresh_tag_regex()
        .replace_all(&clean, "")
        .into_owned();
    clean = external_link_tag_regex()
        .replace_all(&clean, "")
        .into_owned();
    clean = inline_event_attribute_regex()
        .replace_all(&clean, "")
        .into_owned();
    clean = unsafe_url_attribute_regex()
        .replace_all(&clean, "")
        .into_owned();
    clean = unsafe_style_attribute_regex()
        .replace_all(&clean, "")
        .into_owned();
    clean = goodreader_attribute_regex()
        .replace_all(&clean, "")
        .into_owned();
    clean = style_block_regex()
        .replace_all(&clean, |capture: &Captures<'_>| {
            let css = capture.get(1).map_or("", |value| value.as_str());
            format!("<style>{}</style>", sanitize_css_fragment(css))
        })
        .into_owned();
    if clean != original {
        stats.sanitized_html += 1;
    }
    clean
}

fn sanitize_css(css: &str, stats: &mut ConversionStats) -> String {
    let clean = sanitize_css_fragment(css);
    if clean != css {
        stats.sanitized_css += 1;
    }
    clean
}

fn sanitize_css_fragment(css: &str) -> String {
    let mut clean = css_import_regex().replace_all(css, "").into_owned();
    clean = external_css_url_regex()
        .replace_all(&clean, "url(\"\")")
        .into_owned();
    clean = css_expression_regex().replace_all(&clean, "").into_owned();
    clean = clean.replace("javascript:", "");
    clean
}

fn transform_chapter(html: &str, chapter_id: &str) -> Result<String> {
    if let Some(opening) = main_open_regex().find(html) {
        let tail = &html[opening.end()..];
        let closing = main_close_regex().find(tail).context("main 元素未闭合")?;
        let inner_end = opening.end() + closing.start();
        let opening_tag = add_content_attributes(opening.as_str(), chapter_id);
        let (marked, count) = mark_blocks(&html[opening.end()..inner_end], chapter_id);
        let content = ensure_one_block(marked, count, chapter_id);
        return Ok(format!(
            "{}{}{}{}",
            &html[..opening.start()],
            opening_tag,
            content,
            &html[inner_end..]
        ));
    }

    if let Some(opening) = body_open_regex().find(html) {
        let tail = &html[opening.end()..];
        let closing = body_close_regex().find(tail).context("body 元素未闭合")?;
        let inner_end = opening.end() + closing.start();
        let (marked, count) = mark_blocks(&html[opening.end()..inner_end], chapter_id);
        let content = ensure_one_block(marked, count, chapter_id);
        return Ok(format!(
            "{}{}<main data-goodreader-content data-goodreader-chapter=\"{}\">{}</main>{}",
            &html[..opening.start()],
            opening.as_str(),
            chapter_id,
            content,
            &html[inner_end..]
        ));
    }

    let (marked, count) = mark_blocks(html, chapter_id);
    let content = ensure_one_block(marked, count, chapter_id);
    Ok(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"></head><body><main data-goodreader-content data-goodreader-chapter=\"{chapter_id}\">{content}</main></body></html>"
    ))
}

fn mark_blocks(fragment: &str, chapter_id: &str) -> (String, usize) {
    let mut count = 0_usize;
    let marked = block_open_regex()
        .replace_all(fragment, |capture: &Captures<'_>| {
            count += 1;
            let tag = capture.get(1).expect("标签捕获存在").as_str();
            let attributes = capture.get(2).map_or("", |value| value.as_str());
            format!("<{tag}{attributes} data-goodreader-block=\"{chapter_id}-block-{count:04}\">")
        })
        .into_owned();
    (marked, count)
}

fn ensure_one_block(content: String, count: usize, chapter_id: &str) -> String {
    if count > 0 {
        content
    } else {
        format!("<div data-goodreader-block=\"{chapter_id}-block-0001\">{content}</div>")
    }
}

fn add_content_attributes(opening_tag: &str, chapter_id: &str) -> String {
    let insert_at = opening_tag.rfind('>').unwrap_or(opening_tag.len());
    format!(
        "{} data-goodreader-content data-goodreader-chapter=\"{}\"{}",
        &opening_tag[..insert_at],
        chapter_id,
        &opening_tag[insert_at..]
    )
}

fn document_title(html: &str) -> Option<String> {
    let candidate = h1_regex()
        .captures(html)
        .or_else(|| title_regex().captures(html))?;
    let value = candidate.get(1)?.as_str();
    let text = strip_tags(value);
    (!text.is_empty()).then_some(text)
}

fn document_author(html: &str) -> Option<String> {
    let captures = author_meta_regex()
        .captures(html)
        .or_else(|| author_meta_reversed_regex().captures(html))?;
    let value = captures.get(1)?.as_str();
    let text = decode_basic_entities(value).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn strip_tags(value: &str) -> String {
    let text = html_tag_regex().replace_all(value, " ");
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

fn humanize_file_name(path: &Path, index: usize) -> String {
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .replace(['-', '_'], " ");
    let title = stem.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        format!("第 {index} 章")
    } else {
        title
    }
}

fn choose_cover(images: &[PathBuf]) -> Option<&PathBuf> {
    const NAMES: &[&str] = &["cover", "front-cover", "front", "thumbnail", "poster"];
    images
        .iter()
        .min_by_key(|path| {
            let stem = path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            NAMES
                .iter()
                .position(|candidate| *candidate == stem)
                .unwrap_or(NAMES.len())
        })
        .filter(|path| {
            let stem = path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            NAMES.contains(&stem.as_str())
        })
}

fn conversion_warnings(stats: &ConversionStats) -> Vec<String> {
    let mut warnings = Vec::new();
    if stats.converted_parallel_texts > 0 {
        warnings.push(format!(
            "已安全转换 {} 章旧版对照原文",
            stats.converted_parallel_texts
        ));
    }
    if stats.skipped_scripts > 0 {
        warnings.push(format!(
            "已移除 {} 个脚本或 WebAssembly 文件",
            stats.skipped_scripts
        ));
    }
    if stats.sanitized_html > 0 {
        warnings.push(format!(
            "已静态化处理 {} 个 HTML 文件中的脚本或主动内容",
            stats.sanitized_html
        ));
    }
    if stats.sanitized_css > 0 {
        warnings.push(format!(
            "已移除 {} 个 CSS 文件中的外部或可执行规则",
            stats.sanitized_css
        ));
    }
    if stats.skipped_unsupported > 0 {
        warnings.push(format!(
            "已跳过 {} 个 GoodReader 不支持的文件",
            stats.skipped_unsupported
        ));
    }
    if stats.used_default_cover {
        warnings.push("未找到封面，已使用 GoodReader 默认封面".to_string());
    }
    warnings
}

fn copy_regular_file(file: &SourceFile, destination: &Path) -> Result<()> {
    if file.size > MAX_SINGLE_FILE_BYTES {
        bail!("文件超过 64 MB：{}", file.relative.display());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&file.absolute, destination)
        .with_context(|| format!("无法复制 {}", file.relative.display()))?;
    Ok(())
}

fn write_text_file(destination: &Path, text: &str) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(destination, text).with_context(|| format!("无法写入 {}", destination.display()))?;
    Ok(())
}

fn web_path(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .with_context(|| format!("文件名不是有效 UTF-8：{}", path.display()))?;
    Ok(value.replace('\\', "/"))
}

fn destination_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "book".to_string()
    } else {
        slug.to_string()
    }
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn is_html(path: &Path) -> bool {
    matches!(extension(path).as_str(), "html" | "htm")
}

fn is_image(path: &Path) -> bool {
    matches!(
        extension(path).as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "ico"
    )
}

fn natural_path_cmp(left: &Path, right: &Path) -> Ordering {
    natural_cmp(
        &left.to_string_lossy().to_ascii_lowercase(),
        &right.to_string_lossy().to_ascii_lowercase(),
    )
}

fn natural_cmp(left: &str, right: &str) -> Ordering {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let (mut left_index, mut right_index) = (0_usize, 0_usize);
    while left_index < left.len() && right_index < right.len() {
        if left[left_index].is_ascii_digit() && right[right_index].is_ascii_digit() {
            let left_end = digit_end(left, left_index);
            let right_end = digit_end(right, right_index);
            let left_number = &left[left_index..left_end];
            let right_number = &right[right_index..right_end];
            let left_trimmed = trim_leading_zeroes(left_number);
            let right_trimmed = trim_leading_zeroes(right_number);
            let ordering = left_trimmed
                .len()
                .cmp(&right_trimmed.len())
                .then_with(|| left_trimmed.cmp(right_trimmed))
                .then_with(|| left_number.len().cmp(&right_number.len()));
            if ordering != Ordering::Equal {
                return ordering;
            }
            left_index = left_end;
            right_index = right_end;
            continue;
        }
        let ordering = left[left_index].cmp(&right[right_index]);
        if ordering != Ordering::Equal {
            return ordering;
        }
        left_index += 1;
        right_index += 1;
    }
    left.len().cmp(&right.len())
}

fn digit_end(value: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < value.len() && value[end].is_ascii_digit() {
        end += 1;
    }
    end
}

fn trim_leading_zeroes(value: &[u8]) -> &[u8] {
    let first_non_zero = value
        .iter()
        .position(|character| *character != b'0')
        .unwrap_or(value.len().saturating_sub(1));
    &value[first_non_zero..]
}

macro_rules! regex {
    ($name:ident, $pattern:literal) => {
        fn $name() -> &'static Regex {
            static REGEX: OnceLock<Regex> = OnceLock::new();
            REGEX.get_or_init(|| Regex::new($pattern).expect("导入正则固定有效"))
        }
    };
}

regex!(script_regex, r"(?is)<script\b[^>]*>.*?</script\s*>");
regex!(
    active_element_regex,
    r"(?is)<(?:iframe|object|embed|svg)\b[^>]*>.*?</(?:iframe|object|embed|svg)\s*>"
);
regex!(
    active_open_tag_regex,
    r"(?is)</?(?:iframe|object|embed|svg)\b[^>]*>"
);
regex!(form_tag_regex, r"(?is)</?form\b[^>]*>");
regex!(base_tag_regex, r"(?is)<base\b[^>]*>");
regex!(
    meta_refresh_tag_regex,
    r#"(?is)<meta\b[^>]*http-equiv\s*=\s*["']?\s*refresh\b[^>]*>"#
);
regex!(
    external_link_tag_regex,
    r#"(?is)<link\b[^>]*href\s*=\s*["']\s*(?:https?:)?//[^"']*["'][^>]*>"#
);
regex!(
    inline_event_attribute_regex,
    r#"(?is)\s+on[a-z][a-z0-9_-]*\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+)"#
);
regex!(
    unsafe_url_attribute_regex,
    r#"(?is)\s+(?:src|srcset|poster|action|href)\s*=\s*(?:"\s*(?:javascript:|https?://|//)[^"]*"|'\s*(?:javascript:|https?://|//)[^']*')"#
);
regex!(
    unsafe_style_attribute_regex,
    r#"(?is)\s+style\s*=\s*(?:"[^"]*(?:javascript:|expression\s*\(|url\s*\(\s*['"]?\s*(?:https?:|//))[^"]*"|'[^']*(?:javascript:|expression\s*\(|url\s*\(\s*["']?\s*(?:https?:|//))[^']*')"#
);
regex!(
    goodreader_attribute_regex,
    r#"(?is)\s+data-goodreader-(?:content|chapter|block)(?:\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+))?"#
);
regex!(style_block_regex, r"(?is)<style\b[^>]*>(.*?)</style\s*>");
regex!(
    css_import_regex,
    r#"(?is)@import\s+(?:url\s*\([^)]*\)|"[^"]*"|'[^']*')[^;]*;"#
);
regex!(
    external_css_url_regex,
    r#"(?is)url\s*\(\s*["']?\s*(?:https?:|//)[^)]*\)"#
);
regex!(css_expression_regex, r"(?is)expression\s*\([^)]*\)");
regex!(main_open_regex, r"(?is)<main\b[^>]*>");
regex!(main_close_regex, r"(?is)</main\s*>");
regex!(
    chapter_body_main_open_regex,
    r#"(?is)<main\b[^>]*class=["'][^"']*\bchapter-body\b[^"']*["'][^>]*>"#
);
regex!(
    in_this_chapter_open_regex,
    r#"(?is)<div\b[^>]*class=["'][^"']*\bin-this-chapter\b[^"']*["'][^>]*>"#
);
regex!(
    legacy_parallel_tag_regex,
    r"(?is)<(/?)(p|li|blockquote)\b([^>]*)>"
);
regex!(
    epigraph_class_regex,
    r#"(?is)\bclass=["'][^"']*\bepigraph\b[^"']*["']"#
);
regex!(body_open_regex, r"(?is)<body\b[^>]*>");
regex!(body_close_regex, r"(?is)</body\s*>");
regex!(
    block_open_regex,
    r"(?is)<(p|li|blockquote|pre|h[1-6]|td|th)(\s[^>]*)?>"
);
regex!(h1_regex, r"(?is)<h1\b[^>]*>(.*?)</h1\s*>");
regex!(title_regex, r"(?is)<title\b[^>]*>(.*?)</title\s*>");
regex!(
    author_meta_regex,
    r#"(?is)<meta\b[^>]*name\s*=\s*["']author["'][^>]*content\s*=\s*["']([^"']+)["'][^>]*>"#
);
regex!(
    author_meta_reversed_regex,
    r#"(?is)<meta\b[^>]*content\s*=\s*["']([^"']+)["'][^>]*name\s*=\s*["']author["'][^>]*>"#
);
regex!(html_tag_regex, r"(?is)<[^>]+>");

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{import_html_directory, natural_cmp, parse_legacy_parallel_text};
    use crate::library::validate_package;

    #[test]
    fn imports_plain_html_as_a_valid_goodreader_book() {
        let temp = TempDir::new().expect("创建临时目录");
        let source = temp.path().join("source");
        let books = temp.path().join("Books");
        fs::create_dir_all(source.join("chapters")).expect("创建源目录");
        fs::write(
            source.join("index.html"),
            r#"<!doctype html><html><head><title>示例书</title><meta name="author" content="作者甲"></head><body><a href="chapters/ch1.html">第一章</a><script src="reader.js"></script></body></html>"#,
        )
        .expect("写首页");
        fs::write(
            source.join("chapters/ch1.html"),
            r#"<!doctype html><html><head><title>第一章</title></head><body><h1>第一章</h1><p onclick="alert(1)">正文内容</p></body></html>"#,
        )
        .expect("写章节");
        fs::write(source.join("reader.js"), "alert(1)").expect("写脚本");

        let before = fs::read_to_string(source.join("chapters/ch1.html")).expect("读源章节");
        let imported = import_html_directory(&source, &books).expect("导入成功");
        let destination = fs::read_dir(&books)
            .expect("读取书库")
            .next()
            .expect("存在导入目录")
            .expect("读取目录项")
            .path();

        let package = validate_package(&destination).expect("转换结果符合契约");
        assert_eq!(package.manifest.id, imported.id);
        assert_eq!(package.manifest.title, "示例书");
        assert_eq!(package.manifest.author, "作者甲");
        assert_eq!(package.manifest.chapters.len(), 1);
        let chapter =
            fs::read_to_string(destination.join("chapters/ch1.html")).expect("读取转换章节");
        assert!(chapter.contains("data-goodreader-content"));
        assert!(chapter.contains("data-goodreader-block"));
        assert!(!chapter.contains("onclick"));
        assert!(!destination.join("reader.js").exists());
        assert_eq!(
            fs::read_to_string(source.join("chapters/ch1.html")).expect("重读源章节"),
            before
        );
    }

    #[test]
    fn imports_legacy_parallel_text_as_read_only_json() {
        let temp = TempDir::new().expect("创建临时目录");
        let source = temp.path().join("source");
        let books = temp.path().join("Books");
        fs::create_dir_all(source.join("chapters")).expect("创建源目录");
        fs::write(
            source.join("index.html"),
            r#"<!doctype html><title>双语书</title><body><a href="chapters/ch1.html">第一章</a></body>"#,
        )
        .expect("写首页");
        fs::write(
            source.join("chapters/ch1.html"),
            r#"<!doctype html><html><head><title>第一章</title></head><body><main class="chapter-body"><p>第一段</p><h2>小节</h2><blockquote class="epigraph">引文</blockquote><p>第二段</p></main></body></html>"#,
        )
        .expect("写章节");
        fs::write(
            source.join("chapters/ch1.en.js"),
            "window.EN_TEXT = [`First paragraph`, `Quotation`, `Second paragraph`];",
        )
        .expect("写旧版原文");

        import_html_directory(&source, &books).expect("导入成功");
        let destination = fs::read_dir(&books)
            .expect("读取书库")
            .next()
            .expect("存在导入目录")
            .expect("读取目录项")
            .path();
        let package = validate_package(&destination).expect("转换结果符合契约");
        let chapter = &package.manifest.chapters[0];
        let parallel_path = chapter.parallel_text.as_deref().expect("章节应声明原文");
        let parallel: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(destination.join(parallel_path)).expect("读取对照原文"),
        )
        .expect("对照原文是合法 JSON");

        assert_eq!(parallel["schemaVersion"], 1);
        assert_eq!(parallel["language"], "en");
        assert_eq!(parallel["blocks"]["chapter-0001-b0001"], "First paragraph");
        assert_eq!(parallel["blocks"]["chapter-0001-b0002"], "Quotation");
        assert_eq!(parallel["blocks"]["chapter-0001-b0003"], "Second paragraph");
        assert!(!destination.join("chapters/ch1.en.js").exists());
    }

    #[test]
    fn rejects_executable_legacy_parallel_templates() {
        let error = parse_legacy_parallel_text(
            "window.EN_TEXT = [`safe ${alert(1)}`];",
            std::path::Path::new("chapter.en.js"),
        )
        .expect_err("模板表达式必须被拒绝");
        assert!(format!("{error:#}").contains("模板表达式"));
    }

    #[test]
    fn imports_a_single_html_file_with_a_default_cover() {
        let temp = TempDir::new().expect("创建临时目录");
        let source = temp.path().join("single");
        let books = temp.path().join("Books");
        fs::create_dir_all(&source).expect("创建源目录");
        fs::write(
            source.join("index.html"),
            "<!doctype html><title>单页书</title><body><p>全部正文</p></body>",
        )
        .expect("写单页书");

        let imported = import_html_directory(&source, &books).expect("导入成功");
        assert_eq!(imported.chapter_count, 1);
        assert!(imported
            .warnings
            .iter()
            .any(|warning| warning.contains("默认封面")));
    }

    #[test]
    fn sorts_numbered_chapters_naturally() {
        assert!(natural_cmp("chapter2.html", "chapter10.html").is_lt());
        assert!(natural_cmp("part01.html", "part1.html").is_gt());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("创建临时目录");
        let source = temp.path().join("source");
        let books = temp.path().join("Books");
        fs::create_dir_all(&source).expect("创建源目录");
        fs::write(source.join("index.html"), "<body><p>正文</p></body>").expect("写首页");
        symlink("/tmp", source.join("escape")).expect("创建符号链接");

        let error = import_html_directory(&source, &books).expect_err("符号链接必须被拒绝");
        assert!(format!("{error:#}").contains("符号链接"));
    }
}

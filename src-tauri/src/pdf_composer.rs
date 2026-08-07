use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::agent::AgentCoordinator;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfSourceLine {
    pub id: String,
    pub text: String,
    pub removable: bool,
}

#[derive(Debug, Clone)]
pub struct PdfPageSource {
    pub page: usize,
    pub image_path: PathBuf,
    pub image_width: usize,
    pub image_height: usize,
    pub requires_figure: bool,
    pub lines: Vec<PdfSourceLine>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfCropBox {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

#[derive(Debug, Clone)]
pub struct ComposedPdfFigure {
    pub marker: String,
    pub crop: PdfCropBox,
    pub alt: String,
    pub caption: String,
}

#[derive(Debug, Clone)]
pub struct ComposedPdfPage {
    pub html: String,
    pub figures: Vec<ComposedPdfFigure>,
    pub reused: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PdfPageInput {
    page: usize,
    image: String,
    image_width: usize,
    image_height: usize,
    requires_figure: bool,
    lines: Vec<PdfSourceLine>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PdfPageLayout {
    page: usize,
    blocks: Vec<PdfPageBlock>,
    #[serde(default)]
    omitted_line_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PdfPageBlock {
    kind: String,
    #[serde(default)]
    line_ids: Vec<String>,
    #[serde(default)]
    level: Option<usize>,
    #[serde(default)]
    crop: Option<PdfCropBox>,
    #[serde(default)]
    alt: Option<String>,
    #[serde(default, rename = "caption")]
    _caption: Option<String>,
}

pub struct PdfPageComposer {
    agent: Arc<AgentCoordinator>,
}

impl PdfPageComposer {
    pub fn new(agent: Arc<AgentCoordinator>) -> Self {
        Self { agent }
    }

    pub async fn compose(
        &self,
        runtime_id: &str,
        workspace: &Path,
        source: &PdfPageSource,
    ) -> Result<ComposedPdfPage> {
        let input_dir = workspace.join("input");
        let output_dir = workspace.join("output");
        fs::create_dir_all(&input_dir)?;
        fs::create_dir_all(&output_dir)?;
        let input = PdfPageInput {
            page: source.page,
            image: "input/page.png".to_string(),
            image_width: source.image_width,
            image_height: source.image_height,
            requires_figure: source.requires_figure,
            lines: source.lines.clone(),
        };
        let input_json = serde_json::to_vec_pretty(&input)?;
        let input_path = input_dir.join("page.json");
        let result_path = output_dir.join("page.json");
        if fs::read(&input_path).ok().as_deref() != Some(input_json.as_slice()) {
            if result_path.exists() {
                fs::remove_file(&result_path)?;
            }
            fs::write(&input_path, &input_json)?;
        }
        let image_path = input_dir.join("page.png");
        if !image_path.is_file() {
            fs::copy(&source.image_path, &image_path).context("无法保存 PDF 页面图像")?;
        }

        if let Ok(layout) = read_and_validate_layout(&result_path, source) {
            return materialize_layout(layout, source, true);
        }

        let instruction = page_composition_instruction(source.page);
        self.agent
            .run_generation(runtime_id, workspace, &instruction)
            .await
            .with_context(|| format!("PDF 第 {} 页 Agent 排版失败", source.page))?;
        let layout = read_and_validate_layout(&result_path, source)?;
        materialize_layout(layout, source, false)
    }
}

fn page_composition_instruction(page: usize) -> String {
    format!(
        "请完成 PDF 第 {page} 页的书籍排版。读取 input/page.png 和 input/page.json。\n\
         page.json 中每一行都有稳定 lineId。结合页面视觉顺序，把这些行组织为语义块，最终只写 output/page.json。\n\
         输出对象格式：{{\"page\":{page},\"blocks\":[{{\"kind\":\"heading|paragraph|list|quote|code|table|figure\",\"lineIds\":[\"l0001\"],\"level\":2,\"crop\":{{\"x\":0,\"y\":0,\"width\":100,\"height\":100}},\"alt\":\"图片说明\",\"caption\":\"图注\"}}],\"omittedLineIds\":[]}}。\n\
         规则：\n\
         1. 不得改写、翻译或补充正文，只能引用 lineId；每个不可移除的 lineId 必须且只能出现一次。\n\
         2. 仅 removable=true 的页眉、页脚或页码可以放入 omittedLineIds。\n\
         3. 按页面真实阅读顺序排列块；多栏页面先完成左栏再进入右栏。\n\
         4. 每个承担信息的图表、截图、照片或示意图建立 figure 块，crop 使用 page.png 的像素坐标并完整包围视觉内容，不能只截局部；requiresFigure=true 时至少返回一个 figure。\n\
         5. 图注行放入对应 figure 的 lineIds；普通正文不要误作图片。\n\
         6. 只写 output/page.json，不修改 input，不解释。"
    )
}

fn read_and_validate_layout(path: &Path, source: &PdfPageSource) -> Result<PdfPageLayout> {
    let bytes = fs::read(path).context("Agent 没有生成 output/page.json")?;
    let layout = serde_json::from_slice::<PdfPageLayout>(&bytes)
        .context("Agent 页面排版结果不是合法结构化 JSON")?;
    validate_layout(&layout, source)?;
    Ok(layout)
}

fn validate_layout(layout: &PdfPageLayout, source: &PdfPageSource) -> Result<()> {
    if layout.page != source.page {
        bail!("Agent 页面编号不匹配");
    }
    let lines = source
        .lines
        .iter()
        .map(|line| (line.id.as_str(), line))
        .collect::<HashMap<_, _>>();
    let mut consumed = HashSet::new();
    let mut figure_count = 0usize;
    for block in &layout.blocks {
        if !matches!(
            block.kind.as_str(),
            "heading" | "paragraph" | "list" | "quote" | "code" | "table" | "figure"
        ) {
            bail!("Agent 返回了不支持的页面块类型：{}", block.kind);
        }
        if block.line_ids.is_empty() && block.kind != "figure" {
            bail!("{} 页面块没有引用来源行", block.kind);
        }
        for id in &block.line_ids {
            if !lines.contains_key(id.as_str()) {
                bail!("Agent 引用了不存在的来源行：{id}");
            }
            if !consumed.insert(id.as_str()) {
                bail!("来源行被重复使用：{id}");
            }
        }
        if block.kind == "figure" {
            figure_count += 1;
            let crop = block.crop.as_ref().context("图片块缺少完整裁切区域")?;
            if crop.width == 0
                || crop.height == 0
                || crop.x.saturating_add(crop.width) > source.image_width
                || crop.y.saturating_add(crop.height) > source.image_height
            {
                bail!("图片裁切区域超出 PDF 页面范围");
            }
        } else if block.crop.is_some() {
            bail!("非图片页面块不能声明裁切区域");
        }
    }
    for id in &layout.omitted_line_ids {
        let line = lines
            .get(id.as_str())
            .with_context(|| format!("Agent 省略了不存在的来源行：{id}"))?;
        if !line.removable {
            bail!("Agent 试图省略正文行：{id}");
        }
        if !consumed.insert(id.as_str()) {
            bail!("来源行被重复使用：{id}");
        }
    }
    let missing = source
        .lines
        .iter()
        .filter(|line| !consumed.contains(line.id.as_str()))
        .map(|line| line.id.as_str())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!("Agent 页面排版遗漏来源行：{}", missing.join(", "));
    }
    if layout.blocks.is_empty() && !source.lines.is_empty() {
        bail!("Agent 没有生成任何页面内容");
    }
    if source.requires_figure && figure_count == 0 {
        bail!("该页检测到嵌入图片，但 Agent 没有返回完整图片区域");
    }
    Ok(())
}

fn materialize_layout(
    layout: PdfPageLayout,
    source: &PdfPageSource,
    reused: bool,
) -> Result<ComposedPdfPage> {
    let lines = source
        .lines
        .iter()
        .map(|line| (line.id.as_str(), line.text.as_str()))
        .collect::<HashMap<_, _>>();
    let mut html = String::new();
    let mut figures = Vec::new();
    for (index, block) in layout.blocks.into_iter().enumerate() {
        let values = block
            .line_ids
            .iter()
            .filter_map(|id| lines.get(id.as_str()).copied())
            .collect::<Vec<_>>();
        match block.kind.as_str() {
            "heading" => {
                let level = block.level.unwrap_or(2).clamp(2, 6);
                html.push_str(&format!(
                    "<h{level}>{}</h{level}>",
                    escape_html(&values.join(" "))
                ));
            }
            "list" => {
                html.push_str("<ul>");
                for value in values {
                    html.push_str(&format!("<li>{}</li>", escape_html(value)));
                }
                html.push_str("</ul>");
            }
            "quote" => html.push_str(&format!(
                "<blockquote><p>{}</p></blockquote>",
                escape_html(&values.join(" "))
            )),
            "code" => html.push_str(&format!(
                "<pre><code>{}</code></pre>",
                escape_html(&values.join("\n"))
            )),
            "table" => {
                html.push_str("<div class=\"pdf-table\" role=\"table\">");
                for value in values {
                    html.push_str(&format!(
                        "<div class=\"pdf-table-row\" role=\"row\">{}</div>",
                        escape_html(value)
                    ));
                }
                html.push_str("</div>");
            }
            "figure" => {
                let marker = format!("{{{{GOODREADER_FIGURE:{index:04}}}}}");
                let caption = values.join(" ");
                let alt = block.alt.unwrap_or_else(|| {
                    (!caption.trim().is_empty())
                        .then(|| caption.clone())
                        .unwrap_or_else(|| format!("PDF 第 {} 页图片", source.page))
                });
                html.push_str(&marker);
                figures.push(ComposedPdfFigure {
                    marker,
                    crop: block.crop.context("图片块缺少裁切区域")?,
                    alt,
                    caption,
                });
            }
            _ => html.push_str(&format!("<p>{}</p>", escape_html(&values.join(" ")))),
        }
    }
    Ok(ComposedPdfPage {
        html,
        figures,
        reused,
    })
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
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::{
        validate_layout, PdfPageBlock, PdfPageComposer, PdfPageLayout, PdfPageSource, PdfSourceLine,
    };
    use crate::agent::AgentCoordinator;
    use crate::db::Database;

    #[tokio::test]
    async fn composes_one_pdf_page_through_the_selected_agent() {
        let temp = TempDir::new().unwrap();
        let agent_script = temp.path().join("page-agent.sh");
        fs::write(
            &agent_script,
            r#"#!/bin/sh
set -eu
cat >/dev/null
mkdir -p output
printf x >> calls.txt
cat > output/page.json <<'JSON'
{"page":1,"blocks":[{"kind":"heading","lineIds":["l0001"],"level":2},{"kind":"figure","lineIds":["l0002"],"crop":{"x":10,"y":20,"width":200,"height":100},"alt":"完整架构图","caption":"图 1-1 完整架构图"}],"omittedLineIds":[]}
JSON
printf 'done\n'
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&agent_script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&agent_script, permissions).unwrap();
        let database = Arc::new(Database::open(&temp.path().join("Data")).unwrap());
        let runtime = database
            .save_custom_agent_runtime("页面排版 Agent", agent_script.to_str().unwrap(), &[])
            .unwrap();
        let agent =
            Arc::new(AgentCoordinator::new(temp.path().join("AgentTasks"), database).unwrap());
        let composer = PdfPageComposer::new(agent);
        let image = temp.path().join("page.png");
        fs::write(&image, b"page image fixture").unwrap();
        let source = PdfPageSource {
            page: 1,
            image_path: image,
            image_width: 800,
            image_height: 1200,
            requires_figure: true,
            lines: vec![
                PdfSourceLine {
                    id: "l0001".to_string(),
                    text: "第一章 Agent 架构".to_string(),
                    removable: false,
                },
                PdfSourceLine {
                    id: "l0002".to_string(),
                    text: "图 1-1 完整架构图".to_string(),
                    removable: false,
                },
            ],
        };

        let composed = composer
            .compose(&runtime.id, &temp.path().join("page-0001"), &source)
            .await
            .unwrap();

        assert!(composed.html.contains("<h2>第一章 Agent 架构</h2>"));
        assert_eq!(composed.figures.len(), 1);
        assert_eq!(composed.figures[0].crop.width, 200);
        assert_eq!(
            fs::read_to_string(temp.path().join("page-0001/calls.txt")).unwrap(),
            "x"
        );

        let reused = composer
            .compose(&runtime.id, &temp.path().join("page-0001"), &source)
            .await
            .unwrap();
        assert!(reused.reused);
        assert_eq!(
            fs::read_to_string(temp.path().join("page-0001/calls.txt")).unwrap(),
            "x"
        );
    }

    #[test]
    fn rejects_layouts_that_drop_source_content() {
        let source = PdfPageSource {
            page: 3,
            image_path: "page.png".into(),
            image_width: 800,
            image_height: 1200,
            requires_figure: false,
            lines: vec![
                PdfSourceLine {
                    id: "l0001".to_string(),
                    text: "正文第一行".to_string(),
                    removable: false,
                },
                PdfSourceLine {
                    id: "l0002".to_string(),
                    text: "正文第二行".to_string(),
                    removable: false,
                },
            ],
        };
        let layout = PdfPageLayout {
            page: 3,
            blocks: vec![PdfPageBlock {
                kind: "paragraph".to_string(),
                line_ids: vec!["l0001".to_string()],
                level: None,
                crop: None,
                alt: None,
                _caption: None,
            }],
            omitted_line_ids: Vec::new(),
        };

        let error = validate_layout(&layout, &source).unwrap_err();
        assert!(error.to_string().contains("遗漏来源行：l0002"));
    }
}

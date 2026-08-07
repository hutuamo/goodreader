import { readFile } from "node:fs/promises";

const source = await readFile(new URL("../frontend/src/reader.ts", import.meta.url), "utf8");

for (const marker of [
  'import MarkdownIt from "markdown-it"',
  "html: false",
  'ruler.before("text", "goodreader-citation"',
  "aiMarkdown.render(value)",
  'class="gr-ai-message-body"',
]) {
  if (!source.includes(marker)) throw new Error(`AI Markdown 缺少实现：${marker}`);
}

console.log("AI 内容使用禁用原始 HTML 的 Markdown 渲染，并保留书籍引用按钮");

import { readFile } from "node:fs/promises";

const [readerSource, readerStyles, serverSource] = await Promise.all([
  readFile(new URL("../frontend/src/reader.ts", import.meta.url), "utf8"),
  readFile(new URL("../frontend/public/assets/reader.css", import.meta.url), "utf8"),
  readFile(new URL("../src-tauri/src/server.rs", import.meta.url), "utf8"),
]);

const requiredSource = [
  'loadPreference("reader-font-size")',
  'savePreference("reader-font-size"',
  "applyReaderFontSize();",
  'id="grReaderFontRange"',
  'injectSidebarResizer("toc")',
  'injectSidebarResizer("ai")',
  'savePreference(kind === "toc" ? "sidebar-width" : "ai-sidebar-width"',
  'event.key === "ArrowLeft"',
  'handle.addEventListener("dblclick"',
];

const requiredStyles = [
  "--gr-sidebar-width: 360px",
  "--gr-ai-width: 420px",
  ".gr-reader-settings-popover",
  ".gr-sidebar-resizer",
  "padding-right: var(--gr-ai-width)",
  "body.gr-resizing-sidebar",
];

const requiredServerSettings = ["reader-font-size", "sidebar-width", "ai-sidebar-width"];

for (const marker of requiredSource) {
  if (!readerSource.includes(marker)) throw new Error(`阅读显示设置缺少实现：${marker}`);
}

for (const marker of requiredStyles) {
  if (!readerStyles.includes(marker)) throw new Error(`阅读显示设置缺少样式：${marker}`);
}

for (const setting of requiredServerSettings) {
  if (!serverSource.includes(`validate_setting_key("${setting}").is_ok()`)) {
    throw new Error(`服务端未放行阅读设置：${setting}`);
  }
}

console.log("阅读字号与侧栏宽度检查通过");

import { readFile } from "node:fs/promises";

const source = await readFile(new URL("../frontend/src/reader.ts", import.meta.url), "utf8");

const requiredMarkers = [
  'data-action="ask-ai"',
  'title="问 AI"',
  '${readerIcon("sparkles")}',
  "function askAiAboutSelection(): void",
  "结合上下文内容，讲解这段内容的含义：",
  "aiViewState.draft =",
  "saveAiViewState();",
  "toggleAiSidebar(true, false);",
  "focusRequestedAiDraft();",
];

for (const marker of requiredMarkers) {
  if (!source.includes(marker)) throw new Error(`选区问 AI 缺少实现：${marker}`);
}

const implementation = source.slice(
  source.indexOf("function askAiAboutSelection"),
  source.indexOf("async function updateParallelButton"),
);

if (implementation.includes("submitAiQuestion") || implementation.includes(".gr-ai-send")) {
  throw new Error("选区问 AI 不应自动发送问题");
}

console.log("选区问 AI 会打开工作区、预填问题并等待用户确认发送");

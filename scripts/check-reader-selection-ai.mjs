// 选区问 AI 前端行为检查。
// P2-10：负向断言不再依赖「两个函数相对顺序」——slice 在顺序变化时会变成空串，
// 负向断言恒真；改为分别检查函数体边界与禁止行为标记。
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

// 负向断言：定位 askAiAboutSelection 函数体（到下一个顶层函数定义或文件结尾），
// 而不是依赖 updateParallelButton 的相对位置。
const functionStart = source.indexOf("function askAiAboutSelection");
if (functionStart < 0) throw new Error("找不到 askAiAboutSelection 函数");
const bodyStart = source.indexOf("{", functionStart);
const nextTopLevel = source.slice(bodyStart).search(/\nfunction |\nasync function |\nconst [A-Za-z_$][\w$]* = /);
const bodyEnd = nextTopLevel < 0 ? source.length : bodyStart + nextTopLevel;
const implementation = source.slice(functionStart, bodyEnd);

if (implementation.includes("submitAiQuestion") || implementation.includes(".gr-ai-send")) {
  throw new Error("选区问 AI 不应自动发送问题");
}

console.log("选区问 AI 会打开工作区、预填问题并等待用户确认发送");

import { readFile } from "node:fs/promises";

const source = await readFile(new URL("../frontend/src/reader.ts", import.meta.url), "utf8");

for (const marker of [
  'class="gr-ai-stop"',
  "停止请求",
  "async function stopAiTask",
  "/stop`, { method: \"POST\"",
]) {
  if (!source.includes(marker)) throw new Error(`停止 AI 请求缺少实现：${marker}`);
}

console.log("AI 任务运行时提供真正的停止请求操作");

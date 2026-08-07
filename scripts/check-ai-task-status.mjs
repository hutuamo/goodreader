import { readFile } from "node:fs/promises";

const source = await readFile(new URL("../frontend/src/reader.ts", import.meta.url), "utf8");

for (const marker of ["createdAt: number", "已运行 ${elapsed}", "formatDuration(Date.now() - task.createdAt)"]) {
  if (!source.includes(marker)) throw new Error(`AI 任务状态缺少实现：${marker}`);
}

console.log("AI 任务处理中会显示实时已用时间");

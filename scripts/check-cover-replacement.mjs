import { readFile } from "node:fs/promises";

const source = await readFile(new URL("../frontend/src/main.ts", import.meta.url), "utf8");

for (const marker of [
  'data-menu="cover"',
  "替换封面",
  "async function replaceBookCover",
  "/cover`, { method: \"POST\"",
]) {
  if (!source.includes(marker)) throw new Error(`替换封面缺少实现：${marker}`);
}

console.log("书籍菜单提供替换封面并调用本地选图接口");

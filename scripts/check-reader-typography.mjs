import { readFile } from "node:fs/promises";

const [readerSource, readerStyles] = await Promise.all([
  readFile(new URL("../frontend/src/reader.ts", import.meta.url), "utf8"),
  readFile(new URL("../frontend/public/assets/reader.css", import.meta.url), "utf8"),
]);

const requiredSource = [
  "language: string | null",
  "language === \"zh\" || language.startsWith(\"zh-\")",
  "document.documentElement.dataset.goodreaderLanguage = \"zh\"",
  "applyBookLanguage();",
];

const requiredStyles = [
  '[data-goodreader-language="zh"]',
  '"PingFang SC"',
  '"Source Serif 4"',
  ":where(h1, h2, h3, h4, h5, h6)",
];

for (const marker of requiredSource) {
  if (!readerSource.includes(marker)) throw new Error(`阅读器缺少语言规则：${marker}`);
}

for (const marker of requiredStyles) {
  if (!readerStyles.includes(marker)) throw new Error(`阅读器缺少字体规则：${marker}`);
}

console.log("中文书籍字体规则检查通过");

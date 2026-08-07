import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

// settings.ts 是 ESM TypeScript 源；用轻量剥离类型后在 node:test 中直接断言行为。
const source = await readFile(new URL("../frontend/src/settings.ts", import.meta.url), "utf8");
const stripped = source
  .replace(/: number/g, "")
  .replace(/: string \| null \| undefined/g, "")
  .replace(/export /g, "");
const dataUrl = `data:text/javascript,${encodeURIComponent(`${stripped}\nexport { clampNumber, parseClampedSetting };`)}`;
const { clampNumber, parseClampedSetting } = await import(dataUrl);

test("clampNumber 限制上下界", () => {
  assert.equal(clampNumber(50, 80, 160), 80);
  assert.equal(clampNumber(100, 80, 160), 100);
  assert.equal(clampNumber(200, 80, 160), 160);
});

test("parseClampedSetting 对 null 使用 fallback，不会变成 0 再钳到下限", () => {
  assert.equal(parseClampedSetting(null, 80, 160, 100), 100);
  assert.equal(parseClampedSetting(undefined, 80, 160, 100), 100);
  assert.equal(parseClampedSetting("", 80, 160, 100), 100);
  assert.equal(parseClampedSetting("   ", 80, 160, 100), 100);
});

test("parseClampedSetting 拒绝非法数字", () => {
  assert.equal(parseClampedSetting("abc", 80, 160, 100), 100);
  assert.equal(parseClampedSetting("NaN", 80, 160, 100), 100);
});

test("parseClampedSetting 解析并限幅合法数字", () => {
  assert.equal(parseClampedSetting("120", 80, 160, 100), 120);
  assert.equal(parseClampedSetting("50", 80, 160, 100), 80);
  assert.equal(parseClampedSetting("200", 80, 160, 100), 160);
  assert.equal(parseClampedSetting("360", 240, 560, 360), 360);
});

// 确认 reader 启动路径使用 parseClampedSetting 而非 Number(null)
const readerSource = await readFile(new URL("../frontend/src/reader.ts", import.meta.url), "utf8");
test("reader 字号与侧栏宽度走 parseClampedSetting", () => {
  assert.match(readerSource, /parseClampedSetting\(savedFontSize/);
  assert.match(readerSource, /parseClampedSetting\(savedSidebarWidth/);
  assert.match(readerSource, /parseClampedSetting\(savedAiSidebarWidth/);
  assert.doesNotMatch(
    readerSource,
    /const parsedFontSize = Number\(savedFontSize\)/,
  );
});

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

// parseClampedSetting 是 TypeScript 模块，无法被 Node 直接 import，这里从源码
// 提取它的函数体并用 Function 执行，对其真实行为做断言，覆盖缺失值、空串、
// 非有限值、合法值与越界值五类输入。
const source = await readFile(new URL("../frontend/src/settings.ts", import.meta.url), "utf8");
const match = source.match(
  /export function parseClampedSetting\([^)]*\)[^{]*\{([\s\S]*?)\n\}/,
);
if (!match) throw new Error("找不到 parseClampedSetting 实现");
const parseClampedSetting = new Function("value", "fallback", "minimum", "maximum", match[1]);

test("缺失值回落到默认", () => {
  assert.equal(parseClampedSetting(null, 100, 80, 160), 100);
  assert.equal(parseClampedSetting(undefined, 100, 80, 160), 100);
});

test("空串与纯空白回落到默认", () => {
  assert.equal(parseClampedSetting("", 100, 80, 160), 100);
  assert.equal(parseClampedSetting("   ", 100, 80, 160), 100);
});

test("非有限值回落到默认", () => {
  assert.equal(parseClampedSetting("abc", 100, 80, 160), 100);
  assert.equal(parseClampedSetting("NaN", 100, 80, 160), 100);
});

test("合法值原样返回", () => {
  assert.equal(parseClampedSetting("120", 100, 80, 160), 120);
});

test("越界值钳到区间边界", () => {
  assert.equal(parseClampedSetting("0", 100, 80, 160), 80);
  assert.equal(parseClampedSetting("999", 100, 80, 160), 160);
});

const readerSource = await readFile(new URL("../frontend/src/reader.ts", import.meta.url), "utf8");

test("reader 字号与侧栏宽度走 parseClampedSetting", () => {
  assert.match(readerSource, /parseClampedSetting\(savedFontSize/);
  assert.match(readerSource, /parseClampedSetting\(\s*savedSidebarWidth/);
  assert.match(readerSource, /parseClampedSetting\(\s*savedAiSidebarWidth/);
  assert.doesNotMatch(readerSource, /const parsedFontSize = Number\(savedFontSize\)/);
});

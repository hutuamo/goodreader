// P2-10：阅读器纯函数真实行为测试（node:test），替代字符串存在性检查。
// 运行：node scripts/check-reader-utils.mjs
import { test } from "node:test";
import assert from "node:assert/strict";

import { clampNumber, escapeHtml, parseFiniteNumber } from "../frontend/src/reader-utils.mjs";

test("parseFiniteNumber 拒绝 null / 空串 / 非数字", () => {
  assert.equal(parseFiniteNumber(null), null);
  assert.equal(parseFiniteNumber(""), null);
  assert.equal(parseFiniteNumber("abc"), null);
  assert.equal(parseFiniteNumber("1e999"), null); // Infinity
  assert.equal(parseFiniteNumber("   "), null);
});

test("parseFiniteNumber 解析合法数字字符串", () => {
  assert.equal(parseFiniteNumber("100"), 100);
  assert.equal(parseFiniteNumber(" 100 "), 100);
  assert.equal(parseFiniteNumber("0"), 0);
  assert.equal(parseFiniteNumber("-50"), -50);
  assert.equal(parseFiniteNumber("1.5"), 1.5);
});

test("clampNumber 把越界值限制在区间内", () => {
  assert.equal(clampNumber(0, 80, 160), 80);
  assert.equal(clampNumber(200, 80, 160), 160);
  assert.equal(clampNumber(100, 80, 160), 100);
  assert.equal(clampNumber(80, 80, 160), 80);
});

test("escapeHtml 转义全部危险字符", () => {
  assert.equal(escapeHtml('<script>"&\'</script>'), "&lt;script&gt;&quot;&amp;&#039;&lt;/script&gt;");
  assert.equal(escapeHtml("普通文本"), "普通文本");
  assert.equal(escapeHtml(""), "");
});

console.log("阅读器纯函数行为测试通过");

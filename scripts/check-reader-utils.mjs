// P2-10：阅读器纯函数真实行为测试（node:test），替代字符串存在性检查。
// 运行：node scripts/check-reader-utils.mjs
import { test } from "node:test";
import assert from "node:assert/strict";

import { escapeHtml } from "../frontend/src/reader-utils.mjs";

test("escapeHtml 转义全部危险字符", () => {
  assert.equal(escapeHtml('<script>"&\'</script>'), "&lt;script&gt;&quot;&amp;&#039;&lt;/script&gt;");
  assert.equal(escapeHtml("普通文本"), "普通文本");
  assert.equal(escapeHtml(""), "");
});

console.log("阅读器纯函数行为测试通过");

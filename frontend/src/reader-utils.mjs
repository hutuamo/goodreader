// 阅读器与书架共享的纯函数：无 DOM 依赖，可被 node:test 直接做真实行为测试。
// 注意：reader.ts 通过 vite 打包引入本模块；修改行为时同步更新
// scripts/check-reader-utils.mjs 中的用例。

/**
 * 把字符串解析为有限数字，null / 空串 / 非数字一律返回 null。
 * 避免 `Number(null) === 0` 把「未设置」误当 0 后再被 clamp（P2-1）。
 */
export function parseFiniteNumber(value) {
  if (value === null || value === undefined || value.trim() === "") return null;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
}

export function clampNumber(value, minimum, maximum) {
  return Math.max(minimum, Math.min(maximum, value));
}

export function escapeHtml(value) {
  return value.replace(
    /[&<>"']/g,
    (character) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#039;" })[
        character
      ] ?? character,
  );
}

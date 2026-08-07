// 阅读器与书架共享的纯函数：无 DOM 依赖，可被 node:test 直接做真实行为测试。
// 注意：reader.ts 通过 vite 打包引入本模块；修改行为时同步更新
// scripts/check-reader-utils.mjs 中的用例。

export function escapeHtml(value) {
  return value.replace(
    /[&<>"']/g,
    (character) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#039;" })[
        character
      ] ?? character,
  );
}

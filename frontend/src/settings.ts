// 阅读器持久化设置的解析与钳制工具。
//
// 后端 save_setting 只做长度校验，数值范围必须在前端防御，否则缺失值会落入
// Number(null) === 0 的陷阱：Number.isFinite(0) 为真，字号被钳到最小 80%、
// 侧栏宽度变成 0。这里把“读取持久化值 → 钳到合法区间”集中成一处，
// 值缺失、空串或非有限时回到默认值，避免把“未设置”误判为 0。

export function parseClampedSetting(
  value: string | null | undefined,
  fallback: number,
  minimum: number,
  maximum: number,
): number {
  if (value === null || value === undefined || value.trim() === "") return fallback;
  const parsed = Number(value);
  if (!Number.isFinite(parsed)) return fallback;
  return Math.min(Math.max(parsed, minimum), maximum);
}

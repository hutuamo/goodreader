/** 将偏好值解析为有限数字并钳制到区间；null / 空串 / 非法值回落到 fallback。 */
export function clampNumber(value: number, minimum: number, maximum: number): number {
  return Math.max(minimum, Math.min(maximum, value));
}

export function parseClampedSetting(
  raw: string | null | undefined,
  minimum: number,
  maximum: number,
  fallback: number,
): number {
  if (raw == null || raw.trim() === "") {
    return fallback;
  }
  const parsed = Number(raw);
  if (!Number.isFinite(parsed)) {
    return fallback;
  }
  return clampNumber(parsed, minimum, maximum);
}

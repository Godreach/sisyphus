// 构建展示格式化工具（票 B4-T4）：耗时/时间戳/字节数的人读形态。
// 纯函数、无 i18n 依赖（单位用通用缩写，双语文案统一），供列表/详情/日志
// 视图共用。

/** 毫秒耗时 → 人读时长（`1d 2h 3m 4s` / `2m 3s` / `850ms`）。 */
export function formatDuration(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms) || ms < 0) return '-'
  if (ms < 1000) return `${ms}ms`
  const totalSeconds = Math.floor(ms / 1000)
  const days = Math.floor(totalSeconds / 86400)
  const hours = Math.floor((totalSeconds % 86400) / 3600)
  const minutes = Math.floor((totalSeconds % 3600) / 60)
  const seconds = totalSeconds % 60
  const parts: string[] = []
  if (days > 0) parts.push(`${days}d`)
  if (hours > 0) parts.push(`${hours}h`)
  if (minutes > 0) parts.push(`${minutes}m`)
  parts.push(`${seconds}s`)
  return parts.join(' ')
}

/** 相对年龄结构（`relativeAge` 产出；文案侧经 i18n `time.*` 键渲染）。 */
export type RelativeAge = { unit: 'now' | 'min' | 'hour' | 'day'; n: number }

/** Unix 毫秒时间戳 → 相对年龄（刚刚 / n 分钟 / n 小时 / n 天，向零取整）。 */
export function relativeAge(
  ms: number | null | undefined,
  now: number = Date.now(),
): RelativeAge {
  if (ms == null || !Number.isFinite(ms)) return { unit: 'now', n: 0 }
  const diff = Math.max(0, now - ms)
  const minutes = Math.floor(diff / 60_000)
  if (minutes < 1) return { unit: 'now', n: 0 }
  if (minutes < 60) return { unit: 'min', n: minutes }
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return { unit: 'hour', n: hours }
  return { unit: 'day', n: Math.floor(hours / 24) }
}

/** 相对年龄 → i18n 键（`time.justNow` / `time.minutesAgo` / …）。 */
export function relativeAgeKey(age: RelativeAge): string {
  switch (age.unit) {
    case 'min':
      return 'time.minutesAgo'
    case 'hour':
      return 'time.hoursAgo'
    case 'day':
      return 'time.daysAgo'
    default:
      return 'time.justNow'
  }
}

/** Unix 毫秒时间戳 → 本地时间 `YYYY-MM-DD HH:mm:ss`（无时间戳返回 '-'）。 */
export function formatDateTime(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return '-'
  const d = new Date(ms)
  const pad = (n: number) => String(n).padStart(2, '0')
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
    `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
  )
}

/** 字节数 → 人读大小（`48.0 MB`；日志截断标注消费）。 */
export function formatBytes(bytes: number | null | undefined): string {
  if (bytes == null || !Number.isFinite(bytes) || bytes < 0) return '-'
  if (bytes < 1024) return `${bytes} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let value = bytes
  let unit = 'B'
  for (const u of units) {
    if (value < 1024) break
    value /= 1024
    unit = u
  }
  return `${value.toFixed(1)} ${unit}`
}

/** 构建/任务状态是否为进行中（queued/running——详情页据此轮询刷新）。 */
export function isLiveStatus(status: string): boolean {
  return status === 'queued' || status === 'running' || status === 'unknown'
}

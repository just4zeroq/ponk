/** 展示格式化：时间（UTC 毫秒整数，§4.1）与时长。 */

/** 毫秒时间戳 → 本地可读时间；空值返回 `—`。 */
export function fmtTs(ms?: number | null): string {
  if (ms === undefined || ms === null || ms === 0) return '—'
  const d = new Date(ms)
  if (Number.isNaN(d.getTime())) return String(ms)
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`
}

/** 毫秒时长 → `1m 05s` / `850ms`。 */
export function fmtDuration(ms?: number | null): string {
  if (ms === undefined || ms === null) return '—'
  if (ms < 1000) return `${Math.round(ms)}ms`
  const s = Math.floor(ms / 1000)
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  const rest = s % 60
  if (m < 60) return `${m}m ${String(rest).padStart(2, '0')}s`
  const h = Math.floor(m / 60)
  return `${h}h ${String(m % 60).padStart(2, '0')}m`
}

/** 相对现在的时间（如 `3 分钟前`）。 */
export function fmtAgo(ms?: number | null): string {
  if (!ms) return '—'
  const diff = Date.now() - ms
  if (diff < 0) return fmtTs(ms)
  return `${fmtDuration(diff)}前`
}

/** 数字千分位；null → `—`。 */
export function fmtNum(n?: number | null): string {
  if (n === undefined || n === null) return '—'
  return n.toLocaleString('en-US')
}

/** JSON 对象 → 缩进字符串；解析失败原样返回。 */
export function prettyJson(value: unknown, space = 2): string {
  if (typeof value === 'string') {
    try {
      return JSON.stringify(JSON.parse(value), null, space)
    } catch {
      return value
    }
  }
  try {
    return JSON.stringify(value, null, space)
  } catch {
    return String(value)
  }
}

/**
 * 定位 JSON 解析错误的行列（供 JSON 编辑器错误提示）。
 * @returns `{line, column, message}` 或 null（能正常解析）。
 */
export function jsonParseError(text: string): { line: number; column: number; message: string } | null {
  try {
    JSON.parse(text)
    return null
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e)
    // V8：Unexpected token } in JSON at position 42 (line 3 column 5)
    const pos = /position (\d+)/.exec(message)
    if (pos) {
      const idx = Number(pos[1])
      const before = text.slice(0, idx)
      const line = before.split('\n').length
      const column = idx - before.lastIndexOf('\n')
      return { line, column, message }
    }
    const lc = /line (\d+) column (\d+)/.exec(message)
    if (lc) return { line: Number(lc[1]), column: Number(lc[2]), message }
    return { line: 0, column: 0, message }
  }
}

/** 未解析例外条目 → `{entry, reason}`（§4.5 兼容 `[entry, reason]` 元组与对象两种形态）。 */
export function unresolvedParts(row: unknown): { entry: string; reason: string } {
  if (Array.isArray(row)) return { entry: String(row[0] ?? ''), reason: String(row[1] ?? '') }
  if (row && typeof row === 'object') {
    const o = row as Record<string, unknown>
    return { entry: String(o.entry ?? o.name ?? ''), reason: String(o.reason ?? '') }
  }
  return { entry: String(row), reason: '' }
}

/**
 * 币信息页的可写点交互（docs/14 §9 `PATCH /assets/{code}`）。
 *
 * 只有两个字段可写：`kind`（人工兜底）与 `external_id`（手填 CG id）。
 * 手填后**服务端**置 `match_status = matched`（前端不自行改状态，避免与服务端规则分叉）。
 */
import { ElMessageBox } from 'element-plus'

/** 输入校验：CG id 只允许字母 / 数字 / `.` `_` `-`（如 `bitcoin`、`1inch`）。 */
const CG_ID_RE = /^[A-Za-z0-9._-]+$/

/**
 * 弹窗手填 / 修改 CG id。
 *
 * @param code 展示用的币代码
 * @param current 当前 `external_id`
 * @returns `undefined` = 取消；`null` = 清除手填；字符串 = 新的 CG id
 */
export async function promptExternalId(code: string, current?: string | null): Promise<string | null | undefined> {
  let raw: string
  try {
    const res = await ElMessageBox.prompt(
      '填写 CoinGecko coin id（如 bitcoin）。留空 = 清除手填（match_status 交回服务端按三级规则判定，docs/14 §5.2）。',
      `手填 ${code} 的 CG id`,
      {
        inputValue: current ?? '',
        inputPlaceholder: '如 bitcoin / ethereum',
        confirmButtonText: '保存',
        inputValidator: (v: string) => {
          const t = v.trim()
          if (t === '') return true
          return CG_ID_RE.test(t) ? true : 'CG id 只能包含字母 / 数字 / . _ -'
        },
      },
    )
    raw = String(res.value ?? '')
  } catch {
    return undefined
  }
  const t = raw.trim()
  return t === '' ? null : t
}

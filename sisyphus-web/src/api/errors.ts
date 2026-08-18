// 认证/提交类错误的统一展示（B4-T2）：按 ApiError code 分支给 i18n 文案。
// LoginView 与 SetupView 共用同一错误描述逻辑（避免两处复制漂移）。
// 经全局 i18n 实例取文案（本函数在 setup 外被调用，不能用 useI18n）。
//
// B4-T4 扩展：构建动作（触发/取消/重跑）与列表加载的错误描述也收敛到本
// 模块（409 冲突、网络失败、泛化）——与认证面共用「按 code 分支」的纪律，
// 不各自复制分支。

import { i18n } from '@/i18n'
import { NETWORK_ERROR_CODE, ApiError } from '@/api/http'

/** 登录/建号/建条目/建项目类错误按 code 分支的人读文案：
 *  429 限流带剩余秒数、网络失败给 errors.network、422 校验清单拼接、
 *  其它（401/403/409/404 等）直接用后端 message（人读、可展示）。 */
export function describeSubmitError(err: unknown): string {
  const t = i18n.global.t
  if (err instanceof ApiError) {
    if (err.code === 'RATE_LIMITED') {
      const ms = err.retryAfterMs
      return ms != null
        ? t('auth.loginRateLimited', { seconds: Math.ceil(ms / 1000) })
        : t('auth.loginRateLimitedGeneric')
    }
    if (err.code === NETWORK_ERROR_CODE) {
      return t('errors.network')
    }
    if (err.code === 'VALIDATION_FAILED') {
      return err.validationIssues.map((i) => i.message).join('；') || err.message
    }
    return err.message
  }
  return t('errors.generic')
}

/** 构建动作/列表加载类错误按 code 分支的人读文案（票 B4-T4）：
 *  409 重跑冲突给固定文案、网络失败给 errors.network、其它直接用后端
 *  message。供构建详情（触发/取消/重跑）与构建列表（加载失败）共用。 */
export function describeActionError(err: unknown): string {
  const t = i18n.global.t
  if (err instanceof ApiError) {
    if (err.status === 409) return t('buildDetail.rerunConflict')
    if (err.code === NETWORK_ERROR_CODE) return t('errors.network')
    return err.message
  }
  return t('errors.generic')
}

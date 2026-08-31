// 流水线行内动作单一缝（票 #105 定稿行内动作；票 #108 项目详情复用）：
// 最近构建状态 → 动作映射（终止/重试/运行，原型红/橙/蓝）+ API 调用 +
// toast 描述符。流水线页（跨项目清单）与项目详情（本项目清单）共用，
// 动作语义/配色/toast 文案不漂移；无 i18n 依赖（toast key 由页面侧 t()）。

import { buildsApi } from '@/api/client'

/** 行内动作种类：终止（红；排队/运行中）/ 重试（橙；失败终态）/ 运行（蓝；其余）。 */
export type PipelineRowActionKind = 'cancel' | 'rerun' | 'trigger'

export interface PipelineRowAction {
  kind: PipelineRowActionKind
  /** 描边小按钮色类（main.css .btn-outline.*）。 */
  cls: 'red' | 'orange' | 'blue'
  /** 按钮文案 i18n 键（plines.actionCancel / actionRetry / actionRun）。 */
  labelKey: 'plines.actionCancel' | 'plines.actionRetry' | 'plines.actionRun'
}

/** 最近构建状态 → 行内动作（运行中/排队 → 终止；失败 → 重试；其余 → 运行）。 */
export function pipelineRowActionFor(status: string | null | undefined): PipelineRowAction {
  if (status === 'running' || status === 'queued') {
    return { kind: 'cancel', cls: 'red', labelKey: 'plines.actionCancel' }
  }
  if (status === 'failed') {
    return { kind: 'rerun', cls: 'orange', labelKey: 'plines.actionRetry' }
  }
  return { kind: 'trigger', cls: 'blue', labelKey: 'plines.actionRun' }
}

/** toast 描述符（动作落定后的页面侧反馈文案键 + 插值参数）。 */
export type PipelineRowActionToast =
  | { key: 'plines.cancelRequested' | 'plines.rerunRequested' }
  | { key: 'plines.triggered'; params: { n: number } }

/** 执行行内动作（fail 抛给页面侧）；返回 toast 描述符。
 *  - cancel/rerun 需要 `latest`（最近构建号）；
 *  - trigger 以缺省参数触发新构建，返回号入 toast。 */
export async function runPipelineRowAction(
  kind: PipelineRowActionKind,
  target: { project: string; pipeline: string; latest: { number: number } | null },
): Promise<PipelineRowActionToast> {
  if (kind === 'cancel') {
    await buildsApi.cancel(target.project, target.pipeline, target.latest!.number)
    return { key: 'plines.cancelRequested' }
  }
  if (kind === 'rerun') {
    await buildsApi.rerun(target.project, target.pipeline, target.latest!.number, {
      mode: 'from_failed',
    })
    return { key: 'plines.rerunRequested' }
  }
  const accepted = await buildsApi.trigger(target.project, target.pipeline, {})
  return { key: 'plines.triggered', params: { n: accepted.number } }
}

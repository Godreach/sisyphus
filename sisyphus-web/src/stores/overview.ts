// 概览页数据 store（ADR-0019，票 B5-T7）。
//
// 单一数据源 = 概览快照端点 `GET /api/v1/overview`（后端 `api/overview.rs`）：
// stat 卡全量真值 + 三类事实警示态 + 最近构建，任意登录角色可读（ADR-0019
// 双消费——同一份数后端也灌 /metrics）。票 B5-T7 交付后不再组合派生、不再
// 退化标注（B4-T3 的退化面随本票移除）。
//
// - 队列深度 / 构建终态 / 槽位占用 / 磁盘占用 / 最近构建 / 警示态全部来自
//   快照响应——单来源，无逐端点组合。
// - 单来源失败（网络 / 500 等）：整页报错，loadError 置消息，可重试。
// - Agent 在线数不再依赖 `/agents`（全局 admin 专属）——快照是登录即见面，
//   普通用户不再看到「仅管理员可见」退化（B4-T3 退化卡随本票移除）。

import { computed, ref } from 'vue'
import { defineStore } from 'pinia'

import { overviewApi } from '@/api/client'
import { ApiError, type OverviewSnapshotResponse } from '@/api/types'

/** 概览页面数据状态（null = 未加载/加载失败，页面展示错误 + 重试）。 */
export interface OverviewState {
  /** 队列深度（全部 queued 任务）。 */
  queueDepth: number
  /** 队列深度原因分类（保序；与后端固定标签全集对应）。 */
  queueReasons: { reason: string; depth: number }[]
  /** Agent 在线/总数。 */
  agentsOnline: number
  agentsTotal: number
  /** 槽位占用/总量。 */
  slotsUsed: number
  slotsTotal: number
  /** 构建终态计数（四态）。 */
  buildsTerminal: { succeeded: number; failed: number; cancelled: number; timeout: number }
  /** 产物 + 日志字节占用。 */
  artifactBytes: number
  logBytes: number
  /** 事实型警示态（零阈值）。 */
  alerts: {
    hasNoMatch: boolean
    hasOfflineAgent: boolean
    hasDrainingIncompatible: boolean
  }
  /** 最近构建（跨可见项目，按最近活动倒序）。 */
  recentBuilds: {
    project: string
    pipeline: string
    number: number
    status: string
    trigger: string
    startedAt: number | null
    finishedAt: number | null
  }[]
}

export const useOverviewStore = defineStore('overview', () => {
  const loadError = ref('')
  /** 首载中（初始加载 true；重试/刷新期间也置 true，#91 骨架屏）。 */
  const loading = ref(false)
  const state = ref<OverviewState | null>(null)

  /** 有启用但离线的 Agent（事实警示，零阈值；ADR-0019）。 */
  const offlineAlert = computed(() => state.value?.alerts.hasOfflineAgent ?? false)
  /** 存在无匹配 Agent 的任务（事实警示）。 */
  const noMatchAlert = computed(() => state.value?.alerts.hasNoMatch ?? false)
  /** 存在排空/版本不兼容 Agent（事实警示）。 */
  const drainingIncompatibleAlert = computed(
    () => state.value?.alerts.hasDrainingIncompatible ?? false,
  )

  /** 加载概览快照：单来源整页语义——失败置 loadError（可重试），不静默部分值。 */
  async function load(): Promise<void> {
    loadError.value = ''
    loading.value = true
    try {
      const snap = await overviewApi.snapshot()
      state.value = fromSnapshot(snap)
    } catch (err) {
      state.value = null
      loadError.value = err instanceof ApiError ? err.message : String(err)
    } finally {
      loading.value = false
    }
  }

  return {
    loadError,
    loading,
    state,
    offlineAlert,
    noMatchAlert,
    drainingIncompatibleAlert,
    load,
  }
})

/** 快照响应 → 页面状态（字段名从后端蛇形映射到前端驼峰）。 */
function fromSnapshot(snap: OverviewSnapshotResponse): OverviewState {
  return {
    queueDepth: snap.queue_depth,
    queueReasons: snap.queue_reasons,
    agentsOnline: snap.agents_online,
    agentsTotal: snap.agents_total,
    slotsUsed: snap.slots_used,
    slotsTotal: snap.slots_total,
    buildsTerminal: snap.builds_terminal,
    artifactBytes: snap.artifact_bytes,
    logBytes: snap.log_bytes,
    alerts: {
      hasNoMatch: snap.alerts.has_no_match,
      hasOfflineAgent: snap.alerts.has_offline_agent,
      hasDrainingIncompatible: snap.alerts.has_draining_incompatible,
    },
    recentBuilds: snap.recent_builds.map((b) => ({
      project: b.project,
      pipeline: b.pipeline,
      number: b.number,
      status: b.status,
      trigger: b.trigger,
      startedAt: b.started_at,
      finishedAt: b.finished_at,
    })),
  }
}
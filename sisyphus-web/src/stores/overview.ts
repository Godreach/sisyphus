// 概览页数据 store（ADR-0019，票 B4-T3）。
//
// 退化语义（Spec B4 实施决策 2 + 票 B4-T3 明示）：概览快照端点（内部快照 /
// /metrics，ADR-0019 双消费）**尚未交付**，本 store 以现有端点组合派生当前
// 值；不可派生的统计（队列深度 / 构建终态 / 全局最近构建 / 无匹配任务 /
// 排空 / 不兼容 Agent）由页面以静态退化文案显式标注「依赖概览快照端点，
// 尚未交付」——不静默给假值，后端补票后在原卡片接上。
//
// - Agent 在线/总数 + 离线警示：`GET /agents`（全局 admin 专属）——普通
//   用户 403，卡片以「仅全局管理员可见」退化展示，不报错。
// - 项目数：`GET /projects`（可见性过滤）。
// - 任何单个来源失败（网络等）不阻塞整页：可得的继续展示、失败的面保持空。

import { computed, ref } from 'vue'
import { defineStore } from 'pinia'

import { agentsApi, projectsApi } from '@/api/client'
import { ApiError } from '@/api/types'

/** Agent 统计面（`visible=false` = 非全局管理员，列表 403 退化）。 */
export interface AgentStats {
  /** 是否可读 Agent 统计（全局 admin 专属；普通用户 403 → false）。 */
  visible: boolean
  total: number
  online: number
  /** 启用但离线（需关注的事实；停用 Agent 离线是预期态，不计）。 */
  offline: number
}

export const useOverviewStore = defineStore('overview', () => {
  const loadError = ref('')
  const agents = ref<AgentStats | null>(null)
  const projectCount = ref<number | null>(null)

  const offlineAlert = computed(
    () => (agents.value?.visible && agents.value.offline > 0) ?? false,
  )

  /** 加载概览：并行取 Agent 统计（403 退化）+ 项目数。单来源失败不阻塞整页
   *  ——每个来源各自 catch，可得的面照常展示（`Promise.allSettled`）。 */
  async function load(): Promise<void> {
    loadError.value = ''
    const [agentStats, projects] = await Promise.allSettled([loadAgents(), projectsApi.list()])
    agents.value = agentStats.status === 'fulfilled' ? agentStats.value : agents.value
    if (projects.status === 'fulfilled') {
      projectCount.value = projects.value.length
    } else {
      projectCount.value = null
      loadError.value =
        projects.reason instanceof ApiError ? projects.reason.message : String(projects.reason)
    }
  }

  /** Agent 统计：`/agents` 为全局 admin 专属——403（非 admin）→
   *  `visible=false`（卡片以「仅管理员可见」退化）；其它失败（网络等）→
   *  null（不阻塞整页）。 */
  async function loadAgents(): Promise<AgentStats | null> {
    try {
      const list = await agentsApi.list()
      const online = list.filter((a) => a.online).length
      const offline = list.filter((a) => !a.online && !a.disabled).length
      return { visible: true, total: list.length, online, offline }
    } catch (err) {
      if (err instanceof ApiError && err.status === 403) {
        return { visible: false, total: 0, online: 0, offline: 0 }
      }
      return null
    }
  }

  return { loadError, agents, projectCount, offlineAlert, load }
})

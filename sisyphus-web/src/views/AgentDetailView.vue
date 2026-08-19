<script setup lang="ts">
// Agent 详情（ADR-0008/0011/0012/0019，票 B4-T5）：单台 Agent 全貌。
//
// 详情页是「看」的表面（编辑/停用/启用在列表页，ADR-0020 IA 分工）：系统/
// 自定义标签分区、槽位占用、磁盘三口径，加工作区/缓存清理入口。
//
// - 标签分区：系统标签（只读，注册/心跳上报，ADR-0008）+ 自定义标签（只读
//   展示；编辑在列表页）。
// - 槽位占用：`active_jobs` / `max_concurrency`（ADR-0008 中心化计数）。
// - 磁盘三口径（ADR-0019）：卷级 total/free + 缓存占用 + 工作区采样；随
//   心跳上报，`disk_usage` 为空 → 从未上报。
// - 工作区/缓存清理入口（ADR-0011/0012）：清理指令经 gRPC 下发，REST 面暂
//   无清理端点 → 「下发指令」形态 + 动作区占位 + 显式退化标注（端点交付
//   后接上，B4 纯前端消费既有契约、不补后端）。

import { computed, onMounted, ref } from 'vue'
import { useRoute } from 'vue-router'
import { useI18n } from 'vue-i18n'

import { agentsApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import type { AgentResponse } from '@/api/types'
import { agentBadgeState, agentStateClass, agentStateLabelKey } from '@/utils/agentState'
import { formatBytes, formatDateTime } from '@/utils/format'

const { t } = useI18n()
const route = useRoute()

const agentName = computed(() => String(route.params.name ?? ''))
const agent = ref<AgentResponse | null>(null)
const loadError = ref('')
const notFound = ref(false)
/** 403（非全局 admin）→ admin-only 退化态：不渲染详情体（对齐 AgentListView
 *  与 overview store 的 403 退化纪律——Agent 管理面全局 admin 专属）。 */
const adminOnly = ref(false)

onMounted(load)

/** 加载详情（`GET /agents/{name}`，全局 admin 专属）。403 → admin-only 退化；
 *  404 → 不存在；其它失败 → 就地错误。 */
async function load(): Promise<void> {
  loadError.value = ''
  notFound.value = false
  adminOnly.value = false
  try {
    agent.value = await agentsApi.get(agentName.value)
  } catch (err) {
    agent.value = null
    if (err instanceof ApiError && err.status === 403) {
      adminOnly.value = true
    } else if (err instanceof ApiError && err.status === 404) {
      notFound.value = true
    } else {
      loadError.value = describeSubmitError(err)
    }
  }
}
</script>

<template>
  <div class="agent-detail-page">
    <p class="agent-back">
      <router-link :to="{ name: 'agents' }">{{ t('agents.back') }}</router-link>
    </p>

    <h1 class="page-title">{{ agent?.name ?? agentName }}</h1>

    <!-- 403 退化态：仅全局管理员可见（与列表页同纪律）。 -->
    <p v-if="adminOnly" class="form-hint">{{ t('agents.adminOnly') }}</p>
    <p v-else-if="loadError" class="form-error" role="alert">{{ loadError }}</p>
    <p v-else-if="notFound" class="form-error" role="alert">{{ t('agents.notFound') }}</p>

    <template v-else-if="agent">
      <span
        class="agent-state-badge"
        :class="agentStateClass(agentBadgeState(agent))"
      >
        {{ t(agentStateLabelKey(agentBadgeState(agent))) }}
      </span>
      <!-- 排空/不兼容 退化标注（与列表页同款：REST 契约未暴露排空/版本字段，
           徽标今日仅派生 在线/离线/停用）。 -->
      <p class="form-hint">{{ t('agents.statesDegraded') }}</p>

      <dl class="agent-meta-dl">
        <dt>{{ t('agents.lastSeen') }}</dt>
        <dd>{{ agent.last_seen_at ? formatDateTime(agent.last_seen_at) : t('agents.neverSeen') }}</dd>
      </dl>

      <!-- 系统标签（只读，注册/心跳上报）。 -->
      <section class="detail-section">
        <h2>{{ t('agents.systemLabels') }}</h2>
        <div class="label-chips">
          <span
            v-for="label in agent.system_labels"
            :key="label"
            class="label-chip mono"
          >{{ label }}</span>
          <span v-if="agent.system_labels.length === 0" class="form-hint">{{ t('agents.noLabels') }}</span>
        </div>
      </section>

      <!-- 自定义标签（只读展示；编辑在列表页）。 -->
      <section class="detail-section">
        <h2>{{ t('agents.customLabels') }}</h2>
        <div class="label-chips">
          <span
            v-for="label in agent.custom_labels"
            :key="label"
            class="label-chip mono"
          >{{ label }}</span>
          <span v-if="agent.custom_labels.length === 0" class="form-hint">{{ t('agents.noLabels') }}</span>
        </div>
      </section>

      <!-- 槽位占用（ADR-0008 中心化计数）。 -->
      <section class="detail-section">
        <h2>{{ t('agents.slotUsage') }}</h2>
        <p class="agent-slots-detail">
          {{ t('agents.activeJobs') }}: {{ agent.active_jobs }} /
          {{ t('agents.maxConcurrency') }}: {{ agent.max_concurrency }}
        </p>
      </section>

      <!-- 磁盘三口径（ADR-0019）。 -->
      <section class="detail-section">
        <h2>{{ t('agents.disk') }}</h2>
        <template v-if="agent.disk_usage">
          <h3 class="disk-sub">{{ t('agents.diskVolumes') }}</h3>
          <table class="disk-table">
            <thead>
              <tr>
                <th>{{ t('agents.diskMount') }}</th>
                <th>{{ t('agents.diskTotal') }}</th>
                <th>{{ t('agents.diskFree') }}</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(v, i) in agent.disk_usage.volumes" :key="i">
                <td class="mono">{{ v.mount_point }}</td>
                <td>{{ formatBytes(v.total_bytes) }}</td>
                <td>{{ formatBytes(v.free_bytes) }}</td>
              </tr>
            </tbody>
          </table>
          <dl class="disk-dl">
            <dt>{{ t('agents.diskCache') }}</dt>
            <dd>{{ formatBytes(agent.disk_usage.cache_bytes) }}</dd>
            <dt>{{ t('agents.diskWorkspace') }}</dt>
            <dd>{{ formatBytes(agent.disk_usage.workspace_bytes) }}</dd>
          </dl>
        </template>
        <p v-else class="form-hint">{{ t('agents.diskNotReported') }}</p>
      </section>

      <!-- 工作区/缓存清理入口（端点未交付 → 形态搭好 + 动作区占位 + 退化标注）。 -->
      <section class="detail-section">
        <h2>{{ t('agents.cleanup') }}</h2>
        <div class="cleanup-block">
          <p>{{ t('agents.cleanupWorkspace') }}</p>
          <button
            type="button"
            class="btn-secondary cleanup-action"
            disabled
            :title="t('agents.cleanupUnavailable')"
          >
            {{ t('agents.cleanupAction') }}
          </button>
        </div>
        <div class="cleanup-block">
          <p>{{ t('agents.cleanupCache') }}</p>
          <button
            type="button"
            class="btn-secondary cleanup-action"
            disabled
            :title="t('agents.cleanupUnavailable')"
          >
            {{ t('agents.cleanupAction') }}
          </button>
        </div>
        <p class="form-hint">{{ t('agents.cleanupHint') }}</p>
        <p class="form-hint cleanup-unavailable">{{ t('agents.cleanupUnavailable') }}</p>
      </section>
    </template>
  </div>
</template>

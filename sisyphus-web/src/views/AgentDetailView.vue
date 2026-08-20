<script setup lang="ts">
// Agent 详情（ADR-0008/0011/0012/0019，票 B4-T5/B5-T4）：单台 Agent 全貌。
//
// 详情页是「看」的表面（编辑/停用/启用在列表页，ADR-0020 IA 分工）：系统/
// 自定义标签分区、槽位占用、磁盘三口径，加工作区/缓存清理入口（B5-T4 起经
// 通道往返真实下发——列表经 send_and_await 往返、清理/删除 fire-and-forget）。
//
// - 标签分区：系统标签（只读，注册/心跳上报，ADR-0008）+ 自定义标签（只读
//   展示；编辑在列表页）。
// - 槽位占用：`active_jobs` / `max_concurrency`（ADR-0008 中心化计数）。
// - 磁盘三口径（ADR-0019）：卷级 total/free + 缓存占用 + 工作区采样。
// - 工作区/缓存清理（ADR-0011/0012）：per-Agent 经 gRPC 通道转发 Agent 侧
//   既有指令。离线 Agent → 409；列表超时 → 504。

import { computed, onMounted, ref } from 'vue'
import { useRoute } from 'vue-router'
import { useI18n } from 'vue-i18n'

import { agentsApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import type { AgentResponse, CacheEntry, WorkspaceEntry } from '@/api/types'
import { agentBadgeState, agentStateClass, agentStateLabelKey } from '@/utils/agentState'
import { formatBytes, formatDateTime } from '@/utils/format'

const { t } = useI18n()
const route = useRoute()

const agentName = computed(() => String(route.params.name ?? ''))
const agent = ref<AgentResponse | null>(null)
const loadError = ref('')
const notFound = ref(false)
/** 403（非全局 admin）→ admin-only 退化态：不渲染详情体。 */
const adminOnly = ref(false)

/** 工作区列表 + 清理表单。 */
const workspace = ref<WorkspaceEntry[] | null>(null)
const wsPipeline = ref('')
const wsJob = ref('')
/** 缓存列表 + 删除表单。 */
const cache = ref<CacheEntry[] | null>(null)
const cacheKey = ref('')
/** 清理面反馈（成功/错误）。 */
const cleanupMsg = ref('')

onMounted(load)

/** 加载详情（`GET /agents/{name}`，全局 admin 专属）。 */
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

/** 查询工作区列表（经通道往返）。 */
async function loadWorkspace(): Promise<void> {
  cleanupMsg.value = ''
  try {
    const res = await agentsApi.listWorkspace(agentName.value)
    workspace.value = res.entries
  } catch (err) {
    workspace.value = null
    cleanupMsg.value = describeSubmitError(err)
  }
}

/** 清理工作区（fire-and-forget）。不自动刷新列表（清理无 ack，刷新另按查询）。 */
async function cleanWorkspace(): Promise<void> {
  cleanupMsg.value = ''
  try {
    await agentsApi.cleanWorkspace(agentName.value, {
      pipeline: wsPipeline.value || null,
      job: wsJob.value || null,
    })
    cleanupMsg.value = t('agents.wsCleanDone')
  } catch (err) {
    cleanupMsg.value = describeSubmitError(err)
  }
}

/** 查询缓存列表（经通道往返）。 */
async function loadCache(): Promise<void> {
  cleanupMsg.value = ''
  try {
    const res = await agentsApi.listCache(agentName.value)
    cache.value = res.entries
  } catch (err) {
    cache.value = null
    cleanupMsg.value = describeSubmitError(err)
  }
}

/** 删除缓存（fire-and-forget）。不自动刷新列表。 */
async function deleteCache(): Promise<void> {
  cleanupMsg.value = ''
  try {
    await agentsApi.deleteCache(agentName.value, { key: cacheKey.value || null })
    cleanupMsg.value = t('agents.cacheDeleteDone')
  } catch (err) {
    cleanupMsg.value = describeSubmitError(err)
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
      <span class="agent-state-badge" :class="agentStateClass(agentBadgeState(agent))">
        {{ t(agentStateLabelKey(agentBadgeState(agent))) }}
      </span>

      <dl class="agent-meta-dl">
        <dt>{{ t('agents.lastSeen') }}</dt>
        <dd>{{ agent.last_seen_at ? formatDateTime(agent.last_seen_at) : t('agents.neverSeen') }}</dd>
        <dt v-if="agent.agent_version">{{ t('agents.version') }}</dt>
        <dd v-if="agent.agent_version">
          {{ agent.agent_version.major }}.{{ agent.agent_version.minor }}.{{ agent.agent_version.patch }}
        </dd>
      </dl>

      <!-- 系统标签（只读，注册/心跳上报）。 -->
      <section class="detail-section">
        <h2>{{ t('agents.systemLabels') }}</h2>
        <div class="label-chips">
          <span v-for="label in agent.system_labels" :key="label" class="label-chip mono">{{ label }}</span>
          <span v-if="agent.system_labels.length === 0" class="form-hint">{{ t('agents.noLabels') }}</span>
        </div>
      </section>

      <!-- 自定义标签（只读展示；编辑在列表页）。 -->
      <section class="detail-section">
        <h2>{{ t('agents.customLabels') }}</h2>
        <div class="label-chips">
          <span v-for="label in agent.custom_labels" :key="label" class="label-chip mono">{{ label }}</span>
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

      <!-- 工作区 / 缓存清理（经通道转发 Agent 侧既有指令）。 -->
      <section class="detail-section">
        <h2>{{ t('agents.cleanup') }}</h2>

        <!-- 工作区。 -->
        <div class="cleanup-block">
          <h3 class="cleanup-sub">{{ t('agents.cleanupWorkspace') }}</h3>
          <button type="button" class="btn-secondary cleanup-action" name="ws-list" @click="loadWorkspace">
            {{ t('agents.wsList') }}
          </button>
          <table v-if="workspace && workspace.length > 0" class="cleanup-table">
            <thead>
              <tr>
                <th>{{ t('agents.wsColPipeline') }}</th>
                <th>{{ t('agents.wsColJob') }}</th>
                <th>{{ t('agents.wsColPath') }}</th>
                <th>{{ t('agents.wsColLastUsed') }}</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(e, i) in workspace" :key="i">
                <td class="mono">{{ e.pipeline }}</td>
                <td class="mono">{{ e.job }}</td>
                <td class="mono">{{ e.path }}</td>
                <td>{{ formatDateTime(e.last_used_at_ms) }}</td>
              </tr>
            </tbody>
          </table>
          <p v-else-if="workspace && workspace.length === 0" class="form-hint">{{ t('agents.wsEmpty') }}</p>
          <div class="cleanup-form">
            <input v-model="wsPipeline" name="ws-pipeline" :placeholder="t('agents.wsPipelinePlaceholder')" />
            <input v-model="wsJob" name="ws-job" :placeholder="t('agents.wsJobPlaceholder')" />
            <button type="button" class="btn-secondary cleanup-action" name="ws-clean" @click="cleanWorkspace">
              {{ t('agents.cleanupAction') }}
            </button>
          </div>
        </div>

        <!-- 缓存。 -->
        <div class="cleanup-block">
          <h3 class="cleanup-sub">{{ t('agents.cleanupCache') }}</h3>
          <button type="button" class="btn-secondary cleanup-action" name="cache-list" @click="loadCache">
            {{ t('agents.cacheList') }}
          </button>
          <table v-if="cache && cache.length > 0" class="cleanup-table">
            <thead>
              <tr>
                <th>{{ t('agents.cacheColKey') }}</th>
                <th>{{ t('agents.cacheColPipeline') }}</th>
                <th>{{ t('agents.cacheColSize') }}</th>
                <th>{{ t('agents.cacheColLastUsed') }}</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(e, i) in cache" :key="i">
                <td class="mono">{{ e.key }}</td>
                <td class="mono">{{ e.pipeline }}</td>
                <td>{{ formatBytes(e.size_bytes) }}</td>
                <td>{{ formatDateTime(e.last_used_at_ms) }}</td>
              </tr>
            </tbody>
          </table>
          <p v-else-if="cache && cache.length === 0" class="form-hint">{{ t('agents.cacheEmpty') }}</p>
          <div class="cleanup-form">
            <input v-model="cacheKey" name="cache-key" :placeholder="t('agents.cacheKeyPlaceholder')" />
            <button type="button" class="btn-secondary cleanup-action" name="cache-delete" @click="deleteCache">
              {{ t('agents.cacheDeleteAction') }}
            </button>
          </div>
        </div>

        <p class="form-hint">{{ t('agents.cleanupHint') }}</p>
        <p v-if="cleanupMsg" class="form-hint">{{ cleanupMsg }}</p>
      </section>
    </template>
  </div>
</template>

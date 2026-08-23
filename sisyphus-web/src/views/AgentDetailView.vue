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
// #94: 使用 Naive UI 组件重写——元信息/标签改 NDescriptions、槽位与磁盘
// 数值改 NStatistic、卷级/工作区/缓存列表改 NDataTable、清理/删除危险操作
// 改 NPopconfirm 确认；视觉与 #84/#86 主题一致。

import { computed, onMounted, ref } from 'vue'
import { useRoute } from 'vue-router'
import { useI18n } from 'vue-i18n'
import {
  NAlert,
  NButton,
  NCard,
  NDataTable,
  NDescriptions,
  NDescriptionsItem,
  NInput,
  NPopconfirm,
  NSkeleton,
  NStatistic,
  NTag,
  type DataTableColumns,
} from 'naive-ui'

import { agentsApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import type { AgentResponse, CacheEntry, WorkspaceEntry } from '@/api/types'
import {
  agentBadgeState,
  agentStateLabelKey,
  agentStateTagType,
} from '@/utils/agentState'
import { formatBytes, formatDateTime } from '@/utils/format'

const { t } = useI18n()
const route = useRoute()

const agentName = computed(() => String(route.params.name ?? ''))
const agent = ref<AgentResponse | null>(null)
const loading = ref(true)
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
  } finally {
    loading.value = false
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

/** Agent 版本（major.minor.patch）。 */
const agentVersion = computed(() => {
  const v = agent.value?.agent_version
  return v ? `${v.major}.${v.minor}.${v.patch}` : null
})

/** 徽标态（单次派生，头部 NTag 复用）。 */
const badgeState = computed(() => (agent.value ? agentBadgeState(agent.value) : null))

/** 卷级磁盘表列（挂载点 / 总量 / 剩余）。 */
const volumeColumns = computed<
  DataTableColumns<{ mount_point: string; total_bytes: number; free_bytes: number }>
>(() => [
  {
    title: t('agents.diskMount'),
    key: 'mount_point',
    render: (v) => v.mount_point,
  },
  {
    title: t('agents.diskTotal'),
    key: 'total_bytes',
    render: (v) => formatBytes(v.total_bytes),
  },
  {
    title: t('agents.diskFree'),
    key: 'free_bytes',
    render: (v) => formatBytes(v.free_bytes),
  },
])

/** 工作区列表列（pipeline / 任务 / 路径 / 最近使用）。 */
const workspaceColumns = computed<DataTableColumns<WorkspaceEntry>>(() => [
  { title: t('agents.wsColPipeline'), key: 'pipeline' },
  { title: t('agents.wsColJob'), key: 'job' },
  { title: t('agents.wsColPath'), key: 'path' },
  {
    title: t('agents.wsColLastUsed'),
    key: 'last_used_at_ms',
    render: (e) => formatDateTime(e.last_used_at_ms),
  },
])

/** 缓存列表列（key / pipeline / 大小 / 最近使用）。 */
const cacheColumns = computed<DataTableColumns<CacheEntry>>(() => [
  { title: t('agents.cacheColKey'), key: 'key' },
  { title: t('agents.cacheColPipeline'), key: 'pipeline' },
  { title: t('agents.cacheColSize'), key: 'size_bytes', render: (e) => formatBytes(e.size_bytes) },
  {
    title: t('agents.cacheColLastUsed'),
    key: 'last_used_at_ms',
    render: (e) => formatDateTime(e.last_used_at_ms),
  },
])

const volumeRowKey = (v: { mount_point: string }): string => v.mount_point
const workspaceRowKey = (e: WorkspaceEntry): string => e.path
const cacheRowKey = (e: CacheEntry): string => e.key
</script>

<template>
  <div class="agent-detail-page">
    <p class="agent-back">
      <router-link :to="{ name: 'agents' }">{{ t('agents.back') }}</router-link>
    </p>

    <div class="agent-detail-header">
      <h1 class="page-title">{{ agent?.name ?? agentName }}</h1>
      <n-tag
        v-if="badgeState"
        size="small"
        :type="agentStateTagType(badgeState)"
        :bordered="false"
        class="agent-state-tag"
      >
        {{ t(agentStateLabelKey(badgeState)) }}
      </n-tag>
    </div>

    <!-- 403 退化态：仅全局管理员可见（与列表页同纪律）。 -->
    <p v-if="adminOnly" class="form-hint">{{ t('agents.adminOnly') }}</p>
    <n-alert v-else-if="loadError" type="error" :title="loadError" role="alert" />
    <n-alert v-else-if="notFound" type="error" :title="t('agents.notFound')" role="alert" />

    <!-- 首载骨架屏（与列表页同纪律——数据到达后替换）。 -->
    <div v-else-if="loading" class="agent-detail-skeleton" data-testid="agent-detail-skeleton">
      <n-skeleton text :repeat="4" height="32px" />
    </div>

    <template v-else-if="agent">
      <!-- 元信息 + 标签分区（NDescriptions：系统标签只读，注册/心跳上报；
           自定义标签只读展示，编辑在列表页）。 -->
      <n-descriptions :column="2" size="small" bordered>
        <n-descriptions-item :label="t('agents.lastSeen')" :span="agentVersion ? 1 : 2">
          {{ agent.last_seen_at ? formatDateTime(agent.last_seen_at) : t('agents.neverSeen') }}
        </n-descriptions-item>
        <n-descriptions-item v-if="agentVersion" :label="t('agents.version')">
          {{ agentVersion }}
        </n-descriptions-item>
        <n-descriptions-item :label="t('agents.systemLabels')" :span="2">
          <div class="label-chips">
            <n-tag v-for="label in agent.system_labels" :key="label" size="small" class="label-chip">
              {{ label }}
            </n-tag>
            <span v-if="agent.system_labels.length === 0" class="form-hint">{{ t('agents.noLabels') }}</span>
          </div>
        </n-descriptions-item>
        <n-descriptions-item :label="t('agents.customLabels')" :span="2">
          <div class="label-chips">
            <n-tag v-for="label in agent.custom_labels" :key="label" size="small" class="label-chip">
              {{ label }}
            </n-tag>
            <span v-if="agent.custom_labels.length === 0" class="form-hint">{{ t('agents.noLabels') }}</span>
          </div>
        </n-descriptions-item>
      </n-descriptions>

      <!-- 槽位占用（ADR-0008 中心化计数，NStatistic）。 -->
      <section class="detail-section">
        <h2>{{ t('agents.slotUsage') }}</h2>
        <div class="agent-stat-grid">
          <n-card size="small" class="agent-stat-card">
            <n-statistic :label="t('agents.activeJobs')">
              <span class="stat-value">{{ agent.active_jobs }}</span>
            </n-statistic>
          </n-card>
          <n-card size="small" class="agent-stat-card">
            <n-statistic :label="t('agents.maxConcurrency')">
              <span class="stat-value">{{ agent.max_concurrency }}</span>
            </n-statistic>
          </n-card>
        </div>
      </section>

      <!-- 磁盘三口径（ADR-0019：卷级 total/free + 缓存占用 + 工作区采样，
           数值 NStatistic）。 -->
      <section class="detail-section">
        <h2>{{ t('agents.disk') }}</h2>
        <div class="agent-stat-grid">
          <n-card size="small" class="agent-stat-card">
            <n-statistic :label="t('agents.diskCache')">
              <span class="stat-value">
                {{ agent.disk_usage ? formatBytes(agent.disk_usage.cache_bytes) : '—' }}
              </span>
            </n-statistic>
          </n-card>
          <n-card size="small" class="agent-stat-card">
            <n-statistic :label="t('agents.diskWorkspace')">
              <span class="stat-value">
                {{ agent.disk_usage ? formatBytes(agent.disk_usage.workspace_bytes) : '—' }}
              </span>
            </n-statistic>
          </n-card>
        </div>

        <!-- 卷级 total/free（ADR-0019；未上报 → 退化标注）。 -->
        <n-data-table
          v-if="agent.disk_usage"
          :columns="volumeColumns"
          :data="agent.disk_usage.volumes"
          :row-key="volumeRowKey"
          :bordered="false"
          :single-line="true"
          size="small"
          :scroll-x="480"
          class="agent-volume-table"
        />
        <p v-else class="form-hint">{{ t('agents.diskNotReported') }}</p>
      </section>

      <!-- 工作区 / 缓存清理（经通道转发 Agent 侧既有指令；危险操作 NPopconfirm）。 -->
      <section class="detail-section">
        <h2>{{ t('agents.cleanup') }}</h2>

        <!-- 工作区。 -->
        <n-card size="small" class="cleanup-card">
          <template #header>{{ t('agents.cleanupWorkspace') }}</template>
          <template #header-extra>
            <n-button size="small" name="ws-list" @click="loadWorkspace">
              {{ t('agents.wsList') }}
            </n-button>
          </template>
          <n-data-table
            v-if="workspace && workspace.length > 0"
            :columns="workspaceColumns"
            :data="workspace"
            :row-key="workspaceRowKey"
            :bordered="false"
            :single-line="true"
            size="small"
            :scroll-x="640"
          />
          <p v-else-if="workspace && workspace.length === 0" class="form-hint">{{ t('agents.wsEmpty') }}</p>
          <div class="cleanup-form">
            <n-input
              v-model:value="wsPipeline"
              :input-props="{ name: 'ws-pipeline' }"
              :placeholder="t('agents.wsPipelinePlaceholder')"
            />
            <n-input
              v-model:value="wsJob"
              :input-props="{ name: 'ws-job' }"
              :placeholder="t('agents.wsJobPlaceholder')"
            />
            <n-popconfirm
              :positive-text="t('common.confirm')"
              :negative-text="t('common.cancel')"
              @positive-click="cleanWorkspace"
            >
              <template #trigger>
                <n-button type="error" secondary name="ws-clean">
                  {{ t('agents.cleanupAction') }}
                </n-button>
              </template>
              {{ t('agents.wsCleanConfirm') }}
            </n-popconfirm>
          </div>
        </n-card>

        <!-- 缓存。 -->
        <n-card size="small" class="cleanup-card">
          <template #header>{{ t('agents.cleanupCache') }}</template>
          <template #header-extra>
            <n-button size="small" name="cache-list" @click="loadCache">
              {{ t('agents.cacheList') }}
            </n-button>
          </template>
          <n-data-table
            v-if="cache && cache.length > 0"
            :columns="cacheColumns"
            :data="cache"
            :row-key="cacheRowKey"
            :bordered="false"
            :single-line="true"
            size="small"
            :scroll-x="640"
          />
          <p v-else-if="cache && cache.length === 0" class="form-hint">{{ t('agents.cacheEmpty') }}</p>
          <div class="cleanup-form">
            <n-input
              v-model:value="cacheKey"
              :input-props="{ name: 'cache-key' }"
              :placeholder="t('agents.cacheKeyPlaceholder')"
            />
            <n-popconfirm
              :positive-text="t('common.confirm')"
              :negative-text="t('common.cancel')"
              @positive-click="deleteCache"
            >
              <template #trigger>
                <n-button type="error" secondary name="cache-delete">
                  {{ t('agents.cacheDeleteAction') }}
                </n-button>
              </template>
              {{ t('agents.cacheDeleteConfirm') }}
            </n-popconfirm>
          </div>
        </n-card>

        <p class="form-hint">{{ t('agents.cleanupHint') }}</p>
        <p v-if="cleanupMsg" class="form-hint">{{ cleanupMsg }}</p>
      </section>
    </template>
  </div>
</template>

<style scoped>
.agent-detail-page {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.agent-back {
  margin: 0 0 4px;
  font-size: 13px;
}

.agent-back a {
  color: var(--sisy-color-primary);
  text-decoration: none;
}

.agent-back a:hover {
  text-decoration: underline;
}

.agent-detail-header {
  display: flex;
  align-items: center;
  gap: 10px;
}

.agent-state-tag {
  flex-shrink: 0;
}

.detail-section {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.detail-section h2 {
  margin: 0;
  font-size: 16px;
}

.agent-stat-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
  gap: 12px;
}

.stat-value {
  font-size: 20px;
  font-weight: 600;
}

.label-chips {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

.label-chip {
  font-family: ui-monospace, 'Cascadia Code', Consolas, monospace;
}

.cleanup-card {
  margin-bottom: 4px;
}

.cleanup-form {
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
  margin-top: 8px;
}

.cleanup-form .n-input {
  width: 220px;
}
</style>

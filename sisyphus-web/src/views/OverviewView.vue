<script setup lang="ts">
// 概览页（ADR-0019，票 B5-T7）：stat 卡 + 事实型警示态 + 最近构建。
// #91: 使用 Naive UI 组件重写——NCard 统计卡（图标 + 数值层级）、NTag 队列
// 原因分类、NAlert 警示态（类型匹配严重程度）、NDataTable 最近构建（可排
// 序列）、NSkeleton 首载骨架屏；视觉与 #84/#86 主题一致。
//
// 数据源（票 B5-T7 交付后单一来源）：概览快照端点 `GET /api/v1/overview`——
// 队列深度（原因分类）/ Agent 在线与总数 / 槽位占用 / 构建终态计数 / 产物与
// 日志占用 / 三类事实警示态 / 最近构建，任意登录角色可读。B4-T3 的退化标注
// （依赖快照端点未交付）随本票移除。
//
// - 单来源整页语义：快照失败 → loadError 报错（NAlert + 重试），不静默部分值。
// - 首载（loading）展示 NSkeleton 骨架屏，数据到达后替换（#91 AC）。
// - 警示态只展示「事实」（ADR-0019：零阈值）：无匹配任务 / 有离线 Agent /
//   排空或不兼容 Agent——全部来自快照响应的 alerts 字段。
// - 时间 / 字节数人读形态复用 `@/utils/format`（与构建列表/详情同纪律）。

import { computed, h, onMounted } from 'vue'
import { useI18n } from 'vue-i18n'
import { NAlert, NButton, NCard, NDataTable, NIcon, NSkeleton, NStatistic, NTag, NText } from 'naive-ui'
import type { DataTableColumns } from 'naive-ui'
import {
  Server,
  Grid,
  Hourglass,
  CheckmarkDone,
  CloudDone,
  RefreshOutline,
} from '@vicons/ionicons5'

import { useOverviewStore } from '@/stores/overview'
import { formatBytes, formatDateTime } from '@/utils/format'
import type { RecentBuildDto } from '@/api/types'

const { t } = useI18n()
const overview = useOverviewStore()

onMounted(() => {
  void overview.load()
})

/** 队列原因 → 人读标签键（与后端 snapshot::classify 固定标签全集对应）。 */
function queueReasonKey(reason: string): string {
  return `overview.queueReason.${reason}`
}

/** 队列原因 → NTag 状态色（不同原因不同颜色，瓶颈类型一目了然）。 */
function queueReasonType(reason: string): 'warning' | 'error' | 'info' | 'default' {
  switch (reason) {
    case 'no_online_agent':
      return 'error'
    case 'missing_labels':
      return 'warning'
    case 'no_slot':
      return 'info'
    default:
      return 'default'
  }
}

/** 构建状态 → 人读标签键（复用 buildStatus.*，与构建列表/详情同纪律）。 */
function buildStatusKey(status: string): string {
  return `buildStatus.${status}`
}

/** 构建状态 → NTag 状态色（成功=绿 / 失败=红 / 运行=蓝 / 取消=灰等）。 */
function buildStatusType(status: string): 'success' | 'error' | 'info' | 'warning' | 'default' {
  switch (status) {
    case 'succeeded':
      return 'success'
    case 'failed':
      return 'error'
    case 'running':
      return 'info'
    case 'queued':
      return 'warning'
    default:
      return 'default'
  }
}

/** 触发源 → 人读标签键（复用 triggerSource.*）。 */
function triggerKey(trigger: string): string {
  return `triggerSource.${trigger}`
}

/** stat 卡定义：图标 + 标签 + 数值（统一 NCard + NStatistic 形态）。
 *  builds 卡为多终态（成功/失败/取消/超时），数值在模板内用 NTag 渲染，
 *  故 value 为空。 */
interface StatCard {
  key: 'agents' | 'slots' | 'queue' | 'builds' | 'storage'
  icon: ReturnType<typeof import('vue').defineComponent>
  label: string
  value?: () => string
}

const statCards = computed<StatCard[]>(() => {
  if (overview.state == null) return []
  const s = overview.state
  return [
    {
      key: 'agents',
      icon: Server,
      label: t('overview.agentsOnline'),
      value: () => `${s.agentsOnline} / ${s.agentsTotal}`,
    },
    {
      key: 'slots',
      icon: Grid,
      label: t('overview.slots'),
      value: () => `${s.slotsUsed} / ${s.slotsTotal}`,
    },
    {
      key: 'queue',
      icon: Hourglass,
      label: t('overview.queueDepth'),
      value: () => `${s.queueDepth}`,
    },
    {
      key: 'builds',
      icon: CheckmarkDone,
      label: t('overview.buildOutcomes'),
    },
    {
      key: 'storage',
      icon: CloudDone,
      label: t('overview.storage'),
      value: () => `${formatBytes(s.artifactBytes)} + ${formatBytes(s.logBytes)}`,
    },
  ]
})

/** 构建终态 → NTag 状态色（与最近构建状态列同色系，主题 Token 驱动）。 */
function buildTerminalType(
  outcome: 'succeeded' | 'failed' | 'cancelled' | 'timeout',
): 'success' | 'error' | 'default' | 'warning' {
  switch (outcome) {
    case 'succeeded':
      return 'success'
    case 'failed':
      return 'error'
    case 'cancelled':
      return 'default'
    case 'timeout':
      return 'warning'
  }
}

/** 构建终态计数（模板 v-for 渲染 NTag 数值层级）。 */
const buildTerminalOutcomes = computed<{ key: 'succeeded' | 'failed' | 'cancelled' | 'timeout'; label: string }[]>(
  () => [
    { key: 'succeeded', label: t('overview.outcomeSucceeded') },
    { key: 'failed', label: t('overview.outcomeFailed') },
    { key: 'cancelled', label: t('overview.outcomeCancelled') },
    { key: 'timeout', label: t('overview.outcomeTimeout') },
  ],
)

/** 最近构建 NDataTable 列（按列可排序，NDataTable 内建客户端排序）。 */
const recentBuildColumns = computed<DataTableColumns<RecentBuildDto>>(() => [
  {
    title: t('overview.colProject'),
    key: 'project',
  },
  {
    title: t('overview.colPipeline'),
    key: 'pipeline',
  },
  {
    title: t('overview.colBuild'),
    key: 'number',
    sorter: 'default',
    render: (row) => `#${row.number}`,
  },
  {
    title: t('overview.colStatus'),
    key: 'status',
    sorter: (a, b) => a.status.localeCompare(b.status),
    render: (row) =>
      h(
        NTag,
        { size: 'small', type: buildStatusType(row.status), bordered: false },
        { default: () => t(buildStatusKey(row.status)) },
      ),
  },
  {
    title: t('overview.colTrigger'),
    key: 'trigger',
    render: (row) => t(triggerKey(row.trigger)),
  },
  {
    title: t('overview.colFinished'),
    key: 'finished_at',
    sorter: (a, b) =>
      (a.finished_at ?? a.started_at ?? 0) - (b.finished_at ?? b.started_at ?? 0),
    render: (row) => formatDateTime(row.finished_at ?? row.started_at),
  },
])

const recentBuildRowKey = (row: RecentBuildDto): string =>
  `${row.project}-${row.pipeline}-${row.number}`
</script>

<template>
  <div class="overview-page">
    <h1 class="page-title">{{ t('routes.overview') }}</h1>

    <!-- 快照失败：整页报错 + 重试（NAlert type=error）。 -->
    <n-alert
      v-if="overview.loadError"
      type="error"
      :title="overview.loadError"
      role="alert"
      class="overview-error-alert"
      data-testid="overview-error"
    >
      <n-button size="small" @click="overview.load()">
        <template #icon>
          <n-icon :component="RefreshOutline" />
        </template>
        {{ t('overview.retry') }}
      </n-button>
    </n-alert>

    <!-- 首载骨架屏（#91 AC：数据到达后替换）。 -->
    <div v-if="overview.loading && overview.state == null" class="overview-skeleton" data-testid="overview-skeleton">
      <div class="stat-grid">
        <n-skeleton v-for="i in 5" :key="i" text :repeat="3" height="40px" class="overview-skeleton-card" />
      </div>
      <n-skeleton text :repeat="2" height="28px" class="overview-skeleton-section" />
      <n-skeleton text :repeat="4" height="32px" class="overview-skeleton-table" />
    </div>

    <template v-if="overview.state != null">
      <!-- stat 卡（ADR-0019：只展示当前值，无历史曲线；NCard + 图标 + NStatistic）。 -->
      <section class="stat-grid" aria-label="stat cards">
        <n-card
          v-for="card in statCards"
          :key="card.key"
          class="stat-card"
          size="small"
          :bordered="true"
        >
          <n-statistic :label="card.label">
            <template #prefix>
              <n-icon :component="card.icon" class="overview-stat-icon" />
            </template>
            <template #default>
              <span v-if="card.key === 'builds'" class="build-outcomes">
                <n-tag
                  v-for="o in buildTerminalOutcomes"
                  :key="o.key"
                  :type="buildTerminalType(o.key)"
                  size="small"
                  :bordered="false"
                >
                  {{ o.label }} {{ overview.state.buildsTerminal[o.key] }}
                </n-tag>
              </span>
              <span v-else class="stat-value">{{ card.value?.() }}</span>
            </template>
          </n-statistic>
        </n-card>
      </section>

      <!-- 队列原因分类（有任务等待时给出「卡在哪」的事实；NTag 状态色）。 -->
      <section v-if="overview.state.queueReasons.length > 0" class="queue-reasons">
        <h2>{{ t('overview.queueReasons') }}</h2>
        <div class="queue-reason-list">
          <n-tag
            v-for="r in overview.state.queueReasons"
            :key="r.reason"
            :type="queueReasonType(r.reason)"
            size="medium"
            round
          >
            {{ t(queueReasonKey(r.reason)) }} · {{ r.depth }}
          </n-tag>
        </div>
      </section>

      <!-- 事实型警示态（ADR-0019：事实判断、零阈值配置；NAlert 类型匹配严重程度）。 -->
      <section class="alerts" aria-label="alerts">
        <n-alert
          v-if="overview.offlineAlert"
          type="warning"
          :title="t('overview.alertAgentsOffline')"
          role="alert"
          class="overview-alert"
        />
        <n-alert
          v-if="overview.noMatchAlert"
          type="warning"
          :title="t('overview.alertNoMatch')"
          role="alert"
          class="overview-alert"
        />
        <n-alert
          v-if="overview.drainingIncompatibleAlert"
          type="warning"
          :title="t('overview.alertDrainingIncompatible')"
          role="alert"
          class="overview-alert"
        />
        <!-- 尚未注册 Agent：信息性提示（NAlert type=info，非警示严重程度）。 -->
        <n-alert
          v-if="overview.state.agentsTotal === 0"
          type="info"
          :title="t('overview.alertNoAgents')"
          class="overview-alert"
        />
      </section>

      <!-- 最近构建（NDataTable 可排序列；空态文案）。 -->
      <section class="recent-builds">
        <h2>{{ t('overview.recentBuilds') }}</h2>
        <n-data-table
          v-if="overview.state.recentBuilds.length > 0"
          :columns="recentBuildColumns"
          :data="overview.state.recentBuilds"
          :row-key="recentBuildRowKey"
          :bordered="false"
          :single-line="true"
          size="small"
          :scroll-x="720"
          class="recent-builds-table"
        />
        <div v-else class="recent-builds-empty">
          <n-text depth="3">{{ t('overview.recentBuildsEmpty') }}</n-text>
        </div>
      </section>
    </template>
  </div>
</template>

<style scoped>
.stat-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
  gap: 12px;
  margin: 16px 0;
}

.stat-card {
  /* NCard 自带边框/背景（主题 Token）；此层只保证卡内高度与视觉层级。 */
  height: 100%;
}

.stat-value {
  font-size: 22px;
  font-weight: 600;
}

.build-outcomes {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

.overview-stat-icon {
  font-size: 18px;
}

.overview-skeleton {
  display: flex;
  flex-direction: column;
  gap: 16px;
  margin: 16px 0;
}

.overview-skeleton-card {
  width: 100%;
}

.overview-skeleton-section {
  margin-top: 8px;
}

.overview-skeleton-table {
  margin-top: 8px;
}

.queue-reason-list {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  margin: 8px 0 16px;
}

.alerts {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin: 8px 0 20px;
}

.recent-builds-empty {
  color: var(--n-text-color-3, #999);
  padding: 12px 0;
}
</style>

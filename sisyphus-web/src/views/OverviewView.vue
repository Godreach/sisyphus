<script setup lang="ts">
// 工作台（原型页一，spec #99，ADR-0019 数据面不变）：指标卡 + 最近构建表
// + 右栏（Agent 健康 / 收藏的流水线）。
//
// 数据源仍是概览快照端点 `GET /api/v1/overview`（单一来源，任意登录角色可
// 读）：指标卡（在途任务 = 槽位占用；构建 = 终态合计；队列深度；在线构建
// 机）+ 最近构建（含排队/运行中动态构建，契约票 #104）。
//
// 右栏「收藏的流水线」（W8 裁定，契约票 #104：用户级收藏端点契约先行 +
// mock）：条目 = 项目 / 流水线名 + 最近构建状态徽章 + 运行按钮 + 取消收藏；
// 无收藏时引导去流水线页（收藏入口随票 #105 落地）。运行成功后刷新概览
// 快照与收藏列表（W2：新构建即时可见，在途/队列计数真实变化）。
//
// - 快照失败 → loadError 报错（NAlert + 重试）；首载 NSkeleton 骨架屏。
// - 原型无对应数据的字段（分支/触发人）不造假：契约无分支字段，收藏条目
//   以「项目 / 流水线名」区分（fixture 的 main/release 名即分支口径）。

import { computed, onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'
import { NAlert, NButton, NIcon, NSkeleton, NText, useMessage } from 'naive-ui'
import { RefreshOutline, Star } from '@vicons/ionicons5'

import { useOverviewStore } from '@/stores/overview'
import { buildsApi, favoritesApi } from '@/api/client'
import { describeActionError } from '@/api/errors'
import type { PipelineFavoriteResponse } from '@/api/types'
import { formatDuration, relativeAge, relativeAgeKey } from '@/utils/format'

/** 最近构建行（overview store 已把 API 蛇形字段映射为驼峰）。 */
interface RecentBuildRow {
  project: string
  pipeline: string
  number: number
  status: string
  trigger: string
  startedAt: number | null
  finishedAt: number | null
}

const { t } = useI18n()
const router = useRouter()
const message = useMessage()
const overview = useOverviewStore()

/** 队列原因 → 人读标签键（与后端 snapshot::classify 固定标签全集对应）。 */
function queueReasonKey(reason: string): string {
  return `overview.queueReason.${reason}`
}

// ===== 指标卡（原型 metric-card 形态：值 + 单位 + 副标） =====

const inflightPct = computed(() => {
  const s = overview.state
  if (!s || s.slotsTotal === 0) return 0
  return Math.round((s.slotsUsed / s.slotsTotal) * 100)
})

const buildsTotal = computed(() => {
  const s = overview.state
  if (!s) return 0
  return (
    s.buildsTerminal.succeeded +
    s.buildsTerminal.failed +
    s.buildsTerminal.cancelled +
    s.buildsTerminal.timeout
  )
})

/** 全部启用 Agent 在线（Agent 健康卡数值转绿）。 */
const agentsAllOnline = computed(() => {
  const s = overview.state
  return !!s && s.agentsTotal > 0 && s.agentsOnline === s.agentsTotal
})

/** 队列卡副标：有排队给首要原因，否则空闲（零阈值事实，ADR-0019）。 */
const queueSub = computed(() => {
  const s = overview.state
  if (!s) return ''
  if (s.queueDepth === 0) return t('overview.subIdle')
  const top = s.queueReasons[0]
  return top ? t('overview.queueTop', { reason: t(queueReasonKey(top.reason)) }) : t('overview.subIdle')
})

// ===== 最近构建表 =====

/** 构建状态 → 原型徽章类（蓝=运行/排队、绿=成功、红=失败、橙=超时、灰=取消）。 */
function statusBadgeClass(status: string): string {
  switch (status) {
    case 'running':
      return 'running'
    case 'queued':
      return 'info'
    case 'succeeded':
      return 'success'
    case 'failed':
      return 'failed'
    case 'timeout':
      return 'warning'
    default:
      return 'neutral'
  }
}

function buildStatusKey(status: string): string {
  return `buildStatus.${status}`
}

function triggerKey(trigger: string): string {
  return `triggerSource.${trigger}`
}

/** 构建耗时：终态 = finished-started；运行中 = now-started；未运行 = null。 */
function buildDuration(row: RecentBuildRow): number | null {
  if (row.startedAt == null) return null
  const end = row.finishedAt ?? Date.now()
  return Math.max(0, end - row.startedAt)
}

function relativeTimeText(ms: number | null): string {
  const age = relativeAge(ms)
  return t(relativeAgeKey(age), { n: age.n })
}

function openBuild(row: RecentBuildRow): void {
  void router.push({
    name: 'build-detail',
    params: { name: row.project, pipeline: row.pipeline, number: String(row.number) },
  })
}

// ===== Agent 健康卡（指标卡内嵌零阈值事实徽章：只亮异常，全正常收一枚） =====

interface HealthRow {
  key: string
  /** 卡内短标签（完整事实句入 title 提示）。 */
  short: string
  full: string
  issue: boolean
}

const healthRows = computed<HealthRow[]>(() => {
  const s = overview.state
  if (!s) return []
  return [
    {
      key: 'offline',
      short: t('overview.healthShortOffline'),
      full: t('overview.alertAgentsOffline'),
      issue: s.alerts.hasOfflineAgent,
    },
    {
      key: 'no-match',
      short: t('overview.healthShortNoMatch'),
      full: t('overview.alertNoMatch'),
      issue: s.alerts.hasNoMatch,
    },
    {
      key: 'draining',
      short: t('overview.healthShortDraining'),
      full: t('overview.alertDrainingIncompatible'),
      issue: s.alerts.hasDrainingIncompatible,
    },
  ]
})

/** 异常事实（健康卡只亮这些；无异常时显示「全部正常」一枚）。 */
const healthIssues = computed(() => healthRows.value.filter((r) => r.issue))

// ===== 右栏：收藏的流水线（W8 裁定；契约票 #104 收藏端点） =====

const favorites = ref<PipelineFavoriteResponse[]>([])
const favoritesLoading = ref(false)
const favoritesError = ref('')

async function loadFavorites(): Promise<void> {
  favoritesError.value = ''
  favoritesLoading.value = true
  try {
    favorites.value = await favoritesApi.list()
  } catch (err) {
    favoritesError.value = err instanceof Error ? err.message : String(err)
  } finally {
    favoritesLoading.value = false
  }
}

function openPipelineBuilds(project: string, pipeline: string): void {
  void router.push({
    name: 'build-list',
    params: { name: project, pipeline },
  })
}

/** 直触手动触发（缺省参数；runner 档之外 403 就地 toast）。 */
const triggering = ref(false)

async function triggerPipeline(project: string, pipeline: string): Promise<void> {
  if (triggering.value) return
  triggering.value = true
  try {
    const accepted = await buildsApi.trigger(project, pipeline, {})
    message.success(t('plines.triggered', { n: accepted.number }))
    // W2：触发成功后刷新概览快照与收藏列表——新构建（排队/运行中动态态）
    // 即时可见，在途/队列计数真实变化，不再等手动刷新。
    void overview.load()
    void loadFavorites()
  } catch (err) {
    message.error(describeActionError(err))
  } finally {
    triggering.value = false
  }
}

/** 取消收藏（行内操作：仅被点行禁用，成功后重载清单 + toast 可感知）。 */
const unfavoriting = ref<string | null>(null)

async function unfavorite(project: string, pipeline: string): Promise<void> {
  const key = `${project}/${pipeline}`
  if (unfavoriting.value != null) return
  unfavoriting.value = key
  try {
    await favoritesApi.remove(project, pipeline)
    message.success(t('overview.unfavorited'))
    await loadFavorites()
  } catch (err) {
    message.error(describeActionError(err))
  } finally {
    unfavoriting.value = null
  }
}

onMounted(() => {
  void overview.load()
  void loadFavorites()
})
</script>

<template>
  <div class="workbench-page">
    <!-- 快照失败：整页报错 + 重试（NAlert type=error）。 -->
    <n-alert
      v-if="overview.loadError"
      type="error"
      :title="overview.loadError"
      role="alert"
      class="workbench-error"
      data-testid="overview-error"
    >
      <n-button size="small" @click="overview.load()">
        <template #icon>
          <n-icon :component="RefreshOutline" />
        </template>
        {{ t('overview.retry') }}
      </n-button>
    </n-alert>

    <!-- 首载骨架屏（数据到达后替换）。 -->
    <div v-if="overview.loading && overview.state == null" class="workbench-skeleton" data-testid="overview-skeleton">
      <div class="metric-row">
        <n-skeleton v-for="i in 4" :key="i" text :repeat="3" height="40px" class="workbench-skeleton-card" />
      </div>
      <n-skeleton text :repeat="4" height="32px" class="workbench-skeleton-table" />
    </div>

    <template v-if="overview.state != null">
      <!-- 指标卡行（原型 metrics-row）。 -->
      <section class="metric-row" aria-label="metrics">
        <div class="metric-card">
          <span class="metric-label">{{ t('overview.metricInflight') }}</span>
          <span class="metric-value">
            {{ overview.state.slotsUsed }}<span class="unit">/ {{ overview.state.slotsTotal }}</span>
          </span>
          <span class="metric-sub">{{ t('overview.subUsageRate', { pct: inflightPct }) }}</span>
        </div>
        <div class="metric-card">
          <span class="metric-label">{{ t('overview.metricBuilds') }}</span>
          <span class="metric-value">
            {{ buildsTotal }}<span class="unit">{{ t('overview.unitTimes') }}</span>
          </span>
          <span class="metric-sub green">
            {{ t('overview.subSuccessFail', { ok: overview.state.buildsTerminal.succeeded, ng: overview.state.buildsTerminal.failed }) }}
          </span>
        </div>
        <div class="metric-card">
          <span class="metric-label">{{ t('overview.metricQueue') }}</span>
          <span class="metric-value">
            {{ overview.state.queueDepth }}<span class="unit">{{ t('overview.unitTasks') }}</span>
          </span>
          <span class="metric-sub">{{ queueSub }}</span>
        </div>
        <!-- Agent 健康卡（与系统指标同行）：在线比 + 三类零阈值事实徽章。 -->
        <div class="metric-card health-card">
          <span class="metric-label">{{ t('overview.agentHealth') }}</span>
          <span class="metric-value" :class="{ green: agentsAllOnline }">
            {{ overview.state.agentsOnline }}<span class="unit">/ {{ overview.state.agentsTotal }}{{ t('overview.unitAgents') }}</span>
          </span>
          <span v-if="overview.state.agentsTotal === 0" class="metric-sub">
            {{ t('overview.noAgentsRow') }}
          </span>
          <span v-else class="health-badges">
            <span
              v-for="row in healthIssues"
              :key="row.key"
              class="badge failed"
              :title="row.full"
            >
              {{ row.short }}
            </span>
            <span v-if="healthIssues.length === 0" class="badge success">
              {{ t('overview.healthAllOk') }}
            </span>
          </span>
        </div>
      </section>

      <div class="dash-main">
        <!-- 最近构建（原型 runs-card）。 -->
        <section class="sisy-card runs-card" aria-label="recent builds">
          <div class="card-header">
            <h2 class="card-title">{{ t('overview.recentRuns') }}</h2>
            <router-link class="card-link" :to="{ name: 'pipelines' }">
              {{ t('overview.viewPipelines') }}
            </router-link>
          </div>
          <div class="runs-head">
            <span class="col-name">{{ t('overview.colName') }}</span>
            <span class="col-status">{{ t('overview.colStatus') }}</span>
            <span class="col-trigger">{{ t('overview.colTrigger') }}</span>
            <span class="col-duration">{{ t('overview.colDuration') }}</span>
            <span class="col-time">{{ t('overview.colTime') }}</span>
          </div>
          <div v-if="overview.state.recentBuilds.length > 0" class="runs-body">
            <button
              v-for="row in overview.state.recentBuilds"
              :key="`${row.project}-${row.pipeline}-${row.number}`"
              type="button"
              class="run-row"
              @click="openBuild(row)"
            >
              <span class="col-name">
                <span class="run-name">{{ row.pipeline }}<span class="build-no">#{{ row.number }}</span></span>
                <span class="run-meta">{{ row.project }}</span>
              </span>
              <span class="col-status">
                <span class="badge" :class="statusBadgeClass(row.status)">{{ t(buildStatusKey(row.status)) }}</span>
              </span>
              <span class="col-trigger">{{ t(triggerKey(row.trigger)) }}</span>
              <span class="col-duration">{{ formatDuration(buildDuration(row)) }}</span>
              <span class="col-time">{{ relativeTimeText(row.finishedAt ?? row.startedAt) }}</span>
            </button>
          </div>
          <div v-else class="runs-empty">
            <n-text depth="3">{{ t('overview.recentBuildsEmpty') }}</n-text>
          </div>
        </section>

        <!-- 右栏：收藏的流水线（W8 裁定；无收藏引导去流水线页）。 -->
        <aside class="dash-right">
          <section class="sisy-card" aria-label="favorite pipelines">
            <div class="card-header">
              <h2 class="card-title">{{ t('overview.favPipelines') }}</h2>
            </div>
            <!-- 收藏清单加载失败：卡内报错 + 重试（不拖垮整页）。 -->
            <div v-if="favoritesError" class="runs-empty">
              <n-text type="error" data-testid="fav-error">{{ favoritesError }}</n-text>
              <n-button size="small" class="fav-retry" @click="loadFavorites">
                <template #icon>
                  <n-icon :component="RefreshOutline" />
                </template>
                {{ t('overview.retry') }}
              </n-button>
            </div>
            <template v-else-if="favoritesLoading">
              <div class="fav-list">
                <n-skeleton v-for="i in 2" :key="i" text height="40px" class="fav-skeleton" />
              </div>
            </template>
            <div v-else-if="favorites.length > 0" class="fav-list">
              <div v-for="p in favorites" :key="`${p.project}-${p.pipeline}`" class="fav-row">
                <button type="button" class="fav-name" @click="openPipelineBuilds(p.project, p.pipeline)">
                  <span class="fav-title">
                    {{ p.pipeline }}
                    <span v-if="p.latest_build" class="badge" :class="statusBadgeClass(p.latest_build.status)">
                      {{ t(buildStatusKey(p.latest_build.status)) }}
                    </span>
                    <span v-else class="badge neutral">{{ t('plines.noRun') }}</span>
                  </span>
                  <span class="sub">{{ p.project }}</span>
                </button>
                <button
                  type="button"
                  class="btn-outline blue"
                  :disabled="triggering"
                  @click="triggerPipeline(p.project, p.pipeline)"
                >
                  {{ t('overview.run') }}
                </button>
                <button
                  type="button"
                  class="fav-remove"
                  :title="t('overview.unfavorite')"
                  :aria-label="t('overview.unfavorite')"
                  :disabled="unfavoriting === `${p.project}/${p.pipeline}`"
                  @click="unfavorite(p.project, p.pipeline)"
                >
                  <n-icon :component="Star" />
                </button>
              </div>
            </div>
            <div v-else class="runs-empty fav-empty">
              <n-text depth="3">{{ t('overview.favEmpty') }}</n-text>
              <n-text depth="3" class="fav-empty-hint">{{ t('overview.favEmptyHint') }}</n-text>
              <router-link class="fav-empty-link" :to="{ name: 'pipelines' }">
                {{ t('overview.goPipelines') }}
              </router-link>
            </div>
          </section>
        </aside>
      </div>
    </template>
  </div>
</template>

<style scoped>
.workbench-page {
  display: flex;
  flex-direction: column;
  gap: 20px;
}

.workbench-error {
  margin-bottom: 4px;
}

.workbench-skeleton {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.workbench-skeleton-card {
  width: 100%;
}

/* 主区：左表 + 右栏（原型 dash-main）。 */
.dash-main {
  display: flex;
  gap: 20px;
  align-items: flex-start;
}

.runs-card {
  flex: 1;
  min-width: 0;
}

/* 表头（原型 table-head）。 */
.runs-head {
  display: flex;
  align-items: center;
  padding: 0 20px;
  height: 40px;
  border-top: 1px solid var(--sisy-color-border);
  border-bottom: 1px solid var(--sisy-color-border);
}

.runs-head span {
  font-size: 12px;
  font-weight: 500;
  color: var(--sisy-color-text-secondary);
}

.runs-body {
  display: flex;
  flex-direction: column;
}

.run-row {
  display: flex;
  align-items: center;
  padding: 0 20px;
  min-height: 64px;
  border: none;
  border-bottom: 1px solid var(--sisy-color-border-light);
  background: none;
  font-family: inherit;
  text-align: left;
  cursor: pointer;
  transition: background 0.15s;
  width: 100%;
}

.run-row:last-child {
  border-bottom: none;
}

.run-row:hover {
  background: var(--sisy-color-bg);
}

.col-name {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 3px;
}

.run-name {
  font-size: 13px;
  font-weight: 600;
  color: var(--sisy-color-text);
  display: flex;
  align-items: baseline;
  gap: 6px;
}

.run-name .build-no {
  font-size: 11px;
  font-weight: 400;
  color: var(--sisy-color-text-secondary);
}

.run-meta {
  font-size: 11px;
  color: var(--sisy-color-text-secondary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.col-status {
  width: 84px;
  display: flex;
}

.col-trigger {
  width: 80px;
  font-size: 13px;
  color: var(--sisy-color-text);
}

.col-duration {
  width: 80px;
  font-size: 13px;
  color: var(--sisy-color-text);
}

.col-time {
  width: 96px;
  font-size: 12px;
  color: var(--sisy-color-text-secondary);
}

.runs-empty {
  padding: 24px 20px;
}

/* 右栏（原型 dash-right：320px 双卡）。 */
.dash-right {
  width: 320px;
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  gap: 20px;
}

/* Agent 健康卡：事实徽章行（紧凑胶囊，完整句入 title）。 */
.health-badges {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

.fav-list {
  padding: 4px 20px 16px;
}

.fav-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  min-height: 56px;
  border-bottom: 1px solid var(--sisy-color-border-light);
}

.fav-row:last-child {
  border-bottom: none;
}

.fav-name {
  flex: 1;
  min-width: 0;
  border: none;
  background: none;
  padding: 0;
  cursor: pointer;
  text-align: left;
  font-family: inherit;
  font-size: 13px;
  font-weight: 600;
  color: var(--sisy-color-text);
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.fav-name:hover {
  color: var(--sisy-color-primary);
}

/* 标题行：流水线名 + 最近构建状态徽章（W1/W8：条目可区分、状态可见）。 */
.fav-title {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}

.fav-title .badge {
  flex-shrink: 0;
}

.fav-name .sub {
  font-size: 11px;
  font-weight: 400;
  color: var(--sisy-color-text-secondary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* 取消收藏（行内图标按钮；默认弱化，hover 点亮）。 */
.fav-remove {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  border: none;
  border-radius: var(--sisy-radius-small);
  background: none;
  color: var(--sisy-color-warning);
  cursor: pointer;
  flex-shrink: 0;
  transition: background 0.15s;
}

.fav-remove:hover {
  background: var(--sisy-color-bg);
}

.fav-remove:disabled {
  opacity: 0.5;
  cursor: default;
}

.fav-retry {
  margin-top: 8px;
}

/* 空态：引导去流水线页收藏（W8 定稿：不回退展示最近流水线）。 */
.fav-empty {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 6px;
}

.fav-empty-hint {
  font-size: 12px;
}

.fav-empty-link {
  font-size: 13px;
  color: var(--sisy-color-primary);
  text-decoration: none;
}

.fav-empty-link:hover {
  text-decoration: underline;
}

.fav-skeleton {
  width: 100%;
  margin-bottom: 8px;
}

/* 窄屏：右栏换行到主表下方。 */
@media (max-width: 1024px) {
  .dash-main {
    flex-direction: column;
  }

  .dash-right {
    width: 100%;
  }
}

/* 平板档（~768px，侧栏未折叠）：最近构建表降级次要列——时间列最先收起，
   触发列随之（G2：列挤压不可读；桌面档不受影响）。 */
@media (max-width: 900px) {
  .runs-head .col-time,
  .run-row .col-time {
    display: none;
  }
}

@media (max-width: 780px) {
  .runs-head .col-trigger,
  .run-row .col-trigger {
    display: none;
  }
}
</style>

<script setup lang="ts">
// 流水线页（原型页二，spec #99）：跨项目流水线列表——筛选 Chips（全部/
// 运行中/失败/成功，含计数）+ 列表/卡片双视图 + 排序 + 行内动作。
//
// 数据（就近填充）：
// - 项目清单 `GET /projects` + 逐项目探测 pipeline 名（main/release，与项目
//   详情页同一降级口径，显式标注）+ 概览快照 recent_builds 的跨项目对
//   （非致命，失败跳过）。
// - 每对 (project, pipeline) 调统计端点 `GET …/stats?window=20`（契约票
//   #102）：成功率/平均耗时/构建总数/最近一条构建由服务端聚合；窗口内无
//   终态构建时 success_rate / avg_duration_ms 为 null → 显示「—」。
// - 行内动作按最近构建状态映射：运行中/排队 → 终止（红）；失败 → 重试
//   （橙）；其余 → 运行（蓝），走既有 cancel / rerun / trigger API。
//
// 顶栏搜索（`?q=`）按流水线/项目名过滤；视图偏好仅会话内保持。

import { computed, onMounted, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'
import { NAlert, NDropdown, NEmpty, NSkeleton, useMessage } from 'naive-ui'

import { buildsApi, overviewApi, pipelinesApi, projectsApi } from '@/api/client'
import { describeActionError, describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import { formatDuration, relativeAge, relativeAgeKey } from '@/utils/format'
import type { LatestBuildRef } from '@/api/types'

const { t } = useI18n()
const route = useRoute()
const router = useRouter()
const message = useMessage()

/** 统计窗口（服务端聚合口径，契约票 #102）。 */
const WINDOW = 20
/** pipeline 名探测清单（与 ProjectDetailView 同一口径；端点交付后换真列表）。 */
const PROBE_NAMES = ['main', 'release']

interface PipelineRow {
  project: string
  pipeline: string
  /** 最近一条构建（stats 端点 latest_build，任意状态）；从未运行为 null。 */
  latest: LatestBuildRef | null
  /** 该 pipeline 构建总数（服务端统计，不受窗口限制）。 */
  total: number
  /** 窗口内终态成功率（服务端一位小数；无终态为 null → 「—」）。 */
  rate: string | null
  /** 窗口内终态平均耗时毫秒（无样本为 null → 「—」）。 */
  avgMs: number | null
}

const rows = ref<PipelineRow[]>([])
const loading = ref(true)
const loadError = ref('')

/** 动作进行中（按行标记，按钮转圈/禁用）。 */
const actingKey = ref('')

type ChipKey = 'all' | 'running' | 'failed' | 'success'
const activeChip = ref<ChipKey>('all')
type SortKey = 'recent' | 'name'
const sortKey = ref<SortKey>('recent')
const viewMode = ref<'list' | 'cards'>('list')

/** 顶栏搜索（`?q=`，App 壳写入）。 */
const searchQuery = computed(() => (typeof route.query.q === 'string' ? route.query.q.trim() : ''))

const rowKeyOf = (row: { project: string; pipeline: string }): string => `${row.project}/${row.pipeline}`

onMounted(load)

async function load(): Promise<void> {
  loading.value = true
  loadError.value = ''
  try {
    const projects = await projectsApi.list()

    // 逐项目探测 pipeline 名（200 = 存在；404 = 不存在；其它失败不当事实）。
    const probed = await Promise.all(
      projects.flatMap((p) =>
        PROBE_NAMES.map(async (pipeline) => {
          try {
            await projectsApi.getPipeline(p.name, pipeline)
            return { project: p.name, pipeline, exists: true }
          } catch (err) {
            return { project: p.name, pipeline, exists: err instanceof ApiError && err.status === 404 }
          }
        }),
      ),
    )

    const pairMap = new Map<string, { project: string; pipeline: string }>()
    for (const r of probed) {
      if (r.exists) pairMap.set(`${r.project}/${r.pipeline}`, { project: r.project, pipeline: r.pipeline })
    }
    // 概览快照的最近构建对并入（探不住但跑过的 pipeline；快照失败非致命）。
    try {
      const snap = await overviewApi.snapshot()
      for (const b of snap.recent_builds) {
        pairMap.set(`${b.project}/${b.pipeline}`, { project: b.project, pipeline: b.pipeline })
      }
    } catch {
      // 快照不可达时本页仍可用（少了「跑过但探不住」的对）。
    }

    const pairs = [...pairMap.values()]
    const loaded = await Promise.all(pairs.map((pair) => loadRow(pair)))
    rows.value = sortRows(loaded)
  } catch (err) {
    rows.value = []
    loadError.value = describeSubmitError(err)
  } finally {
    loading.value = false
  }
}

/** 单对 (project, pipeline) → 行数据；404/403 = 无可见运行记录。 */
async function loadRow(pair: { project: string; pipeline: string }): Promise<PipelineRow> {
  try {
    const stats = await pipelinesApi.stats(pair.project, pair.pipeline, WINDOW)
    return {
      project: pair.project,
      pipeline: pair.pipeline,
      latest: stats.latest_build,
      total: stats.total_builds,
      rate: stats.success_rate != null ? `${stats.success_rate}%` : null,
      avgMs: stats.avg_duration_ms,
    }
  } catch {
    // 清单在、统计不可见（404）或权限不足（403）：按「未运行」行展示。
    return { project: pair.project, pipeline: pair.pipeline, latest: null, total: 0, rate: null, avgMs: null }
  }
}

function sortRows(list: PipelineRow[]): PipelineRow[] {
  const sorted = [...list]
  if (sortKey.value === 'name') {
    sorted.sort((a, b) => `${a.project}/${a.pipeline}`.localeCompare(`${b.project}/${b.pipeline}`))
    return sorted
  }
  const ts = (row: PipelineRow): number =>
    row.latest?.finished_at ?? row.latest?.started_at ?? 0
  sorted.sort((a, b) => ts(b) - ts(a))
  return sorted
}

function changeSort(key: SortKey): void {
  sortKey.value = key
  rows.value = sortRows(rows.value)
}

const sortOptions = computed(() => [
  { label: t('plines.sortRecent'), key: 'recent' as const },
  { label: t('plines.sortName'), key: 'name' as const },
])

// ===== Chips 过滤（计数 + 单选） =====

/** chip 命中：运行中含排队（未终态都算「在跑」）；失败只算 failed。 */
function chipHit(row: PipelineRow, chip: ChipKey): boolean {
  if (chip === 'all') return true
  const status = row.latest?.status
  if (chip === 'running') return status === 'running' || status === 'queued'
  if (chip === 'failed') return status === 'failed'
  return status === 'succeeded'
}

const chipCounts = computed<Record<ChipKey, number>>(() => {
  const counts: Record<ChipKey, number> = { all: rows.value.length, running: 0, failed: 0, success: 0 }
  for (const row of rows.value) {
    if (chipHit(row, 'running')) counts.running += 1
    if (chipHit(row, 'failed')) counts.failed += 1
    if (chipHit(row, 'success')) counts.success += 1
  }
  return counts
})

const chipDefs = computed<{ key: ChipKey; label: string }[]>(() => [
  { key: 'all', label: t('plines.chipAll') },
  { key: 'running', label: t('plines.chipRunning') },
  { key: 'failed', label: t('plines.chipFailed') },
  { key: 'success', label: t('plines.chipSuccess') },
])

const visibleRows = computed(() => {
  const q = searchQuery.value.toLowerCase()
  return rows.value.filter((row) => {
    if (!chipHit(row, activeChip.value)) return false
    if (q === '') return true
    return (
      row.pipeline.toLowerCase().includes(q) || row.project.toLowerCase().includes(q)
    )
  })
})

// ===== 展示形态 =====

function statusBadgeClass(status: string | undefined): string {
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
    case undefined:
      return 'neutral'
    default:
      return 'neutral'
  }
}

function statusLabel(status: string | undefined): string {
  return status === undefined ? t('plines.noRun') : t(`buildStatus.${status}`)
}

function triggerLabel(row: PipelineRow): string {
  return row.latest ? t(`triggerSource.${row.latest.trigger}`) : '—'
}

function latestRunText(row: PipelineRow): string {
  if (!row.latest) return t('plines.noRun')
  const age = relativeAge(row.latest.finished_at ?? row.latest.started_at)
  return `#${row.latest.number} · ${t(relativeAgeKey(age), { n: age.n })}`
}

interface RowAction {
  label: string
  cls: string
  run: (row: PipelineRow) => Promise<void>
}

/** 最近构建状态 → 行内动作（终止/重试/运行，原型红/橙/蓝）。 */
function actionFor(row: PipelineRow): RowAction {
  const status = row.latest?.status
  if (status === 'running' || status === 'queued') {
    return {
      label: t('plines.actionCancel'),
      cls: 'red',
      run: async (r) => {
        await buildsApi.cancel(r.project, r.pipeline, r.latest!.number)
        message.success(t('plines.cancelRequested'))
      },
    }
  }
  if (status === 'failed') {
    return {
      label: t('plines.actionRetry'),
      cls: 'orange',
      run: async (r) => {
        await buildsApi.rerun(r.project, r.pipeline, r.latest!.number, { mode: 'from_failed' })
        message.success(t('plines.rerunRequested'))
      },
    }
  }
  return {
    label: t('plines.actionRun'),
    cls: 'blue',
    run: async (r) => {
      const accepted = await buildsApi.trigger(r.project, r.pipeline, {})
      message.success(t('plines.triggered', { n: accepted.number }))
    },
  }
}

async function runAction(row: PipelineRow): Promise<void> {
  const key = rowKeyOf(row)
  if (actingKey.value === key) return
  actingKey.value = key
  try {
    await actionFor(row).run(row)
    // 动作落定后刷新该行（状态/计数就近更新）。
    const fresh = await loadRow(row)
    rows.value = sortRows(rows.value.map((r) => (rowKeyOf(r) === key ? fresh : r)))
  } catch (err) {
    message.error(describeActionError(err))
  } finally {
    actingKey.value = ''
  }
}

function openPipeline(row: PipelineRow): void {
  void router.push({
    name: 'build-list',
    params: { name: row.project, pipeline: row.pipeline },
  })
}

const hasAny = computed(() => rows.value.length > 0)
</script>

<template>
  <div class="plines-page">
    <n-alert
      v-if="loadError"
      type="error"
      :title="loadError"
      role="alert"
      class="plines-error"
    />

    <!-- 首载骨架屏（与概览/构建列表同纪律）。 -->
    <div v-if="loading && !loadError" class="plines-skeleton" data-testid="pipelines-skeleton">
      <n-skeleton text :repeat="1" height="32px" class="plines-skeleton-row" />
      <n-skeleton text :repeat="6" height="44px" class="plines-skeleton-row" />
    </div>

    <template v-else-if="!loadError">
      <div v-if="!hasAny" class="plines-empty">
        <n-empty :description="t('plines.empty')">
          <template #extra>
            <p class="form-hint">{{ t('plines.emptyHint') }}</p>
          </template>
        </n-empty>
      </div>

      <template v-else>
        <!-- 工具栏：筛选 Chips + 视图切换 + 排序（原型 toolbar-row）。 -->
        <div class="toolbar-row">
          <div class="filter-chips" role="tablist">
            <button
              v-for="chip in chipDefs"
              :key="chip.key"
              type="button"
              class="chip"
              :class="{ active: activeChip === chip.key }"
              :data-testid="`chip-${chip.key}`"
              @click="activeChip = chip.key"
            >
              {{ chip.label }}
              <span class="count">{{ chipCounts[chip.key] }}</span>
            </button>
          </div>
          <div class="toolbar-right">
            <div class="view-toggle">
              <button
                type="button"
                class="vt-btn"
                :class="{ active: viewMode === 'list' }"
                data-testid="view-list-btn"
                @click="viewMode = 'list'"
              >
                <svg width="12" height="12" viewBox="0 0 12 12" fill="currentColor" xmlns="http://www.w3.org/2000/svg"><rect x="1" y="1.5" width="10" height="1.6" rx="0.8"/><rect x="1" y="5.2" width="10" height="1.6" rx="0.8"/><rect x="1" y="8.9" width="10" height="1.6" rx="0.8"/></svg>
                {{ t('plines.viewList') }}
              </button>
              <button
                type="button"
                class="vt-btn"
                :class="{ active: viewMode === 'cards' }"
                data-testid="view-cards-btn"
                @click="viewMode = 'cards'"
              >
                <svg width="12" height="12" viewBox="0 0 12 12" fill="currentColor" xmlns="http://www.w3.org/2000/svg"><rect x="1" y="1" width="4.4" height="4.4" rx="1"/><rect x="6.6" y="1" width="4.4" height="4.4" rx="1"/><rect x="1" y="6.6" width="4.4" height="4.4" rx="1"/><rect x="6.6" y="6.6" width="4.4" height="4.4" rx="1"/></svg>
                {{ t('plines.viewCards') }}
              </button>
            </div>
            <n-dropdown :options="sortOptions" trigger="click" @select="(key: SortKey) => changeSort(key)">
              <button type="button" class="sort-btn" data-testid="sort-btn">
                {{ t('plines.sort') }}
                <svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" stroke-width="1.5" xmlns="http://www.w3.org/2000/svg"><path d="M2 3.5 L5 6.5 L8 3.5"/></svg>
              </button>
            </n-dropdown>
          </div>
        </div>

        <!-- 列表视图（原型 pipe-table）。 -->
        <section v-if="viewMode === 'list'" class="sisy-card pipe-table" aria-label="pipeline list">
          <div class="pipe-thead">
            <span class="pc-name">{{ t('plines.colPipeline') }}</span>
            <span class="pc-status">{{ t('plines.colStatus') }}</span>
            <span class="pc-progress">{{ t('plines.colProgress') }}</span>
            <span class="pc-rate">{{ t('plines.colRate') }}</span>
            <span class="pc-avg">{{ t('plines.colAvg') }}</span>
            <span class="pc-trigger">{{ t('plines.colTrigger') }}</span>
            <span class="pc-action" />
          </div>
          <div class="pipe-tbody">
            <div v-for="row in visibleRows" :key="rowKeyOf(row)" class="pipe-row">
              <button type="button" class="pc-name" @click="openPipeline(row)">
                <span class="n">{{ row.pipeline }}</span>
                <span class="r">{{ row.project }} · {{ t('plines.latestRun') }} {{ latestRunText(row) }}</span>
              </button>
              <div class="pc-status">
                <span class="badge" :class="statusBadgeClass(row.latest?.status)">
                  {{ statusLabel(row.latest?.status) }}
                </span>
              </div>
              <div class="pc-progress"><span class="pct-none">—</span></div>
              <span class="pc-rate">{{ row.rate ?? '—' }}</span>
              <span class="pc-avg">{{ row.avgMs != null ? formatDuration(row.avgMs) : '—' }}</span>
              <span class="pc-trigger">{{ triggerLabel(row) }}</span>
              <div class="pc-action">
                <button
                  type="button"
                  class="btn-outline"
                  :class="actionFor(row).cls"
                  :disabled="actingKey === rowKeyOf(row)"
                  @click="runAction(row)"
                >
                  {{ actionFor(row).label }}
                </button>
              </div>
            </div>
            <div v-if="visibleRows.length === 0" class="pipe-empty">
              <n-text depth="3">{{ t('plines.empty') }}</n-text>
            </div>
          </div>
        </section>

        <!-- 卡片视图（原型 cards-view 2 列网格）。 -->
        <section v-else class="cards-view" aria-label="pipeline cards">
          <article v-for="row in visibleRows" :key="rowKeyOf(row)" class="p-card">
            <div class="p-card-head">
              <button type="button" class="p-card-name" @click="openPipeline(row)">{{ row.pipeline }}</button>
              <span class="badge" :class="statusBadgeClass(row.latest?.status)">
                {{ statusLabel(row.latest?.status) }}
              </span>
            </div>
            <div class="p-card-sub">{{ row.project }} · {{ t('plines.latestRun') }} {{ latestRunText(row) }}</div>
            <div class="p-card-stats">
              <div class="p-stat">
                <span class="l">{{ t('plines.colRate') }}</span>
                <span class="v">{{ row.rate ?? '—' }}</span>
              </div>
              <div class="p-stat">
                <span class="l">{{ t('plines.colAvg') }}</span>
                <span class="v">{{ row.avgMs != null ? formatDuration(row.avgMs) : '—' }}</span>
              </div>
              <div class="p-stat">
                <span class="l">{{ t('plines.colTrigger') }}</span>
                <span class="v">{{ triggerLabel(row) }}</span>
              </div>
            </div>
            <div class="p-card-foot">
              <span class="trigger-tag">{{ t('plines.totalPipelines', { n: row.total }) }}</span>
              <button
                type="button"
                class="btn-outline"
                :class="actionFor(row).cls"
                :disabled="actingKey === rowKeyOf(row)"
                @click="runAction(row)"
              >
                {{ actionFor(row).label }}
              </button>
            </div>
          </article>
        </section>

        <p class="form-hint plines-hint">{{ t('plines.statsHint') }}</p>
      </template>
    </template>
  </div>
</template>

<style scoped>
.plines-page {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.plines-error {
  margin-bottom: 4px;
}

.plines-skeleton {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.plines-skeleton-row {
  width: 100%;
}

.plines-empty {
  padding: 48px 0;
}

/* 工具栏（原型 toolbar-row）。 */
.toolbar-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  flex-wrap: wrap;
}

.filter-chips {
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
}

.chip {
  display: flex;
  align-items: center;
  gap: 6px;
  height: 32px;
  padding: 0 6px 0 12px;
  border-radius: var(--sisy-radius-pill);
  background: var(--sisy-color-surface);
  border: 1px solid var(--sisy-color-border);
  font-family: inherit;
  font-size: 12px;
  font-weight: 500;
  color: var(--sisy-color-text);
  cursor: pointer;
  user-select: none;
  transition: all 0.15s;
}

.chip .count {
  display: flex;
  align-items: center;
  justify-content: center;
  min-width: 22px;
  height: 18px;
  padding: 0 5px;
  border-radius: var(--sisy-radius-pill);
  background: var(--sisy-color-bg);
  font-size: 11px;
  color: var(--sisy-color-text-secondary);
}

.chip:hover {
  border-color: var(--sisy-color-text-tertiary);
}

.chip.active {
  background: var(--sisy-color-primary);
  border-color: var(--sisy-color-primary);
  color: #ffffff;
}

.chip.active .count {
  background: rgba(255, 255, 255, 0.22);
  color: #ffffff;
}

.toolbar-right {
  display: flex;
  align-items: center;
  gap: 12px;
}

.view-toggle {
  display: flex;
  background: var(--sisy-color-surface);
  border: 1px solid var(--sisy-color-border);
  border-radius: var(--sisy-radius);
  padding: 3px;
  gap: 2px;
}

.vt-btn {
  display: flex;
  align-items: center;
  gap: 6px;
  height: 26px;
  padding: 0 12px;
  border: none;
  border-radius: var(--sisy-radius-small);
  background: none;
  font-family: inherit;
  font-size: 12px;
  font-weight: 500;
  color: var(--sisy-color-text-secondary);
  cursor: pointer;
  user-select: none;
  transition: all 0.15s;
}

.vt-btn.active {
  background: var(--sisy-color-primary);
  color: #ffffff;
}

.sort-btn {
  display: flex;
  align-items: center;
  gap: 6px;
  height: 32px;
  padding: 0 12px;
  background: var(--sisy-color-surface);
  border: 1px solid var(--sisy-color-border);
  border-radius: var(--sisy-radius);
  font-family: inherit;
  font-size: 12px;
  font-weight: 500;
  color: var(--sisy-color-text);
  cursor: pointer;
  user-select: none;
  transition: border-color 0.15s;
}

.sort-btn:hover {
  border-color: var(--sisy-color-text-tertiary);
}

.sort-btn svg {
  color: var(--sisy-color-text-secondary);
}

/* 列表视图（原型 pipe-table）。 */
.pipe-table {
  min-height: 0;
}

.pipe-thead {
  display: flex;
  align-items: center;
  padding: 0 20px;
  height: 44px;
  border-bottom: 1px solid var(--sisy-color-border);
  flex-shrink: 0;
}

.pipe-thead span {
  font-size: 12px;
  font-weight: 500;
  color: var(--sisy-color-text-secondary);
}

.pipe-tbody {
  display: flex;
  flex-direction: column;
}

.pipe-row {
  display: flex;
  align-items: center;
  padding: 0 20px;
  min-height: 64px;
  border-bottom: 1px solid var(--sisy-color-border-light);
  transition: background 0.15s;
}

.pipe-row:last-child {
  border-bottom: none;
}

.pipe-row:hover {
  background: var(--sisy-color-bg);
}

.pc-name {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 3px;
  border: none;
  background: none;
  padding: 0;
  cursor: pointer;
  text-align: left;
  font-family: inherit;
}

.pc-name .n {
  font-size: 13px;
  font-weight: 600;
  color: var(--sisy-color-text);
}

.pc-name:hover .n {
  color: var(--sisy-color-primary);
}

.pc-name .r {
  font-size: 11px;
  color: var(--sisy-color-text-secondary);
}

.pc-status {
  width: 90px;
  display: flex;
}

.pc-progress {
  width: 120px;
  display: flex;
  align-items: center;
}

.pc-progress .pct-none {
  font-size: 12px;
  color: var(--sisy-color-text-tertiary);
}

.pc-rate,
.pc-avg {
  width: 90px;
  font-size: 13px;
  color: var(--sisy-color-text);
}

.pc-trigger {
  width: 90px;
  font-size: 12px;
  color: var(--sisy-color-text-secondary);
}

.pc-action {
  width: 84px;
  display: flex;
  justify-content: flex-end;
}

.pipe-empty {
  padding: 24px 20px;
}

/* 卡片视图（原型 cards-view：1440 桌面 2 列；窄屏回落自适应）。 */
.cards-view {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(420px, 1fr));
  gap: 16px;
  align-content: start;
}

.p-card {
  background: var(--sisy-color-surface);
  border-radius: var(--sisy-radius-card);
  padding: 20px;
  display: flex;
  flex-direction: column;
  gap: 14px;
  transition: box-shadow 0.15s;
}

.p-card:hover {
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.06);
}

.p-card-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}

.p-card-name {
  border: none;
  background: none;
  padding: 0;
  cursor: pointer;
  font-family: inherit;
  font-size: 14px;
  font-weight: 600;
  color: var(--sisy-color-text);
}

.p-card-name:hover {
  color: var(--sisy-color-primary);
}

.p-card-sub {
  font-size: 12px;
  color: var(--sisy-color-text-secondary);
  margin-top: -6px;
}

.p-card-stats {
  display: flex;
  border-top: 1px solid var(--sisy-color-border-light);
  padding-top: 12px;
}

.p-stat {
  flex: 1;
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-width: 0;
}

.p-stat .l {
  font-size: 11px;
  color: var(--sisy-color-text-secondary);
}

.p-stat .v {
  font-size: 13px;
  font-weight: 600;
  color: var(--sisy-color-text);
}

.p-card-foot {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}

.trigger-tag {
  display: inline-flex;
  align-items: center;
  height: 22px;
  padding: 0 10px;
  border-radius: var(--sisy-radius-pill);
  background: var(--sisy-color-bg);
  font-size: 11px;
  color: var(--sisy-color-text-secondary);
}

.plines-hint {
  margin-top: -4px;
}

@media (max-width: 767px) {
  .pc-progress,
  .pc-rate {
    display: none;
  }
}
</style>

<script setup lang="ts">
// 流水线页（原型页二，spec #99/#105 定稿）：跨项目流水线列表——筛选 Chips
// （全部/进行中/成功/失败/超时取消，含计数，P2 裁定口径）+ 卡片/列表双视图
// （P6 裁定：默认卡片，切换仅会话内保持）+ 排序 + 行内动作 + 收藏入口。
//
// 数据：
// - 清单 `GET /pipelines`（契约票 #105，P1 裁定——服务端权威清单，替代
//   探测凑数）；清单失败整页报错 + 重试（既有事实态纪律，不做探测回退）。
// - 每行调统计端点 `GET …/stats?window=20`（契约票 #102）：成功率/平均耗时/
//   构建总数/最近一条构建由服务端聚合；窗口内无终态构建时 success_rate /
//   avg_duration_ms 为 null → 显示「—」。
// - 进度（P3 裁定）：最近构建为 running 的行走既有构建详情端点取阶段/任务
//   态，进度 = 当前 attempt 已落定任务数 / 任务总数（双层进度条）；排队未
//   开始与非运行行显示「—」。
// - 收藏（票 #104 W8 裁定，入口随本票落地）：`GET/PUT/DELETE
//   /user/pipeline-favorites`；星标切换，失败 toast 行内报错。
// - 轻轮询（5s）：仅对最近构建为排队/运行中的行重取统计与进度，mock 动态
//   构建生命周期在页面上「活」起来（触发后无需手动刷新）。
//
// 行内动作按最近构建状态映射：运行中/排队 → 终止（红）；失败 → 重试
// （橙）；其余 → 运行（蓝），走既有 cancel / rerun / trigger API。
// 顶栏搜索（`?q=`）按流水线/项目名过滤。

import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'
import { NAlert, NDropdown, NEmpty, NSkeleton, useMessage } from 'naive-ui'

import { buildsApi, favoritesApi, pipelinesApi } from '@/api/client'
import { describeActionError, describeSubmitError } from '@/api/errors'
import { formatDuration, relativeAge, relativeAgeKey, settledPercent, statusBadgeClass } from '@/utils/format'
import type { BuildDetailResponse, LatestBuildRef } from '@/api/types'

const { t } = useI18n()
const route = useRoute()
const router = useRouter()
const message = useMessage()

/** 统计窗口（服务端聚合口径，契约票 #102）。 */
const WINDOW = 20
/** 活跃行轻轮询间隔（排队/运行中行重取统计与进度）。 */
const POLL_MS = 5000

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
  /** 最近构建为 running 时的任务进度（0–100 整数；其余为 null → 「—」）。 */
  progress: number | null
}

const rows = ref<PipelineRow[]>([])
const loading = ref(true)
const loadError = ref('')

/** 动作进行中（按行标记，按钮转圈/禁用）。 */
const actingKey = ref('')

type ChipKey = 'all' | 'active' | 'success' | 'failed' | 'ended' | 'never'
const activeChip = ref<ChipKey>('all')
type SortKey = 'recent' | 'name'
const sortKey = ref<SortKey>('recent')
/** P6 裁定：默认卡片视图；切换偏好仅会话内保持（不落 localStorage）。 */
const viewMode = ref<'list' | 'cards'>('cards')

/** 当前用户收藏的 (project, pipeline) 集合（票 #104 W8 契约；加载失败非致命）。 */
const favoriteKeys = ref(new Set<string>())

/** 顶栏搜索（`?q=`，App 壳写入）。 */
const searchQuery = computed(() => (typeof route.query.q === 'string' ? route.query.q.trim() : ''))

const rowKeyOf = (row: { project: string; pipeline: string }): string => `${row.project}/${row.pipeline}`

/** 活跃行轻轮询间隔（排队/运行中行重取统计与进度）。 */
let pollTimer: ReturnType<typeof setInterval> | null = null

onMounted(() => {
  void load()
  pollTimer = setInterval(() => {
    void refreshLiveRows()
  }, POLL_MS)
})

onBeforeUnmount(() => {
  if (pollTimer != null) clearInterval(pollTimer)
  pollTimer = null
})

async function load(): Promise<void> {
  loading.value = true
  loadError.value = ''
  try {
    const list = await pipelinesApi.list()

    // 收藏集合（非致命：失败仅星标态不可见，可重进页面恢复）。
    void loadFavorites()

    const loaded = await Promise.all(list.items.map((item) => loadRow(item)))
    rows.value = sortRows(loaded)
  } catch (err) {
    rows.value = []
    loadError.value = describeSubmitError(err)
  } finally {
    loading.value = false
  }
}

async function loadFavorites(): Promise<void> {
  try {
    favoriteKeys.value = new Set((await favoritesApi.list()).map((f) => rowKeyOf(f)))
  } catch {
    // 星标态非致命数据：失败不拖垮清单，星标视为未收藏。
    favoriteKeys.value = new Set()
  }
}

/** 单条流水线 → 行数据；统计不可见（404）或权限不足（403）按「未运行」行展示。 */
async function loadRow(item: { project: string; pipeline: string }): Promise<PipelineRow> {
  try {
    const stats = await pipelinesApi.stats(item.project, item.pipeline, WINDOW)
    const latest = stats.latest_build
    return {
      project: item.project,
      pipeline: item.pipeline,
      latest,
      total: stats.total_builds,
      rate: stats.success_rate != null ? `${stats.success_rate}%` : null,
      avgMs: stats.avg_duration_ms,
      progress: await progressFor(item, latest),
    }
  } catch {
    return {
      project: item.project,
      pipeline: item.pipeline,
      latest: null,
      total: 0,
      rate: null,
      avgMs: null,
      progress: null,
    }
  }
}

/** P3：运行中构建 → 任务进度（当前 attempt 已落定任务 / 任务总数）。
 *  排队未开始/非运行/详情不可得 → null（显示「—」，不造假）。 */
async function progressFor(
  item: { project: string; pipeline: string },
  latest: LatestBuildRef | null,
): Promise<number | null> {
  if (latest?.status !== 'running') return null
  try {
    const detail = await buildsApi.detail(item.project, item.pipeline, latest.number)
    return progressOfDetail(detail)
  } catch {
    return null
  }
}

function progressOfDetail(detail: BuildDetailResponse): number | null {
  // 进度口径（P3）与构建详情阶段进度共享 settledPercent。
  const jobs = detail.stages
    .flatMap((stage) => stage.jobs)
    .filter((job) => job.attempt === detail.attempt)
  return settledPercent(jobs)
}

/** 轻轮询：仅重取最近构建为排队/运行中的行（统计 + 进度），其余不动。 */
async function refreshLiveRows(): Promise<void> {
  if (loading.value) return
  const live = new Map<string, PipelineRow>()
  for (const row of rows.value) {
    if (row.latest?.status === 'queued' || row.latest?.status === 'running') {
      live.set(rowKeyOf(row), row)
    }
  }
  if (live.size === 0) return
  const fresh = await Promise.all([...live.values()].map((row) => loadRow(row)))
  // 统计取不到（瞬时失败）的行不回写——构建不会凭空消失，避免把在跑行
  // 错误翻成「未运行」；下一轮轮询自会修正。
  const freshByKey = new Map<string, PipelineRow>()
  for (const row of fresh) {
    if (row.latest != null) freshByKey.set(rowKeyOf(row), row)
  }
  if (freshByKey.size === 0) return
  rows.value = sortRows(rows.value.map((row) => freshByKey.get(rowKeyOf(row)) ?? row))
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

// ===== Chips 过滤（计数 + 单选；P2 裁定口径）=====
// 「进行中」含排队（未终态都算在跑）；「超时/取消」单列；「未运行」收留
// latest 为空（零构建/统计不可见）的行——保证
// 全部 = 进行中 + 成功 + 失败 + 超时/取消 + 未运行 计数严格对账。

function chipHit(row: PipelineRow, chip: ChipKey): boolean {
  if (chip === 'all') return true
  const status = row.latest?.status
  if (chip === 'active') return status === 'running' || status === 'queued'
  if (chip === 'failed') return status === 'failed'
  if (chip === 'ended') return status === 'timeout' || status === 'cancelled'
  if (chip === 'never') return status === undefined
  return status === 'succeeded'
}

const chipCounts = computed<Record<ChipKey, number>>(() => {
  const counts: Record<ChipKey, number> = {
    all: rows.value.length,
    active: 0,
    success: 0,
    failed: 0,
    ended: 0,
    never: 0,
  }
  for (const row of rows.value) {
    if (chipHit(row, 'active')) counts.active += 1
    if (chipHit(row, 'success')) counts.success += 1
    if (chipHit(row, 'failed')) counts.failed += 1
    if (chipHit(row, 'ended')) counts.ended += 1
    if (chipHit(row, 'never')) counts.never += 1
  }
  return counts
})

const chipDefs = computed<{ key: ChipKey; label: string }[]>(() => [
  { key: 'all', label: t('plines.chipAll') },
  { key: 'active', label: t('plines.chipActive') },
  { key: 'success', label: t('plines.chipSuccess') },
  { key: 'failed', label: t('plines.chipFailed') },
  { key: 'ended', label: t('plines.chipEnded') },
  { key: 'never', label: t('plines.chipNever') },
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

// ===== 收藏（票 #104 W8 契约；入口随本票落地）=====

function isFavorite(row: PipelineRow): boolean {
  return favoriteKeys.value.has(rowKeyOf(row))
}

async function toggleFavorite(row: PipelineRow): Promise<void> {
  const key = rowKeyOf(row)
  const removing = favoriteKeys.value.has(key)
  try {
    if (removing) {
      await favoritesApi.remove(row.project, row.pipeline)
      const next = new Set(favoriteKeys.value)
      next.delete(key)
      favoriteKeys.value = next
      message.success(t('plines.favoriteRemoved'))
    } else {
      await favoritesApi.add(row.project, row.pipeline)
      favoriteKeys.value = new Set(favoriteKeys.value).add(key)
      message.success(t('plines.favoriteAdded'))
    }
  } catch (err) {
    message.error(describeActionError(err))
  }
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
    // 动作落定后刷新该行（状态/计数/进度就近更新）。
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
    >
      <button type="button" class="btn-outline" data-testid="pipelines-retry" @click="load">
        {{ t('plines.retry') }}
      </button>
    </n-alert>

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
            <span class="pc-fav" />
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
              <div class="pc-fav">
                <button
                  type="button"
                  class="fav-btn"
                  :class="{ active: isFavorite(row) }"
                  :title="isFavorite(row) ? t('plines.favoriteRemove') : t('plines.favoriteAdd')"
                  :data-testid="`fav-${rowKeyOf(row)}`"
                  @click="toggleFavorite(row)"
                >
                  <svg width="14" height="14" viewBox="0 0 24 24" :fill="isFavorite(row) ? 'currentColor' : 'none'" stroke="currentColor" stroke-width="2" stroke-linejoin="round" xmlns="http://www.w3.org/2000/svg"><path d="M12 2.5l2.9 6 6.6.9-4.8 4.6 1.2 6.5-5.9-3.2-5.9 3.2 1.2-6.5L2.5 9.4l6.6-.9z"/></svg>
                </button>
              </div>
              <button type="button" class="pc-name" @click="openPipeline(row)">
                <span class="n">{{ row.pipeline }}</span>
                <span class="r">{{ row.project }} · {{ t('plines.latestRun') }} {{ latestRunText(row) }}</span>
              </button>
              <div class="pc-status">
                <span class="badge" :class="statusBadgeClass(row.latest?.status ?? '')">
                  {{ statusLabel(row.latest?.status) }}
                </span>
              </div>
              <div class="pc-progress">
                <div v-if="row.progress != null" class="usage-row">
                  <div class="track">
                    <div class="fill" :style="{ width: `${row.progress}%` }" />
                  </div>
                  <span class="pct">{{ row.progress }}%</span>
                </div>
                <span v-else class="pct-none">—</span>
              </div>
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

        <!-- 卡片视图（原型 cards-view 2 列网格；P6 定稿默认视图）。 -->
        <section v-else class="cards-view" aria-label="pipeline cards">
          <article v-for="row in visibleRows" :key="rowKeyOf(row)" class="p-card">
            <div class="p-card-head">
              <div class="p-card-title">
                <button
                  type="button"
                  class="fav-btn"
                  :class="{ active: isFavorite(row) }"
                  :title="isFavorite(row) ? t('plines.favoriteRemove') : t('plines.favoriteAdd')"
                  :data-testid="`fav-${rowKeyOf(row)}`"
                  @click="toggleFavorite(row)"
                >
                  <svg width="14" height="14" viewBox="0 0 24 24" :fill="isFavorite(row) ? 'currentColor' : 'none'" stroke="currentColor" stroke-width="2" stroke-linejoin="round" xmlns="http://www.w3.org/2000/svg"><path d="M12 2.5l2.9 6 6.6.9-4.8 4.6 1.2 6.5-5.9-3.2-5.9 3.2 1.2-6.5L2.5 9.4l6.6-.9z"/></svg>
                </button>
                <button type="button" class="p-card-name" @click="openPipeline(row)">{{ row.pipeline }}</button>
              </div>
              <span class="badge" :class="statusBadgeClass(row.latest?.status ?? '')">
                {{ statusLabel(row.latest?.status) }}
              </span>
            </div>
            <div class="p-card-sub">{{ row.project }} · {{ t('plines.latestRun') }} {{ latestRunText(row) }}</div>
            <div v-if="row.progress != null" class="p-card-progress">
              <div class="usage-row">
                <div class="track">
                  <div class="fill" :style="{ width: `${row.progress}%` }" />
                </div>
                <span class="pct">{{ row.progress }}%</span>
              </div>
            </div>
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
              <span class="trigger-tag">{{ t('plines.totalBuilds', { n: row.total }) }}</span>
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

.plines-error button {
  margin-top: 8px;
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

/* 收藏星标（票 #104 W8 入口）。 */
.fav-btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  border: none;
  border-radius: var(--sisy-radius-small);
  background: none;
  padding: 0;
  color: var(--sisy-color-text-tertiary);
  cursor: pointer;
  transition: color 0.15s, background 0.15s;
}

.fav-btn:hover {
  color: var(--sisy-color-warning);
  background: var(--sisy-color-bg);
}

.fav-btn.active {
  color: var(--sisy-color-warning);
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

.pc-fav {
  width: 32px;
  flex-shrink: 0;
  display: flex;
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

.p-card-title {
  display: flex;
  align-items: center;
  gap: 4px;
  min-width: 0;
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

.p-card-progress {
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

/* G2（平板档降级次要列）：≤1024px 收起「触发方式」；≤880px 再收起
   「进度/成功率」，保留 状态/平均耗时/动作。桌面档不受影响。 */
@media (max-width: 1024px) {
  .pc-trigger {
    display: none;
  }
}

@media (max-width: 880px) {
  .pc-progress,
  .pc-rate {
    display: none;
  }
}
</style>

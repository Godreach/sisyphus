<script setup lang="ts">
// 构建列表页（票 B4-T4，ADR-0006）：`GET .../builds` 分页 + 状态过滤，
// 作为构建详情页（#66 本票）的入口——从列表行点击进入构建详情。
//
// - 分页：page/limit（默认 20，1..=100），倒序；状态过滤下拉（全部 +
//   queued/running/succeeded/failed/cancelled/timeout）。
// - 行展示：构建号、状态、触发源、触发人、attempt、开始/终态、耗时。
// - 加载失败（404 = 项目/pipeline 不可见）与空列表都有明确文案。
// #93: 使用 Naive UI 组件重写——NDataTable（状态列 NTag 颜色编码、
// 行点击进详情）、NSelect 状态筛选、NPagination 分页、NEmpty 空态、
// NAlert 错误态、NSkeleton 首载骨架屏；视觉与 #84/#86 主题一致。

import { computed, h, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'
import {
  NAlert,
  NDataTable,
  NEmpty,
  NPagination,
  NSelect,
  NSkeleton,
  NTag,
  type DataTableColumns,
} from 'naive-ui'

import { buildsApi } from '@/api/client'
import { describeActionError } from '@/api/errors'
import { formatDateTime } from '@/utils/format'
import type { BuildListResponse, BuildStatusDto, BuildSummaryResponse } from '@/api/types'

const route = useRoute()
const router = useRouter()
const { t } = useI18n()

const project = computed(() => String(route.params.name ?? ''))
const pipeline = computed(() => String(route.params.pipeline ?? ''))

const PAGE_SIZE = 20

const list = ref<BuildListResponse | null>(null)
const loading = ref(true)
const errorMessage = ref('')
const currentPage = ref(1)
const statusFilter = ref<BuildStatusDto | ''>('')

const totalPages = computed(() =>
  list.value ? Math.max(1, Math.ceil(list.value.total / list.value.limit)) : 1,
)

async function loadList(): Promise<void> {
  loading.value = true
  errorMessage.value = ''
  try {
    list.value = await buildsApi.list(project.value, pipeline.value, {
      page: currentPage.value,
      limit: PAGE_SIZE,
      status: statusFilter.value,
    })
  } catch (err) {
    // 统一错误描述（网络失败/泛化与详情页动作共用同一分支，errors.ts）。
    errorMessage.value = describeActionError(err)
    list.value = null
  } finally {
    loading.value = false
  }
}

function goPage(page: number): void {
  if (page < 1 || page > totalPages.value) return
  currentPage.value = page
  void loadList()
}

function changeStatus(): void {
  currentPage.value = 1
  void loadList()
}

function openBuild(number: number): void {
  void router.push({
    name: 'build-detail',
    params: { name: project.value, pipeline: pipeline.value, number: String(number) },
  })
}

function buildStatusKey(status: BuildStatusDto): string {
  return `buildStatus.${status}`
}

function triggerKey(trigger: string): string {
  return `triggerSource.${trigger}`
}

/** 构建状态 → NTag 状态色（成功=绿 / 失败=红 / 运行=蓝 / 取消=灰等，
 *  与 Overview 最近构建列同色系，主题 Token 驱动）。 */
function buildStatusType(status: BuildStatusDto): 'success' | 'error' | 'info' | 'warning' | 'default' {
  switch (status) {
    case 'succeeded':
      return 'success'
    case 'failed':
      return 'error'
    case 'running':
      return 'info'
    case 'queued':
    case 'timeout':
      return 'warning'
    default:
      return 'default'
  }
}

/** 状态筛选下拉选项（全部 + 各状态；NSelect value 为空串 = 全部）。 */
const statusOptions = computed(() => [
  { label: t('buildList.allStatuses'), value: '' },
  ...(['queued', 'running', 'succeeded', 'failed', 'cancelled', 'timeout'] as const).map((s) => ({
    label: t(buildStatusKey(s)),
    value: s,
  })),
])

/** NDataTable 列（行点击进详情经 row-props 挂载；状态列 NTag 色标）。 */
const columns = computed<DataTableColumns<BuildSummaryResponse>>(() => [
  {
    title: t('buildList.number'),
    key: 'number',
    sorter: (a, b) => a.number - b.number,
    render: (row) => `#${row.number}`,
  },
  {
    title: t('buildList.status'),
    key: 'status',
    sorter: (a, b) => a.status.localeCompare(b.status),
    render: (row) =>
      h(NTag, { size: 'small', type: buildStatusType(row.status), bordered: false }, {
        default: () => t(buildStatusKey(row.status)),
      }),
  },
  {
    title: t('buildList.trigger'),
    key: 'trigger',
    render: (row) => t(triggerKey(row.trigger)),
  },
  {
    title: t('buildList.triggerBy'),
    key: 'trigger_by',
  },
  {
    title: t('buildList.attempt'),
    key: 'attempt',
  },
  {
    title: t('buildList.startedAt'),
    key: 'started_at',
    render: (row) => formatDateTime(row.started_at),
  },
  {
    title: t('buildList.finishedAt'),
    key: 'finished_at',
    render: (row) => formatDateTime(row.finished_at),
  },
])

const rowKey = (row: BuildSummaryResponse): number => row.number

/** 行点击进详情（NDataTable row-props：class 供测试定位 + onClick 跳转）。 */
function rowProps(row: BuildSummaryResponse): { class: string; onClick: () => void } {
  return {
    class: 'build-list-row',
    onClick: () => openBuild(row.number),
  }
}

onMounted(loadList)

watch(
  () => [project.value, pipeline.value],
  () => {
    currentPage.value = 1
    statusFilter.value = ''
    void loadList()
  },
)
</script>

<template>
  <div class="build-list-page">
    <nav class="breadcrumb" aria-label="Breadcrumb">
      <router-link :to="{ name: 'projects' }">{{ t('routes.projects') }}</router-link>
      <span class="breadcrumb-sep">/</span>
      <router-link :to="{ name: 'project-detail', params: { name: project } }">
        {{ project }}
      </router-link>
      <span class="breadcrumb-sep">/</span>
      <span class="breadcrumb-current">{{ pipeline }}</span>
    </nav>

    <header class="build-list-header">
      <h1>{{ pipeline }}</h1>
      <router-link
        :to="{ name: 'pipeline-edit', params: { name: project, pipeline } }"
        class="build-list-edit"
      >
        {{ t('buildList.editPipeline') }}
      </router-link>
    </header>

    <div class="build-list-toolbar">
      <n-select
        v-model:value="statusFilter"
        :options="statusOptions"
        name="status-filter"
        class="build-status-filter"
        :placeholder="t('buildList.filterByStatus')"
        :virtual-scroll="false"
        @update:value="changeStatus"
      />
    </div>

    <n-alert
      v-if="errorMessage"
      type="error"
      :title="errorMessage"
      role="alert"
      class="build-list-alert"
    />

    <!-- 首载骨架屏（#93：与 Overview 同纪律——数据到达后替换）。 -->
    <div v-if="loading && !errorMessage" class="build-list-skeleton" data-testid="build-list-skeleton">
      <n-skeleton v-for="i in 5" :key="i" text :repeat="2" height="28px" class="build-list-skeleton-row" />
    </div>

    <div v-else-if="!errorMessage && list && list.items.length === 0" class="build-list-empty">
      <n-empty :description="t('buildList.empty')" />
    </div>

    <n-data-table
      v-else-if="list && list.items.length > 0"
      :columns="columns"
      :data="list.items"
      :row-key="rowKey"
      :row-props="rowProps"
      :bordered="false"
      :single-line="true"
      size="small"
      :scroll-x="840"
      class="build-list-table"
    />

    <div v-if="list && list.items.length > 0" class="build-list-pagination">
      <n-pagination
        v-model:page="currentPage"
        :item-count="list.total"
        :page-size="PAGE_SIZE"
        @update:page="goPage"
      />
    </div>
  </div>
</template>

<style scoped>
.build-list-page {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.build-list-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}

.build-list-header h1 {
  margin: 0;
  font-size: 22px;
}

.build-list-edit {
  font-size: 13px;
  color: var(--n-text-color-link, #2b5797);
  text-decoration: none;
}

.build-list-edit:hover {
  text-decoration: underline;
}

.build-list-toolbar {
  display: flex;
  justify-content: flex-end;
}

.build-status-filter {
  width: 200px;
}

.build-list-alert {
  margin: 4px 0;
}

.build-list-empty {
  padding: 24px 0;
}

.build-list-skeleton {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin: 8px 0;
}

.build-list-skeleton-row {
  width: 100%;
}

/* NDataTable 行点击进详情：hover 高亮 + 可点击指针。 */
.build-list-table :deep(.build-list-row) {
  cursor: pointer;
}

.build-list-pagination {
  display: flex;
  justify-content: flex-end;
}
</style>

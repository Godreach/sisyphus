<script setup lang="ts">
// 审计日志页（ADR-0015，票 B4-T6）：全局 admin 安全事件回放面。
//
// 管理区全局 admin 面（侧栏 is_admin 门控 + 路由守卫兜底；端点亦全局 admin
// 专属，403 → admin-only 退化态兜底直访/会话过期场景）。
//
// - `GET /audit`：按时间 since/until + 用户 + 项目 + 事件类型过滤 + limit/offset
//   分页，时间倒序（后端保证，新事件在前）。
// - 响应为审计条目数组（无 total）：下一页可用性由「本页条数 == limit」判定，
//   上一页由 offset > 0 判定——与有 total 的列表页（构建列表 NPagination）不同，
//   此处仅前后翻页 NButton，不显示总页数。
// - detail 为 JSON 对象（机密事件只记名、永不泄值）；事件类型取值域与后端
//   `AuditEvent::ALL` 同源（`AUDIT_EVENTS` 单点），过滤下拉与表格列均按此渲染。
// #95: 使用 Naive UI 组件重写——过滤器改 NDatePicker(datetimerange) + NInput +
// NSelect、表格改 NDataTable（事件列 NTag、detail 列 NCode JSON）、分页保留
// 前后翻页 NButton、错误态 NAlert、加载 NSkeleton、空态 NEmpty。

import { computed, h, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  NAlert,
  NButton,
  NDataTable,
  NDatePicker,
  NEmpty,
  NInput,
  NSelect,
  NSkeleton,
  NTag,
  NCode,
  type DataTableColumns,
} from 'naive-ui'

import { auditApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import { AUDIT_EVENTS, type AuditEntryResponse, type AuditEventDto } from '@/api/types'
import { formatDateTime } from '@/utils/format'

const { t } = useI18n()

const PAGE_SIZE = 50

/** 时间范围（NDatePicker datetimerange 值形态：[sinceMs, untilMs] | null）。 */
const range = ref<[number, number] | null>(null)
const userFilter = ref('')
const projectFilter = ref('')
const eventFilter = ref<AuditEventDto | ''>('')

const entries = ref<AuditEntryResponse[] | null>(null)
const errorMessage = ref('')
/** 403（非全局 admin）→ admin-only 退化态：不渲染过滤器/表格。 */
const adminOnly = ref(false)
const loading = ref(false)
const offset = ref(0)

/** 下一页可用：本页恰满 limit 即可能还有更多（无 total，保守判定）。 */
const hasNext = computed(() => entries.value != null && entries.value.length === PAGE_SIZE)
/** 上一页可用：offset > 0。 */
const hasPrev = computed(() => offset.value > 0)

/** 事件类型下拉选项（全部 + 各事件；NSelect value 为空串 = 全部）。 */
const eventOptions = computed(() => [
  { label: t('audit.allEvents'), value: '' },
  ...AUDIT_EVENTS.map((e) => ({ label: t(eventLabelKey(e)), value: e })),
])

/** NDataTable 列（事件列 NTag 徽标；detail 列 NCode JSON 人读形态）。 */
const columns = computed<DataTableColumns<AuditEntryResponse>>(() => [
  {
    title: t('audit.colTime'),
    key: 'ts',
    width: 170,
    render: (row) => formatDateTime(row.ts),
  },
  {
    title: t('audit.colActor'),
    key: 'actor',
    width: 120,
  },
  {
    title: t('audit.colEvent'),
    key: 'event',
    width: 130,
    render: (row) =>
      h(
        NTag,
        { size: 'small', bordered: false },
        { default: () => t(eventLabelKey(row.event)) },
      ),
  },
  {
    title: t('audit.colProject'),
    key: 'project',
    width: 140,
    render: (row) => row.project ?? '—',
  },
  {
    title: t('audit.colDetail'),
    key: 'detail',
    render: (row) =>
      row.detail == null
        ? h('span', { class: 'form-hint' }, '—')
        : h(NCode, { code: detailText(row.detail), class: 'audit-detail-code' }),
  },
])

const rowKey = (row: AuditEntryResponse): number => row.id

onMounted(() => {
  void load()
})

/** 提交过滤：offset 归零后加载。 */
function applyFilters(): void {
  offset.value = 0
  void load()
}

/** 清空过滤：重置全部过滤器 + offset，重新加载。 */
function clearFilters(): void {
  range.value = null
  userFilter.value = ''
  projectFilter.value = ''
  eventFilter.value = ''
  offset.value = 0
  void load()
}

function prevPage(): void {
  if (!hasPrev.value) return
  offset.value = Math.max(0, offset.value - PAGE_SIZE)
  void load()
}

function nextPage(): void {
  if (!hasNext.value) return
  offset.value = offset.value + PAGE_SIZE
  void load()
}

/** 加载审计条目（时间倒序；detail 为 JSON 对象，机密事件只记名）。 */
async function load(): Promise<void> {
  loading.value = true
  errorMessage.value = ''
  adminOnly.value = false
  try {
    entries.value = await auditApi.list({
      since: range.value?.[0],
      until: range.value?.[1],
      user: userFilter.value.trim() || undefined,
      project: projectFilter.value.trim() || undefined,
      event: eventFilter.value === '' ? undefined : eventFilter.value,
      limit: PAGE_SIZE,
      offset: offset.value,
    })
  } catch (err) {
    entries.value = null
    if (err instanceof ApiError && err.status === 403) {
      adminOnly.value = true
    } else {
      errorMessage.value = describeSubmitError(err)
    }
  } finally {
    loading.value = false
  }
}

/** 事件类型人读标签键（auditEvent.<event>，与 buildStatus/jobStatus 同纪律）。 */
function eventLabelKey(event: AuditEventDto): string {
  return `auditEvent.${event}`
}

/** detail JSON 人读形态（机密事件只记名，值形态永不出现）。 */
function detailText(detail: AuditEntryResponse['detail']): string {
  if (detail == null) return ''
  return JSON.stringify(detail, null, 2)
}
</script>

<template>
  <div class="admin-page audit-page">
    <div class="page-header">
      <h1 class="page-title">{{ t('routes.adminAudit') }}</h1>
    </div>

    <!-- 403 退化态：仅全局管理员可见（审计仅全局 admin 可读）。 -->
    <p v-if="adminOnly" class="form-hint">{{ t('admin.adminOnly') }}</p>

    <template v-else>
      <!-- 过滤器：时间区间（datetimerange）+ 用户 + 项目 + 事件类型。 -->
      <div class="audit-filters">
        <n-date-picker
          v-model:value="range"
          type="datetimerange"
          clearable
          class="audit-range-picker"
        />
        <n-input
          v-model:value="userFilter"
          :input-props="{ name: 'audit-user' }"
          :placeholder="t('audit.userPlaceholder')"
          class="audit-filter-input"
        />
        <n-input
          v-model:value="projectFilter"
          :input-props="{ name: 'audit-project' }"
          :placeholder="t('audit.projectPlaceholder')"
          class="audit-filter-input"
        />
        <n-select
          v-model:value="eventFilter"
          :options="eventOptions"
          class="audit-event-select"
          :virtual-scroll="false"
        />
        <n-button type="primary" name="audit-apply" @click="applyFilters">
          {{ t('audit.apply') }}
        </n-button>
        <n-button name="audit-clear" @click="clearFilters">
          {{ t('audit.clear') }}
        </n-button>
      </div>

      <n-alert v-if="errorMessage" type="error" :title="errorMessage" role="alert" />

      <!-- 首载/过滤加载骨架屏（数据到达后替换）。 -->
      <div v-else-if="loading" class="audit-skeleton">
        <n-skeleton v-for="i in 4" :key="i" text height="28px" class="audit-skeleton-row" />
      </div>

      <div v-else-if="entries && entries.length === 0" class="audit-empty">
        <n-empty :description="t('audit.empty')" />
      </div>

      <!-- 审计表（时间倒序回放：行序即响应序，信任契约的倒序）。 -->
      <n-data-table
        v-else-if="entries"
        :columns="columns"
        :data="entries"
        :row-key="rowKey"
        :bordered="false"
        :single-line="true"
        size="small"
        class="audit-table"
      />

      <!-- 前后翻页（无 total：按本页条数 == limit 判下一页、offset > 0 判上一页）。 -->
      <div v-if="entries && entries.length > 0" class="audit-pagination">
        <n-button size="small" name="audit-prev" :disabled="!hasPrev" @click="prevPage">
          {{ t('audit.prev') }}
        </n-button>
        <n-button size="small" name="audit-next" :disabled="!hasNext" @click="nextPage">
          {{ t('audit.next') }}
        </n-button>
      </div>
    </template>
  </div>
</template>

<style scoped>
.audit-filters {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}

.audit-range-picker {
  width: 360px;
}

.audit-filter-input {
  width: 200px;
}

.audit-event-select {
  width: 180px;
}

.audit-skeleton {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin: 8px 0;
}

.audit-skeleton-row {
  width: 100%;
}

.audit-empty {
  padding: 24px 0;
}

.audit-pagination {
  display: flex;
  gap: 8px;
  justify-content: flex-end;
}

.audit-detail-code {
  font-size: 12px;
}
</style>

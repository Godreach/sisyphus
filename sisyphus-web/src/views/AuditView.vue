<script setup lang="ts">
// 审计日志页（ADR-0015，票 B4-T6）：全局 admin 安全事件回放面。
//
// 管理区全局 admin 面（侧栏 is_admin 门控 + 路由守卫兜底；端点亦全局 admin
// 专属，403 → admin-only 退化态兜底直访/会话过期场景）。
//
// - `GET /audit`：按时间 since/until + 用户 + 项目 + 事件类型过滤 + limit/offset
//   分页，时间倒序（后端保证，新事件在前）。
// - 响应为审计条目数组（无 total）：下一页可用性由「本页条数 == limit」判定，
//   上一页由 offset > 0 判定——与有 total 的列表页（构建列表）不同，此处不显
//   示总页数，仅前后翻页。
// - detail 为 JSON 对象（机密事件只记名、永不泄值）；事件类型取值域与后端
//   `AuditEvent::ALL` 同源（`AUDIT_EVENTS` 单点），过滤下拉与表格列均按此渲染。

import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'

import { auditApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import { AUDIT_EVENTS, type AuditEntryResponse, type AuditEventDto } from '@/api/types'
import { formatDateTime } from '@/utils/format'

const { t } = useI18n()

const PAGE_SIZE = 50

/** 过滤表单（datetime-local 字符串→提交时转 Unix 毫秒）。 */
const sinceLocal = ref('')
const untilLocal = ref('')
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

onMounted(() => {
  void load()
})

/** datetime-local 字符串 → Unix 毫秒（空串 → undefined，拼 query 时剔除）。 */
function toMs(local: string): number | undefined {
  if (local === '') return undefined
  const ms = new Date(local).getTime()
  return Number.isFinite(ms) ? ms : undefined
}

/** 提交过滤：offset 归零后加载。 */
function applyFilters(): void {
  offset.value = 0
  void load()
}

/** 清空过滤：重置全部过滤器 + offset，重新加载。 */
function clearFilters(): void {
  sinceLocal.value = ''
  untilLocal.value = ''
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
      since: toMs(sinceLocal.value),
      until: toMs(untilLocal.value),
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
      <!-- 过滤器：时间区间 + 用户 + 项目 + 事件类型。 -->
      <form class="audit-filters" @submit.prevent>
        <label class="field">
          <span>{{ t('audit.since') }}</span>
          <input v-model="sinceLocal" type="datetime-local" name="audit-since" />
        </label>
        <label class="field">
          <span>{{ t('audit.until') }}</span>
          <input v-model="untilLocal" type="datetime-local" name="audit-until" />
        </label>
        <label class="field">
          <span>{{ t('audit.user') }}</span>
          <input v-model="userFilter" name="audit-user" :placeholder="t('audit.userPlaceholder')" />
        </label>
        <label class="field">
          <span>{{ t('audit.project') }}</span>
          <input v-model="projectFilter" name="audit-project" :placeholder="t('audit.projectPlaceholder')" />
        </label>
        <label class="field">
          <span>{{ t('audit.event') }}</span>
          <select v-model="eventFilter" name="audit-event">
            <option value="">{{ t('audit.allEvents') }}</option>
            <option v-for="e in AUDIT_EVENTS" :key="e" :value="e">{{ t(eventLabelKey(e)) }}</option>
          </select>
        </label>
        <div class="audit-filter-actions">
          <button type="button" class="btn-primary" name="audit-apply" @click="applyFilters">
            {{ t('audit.apply') }}
          </button>
          <button type="button" class="btn-secondary" name="audit-clear" @click="clearFilters">
            {{ t('audit.clear') }}
          </button>
        </div>
      </form>

      <p v-if="loading" class="form-hint">{{ t('audit.loading') }}</p>
      <p v-else-if="errorMessage" class="form-error" role="alert">{{ errorMessage }}</p>
      <p v-else-if="entries && entries.length === 0" class="form-hint">{{ t('audit.empty') }}</p>

      <table v-else-if="entries" class="audit-table">
        <thead>
          <tr>
            <th>{{ t('audit.colTime') }}</th>
            <th>{{ t('audit.colActor') }}</th>
            <th>{{ t('audit.colEvent') }}</th>
            <th>{{ t('audit.colProject') }}</th>
            <th>{{ t('audit.colDetail') }}</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="entry in entries" :key="entry.id">
            <td class="audit-time">{{ formatDateTime(entry.ts) }}</td>
            <td>{{ entry.actor }}</td>
            <td>
              <span class="audit-event-badge">{{ t(eventLabelKey(entry.event)) }}</span>
            </td>
            <td>{{ entry.project ?? '—' }}</td>
            <td>
              <pre v-if="entry.detail" class="audit-detail mono">{{ detailText(entry.detail) }}</pre>
              <span v-else class="form-hint">—</span>
            </td>
          </tr>
        </tbody>
      </table>

      <div v-if="entries && entries.length > 0" class="audit-pagination">
        <button type="button" class="btn" name="audit-prev" :disabled="!hasPrev" @click="prevPage">
          {{ t('audit.prev') }}
        </button>
        <button type="button" class="btn" name="audit-next" :disabled="!hasNext" @click="nextPage">
          {{ t('audit.next') }}
        </button>
      </div>
    </template>
  </div>
</template>

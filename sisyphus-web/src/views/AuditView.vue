<script setup lang="ts">
// 审计日志页（ADR-0015，spec #110 定稿铺开）：全局 admin 安全事件回放面。
// 设计语言与三主页面/项目详情同源——工具栏过滤条 + sisy-card 回放表
// （事件胶囊徽章 + mono detail），翻页收在卡片尾部。
//
// 管理区全局 admin 面（用户卡弹出菜单入口 + 路由守卫兜底；端点亦全局
// admin 专属，403 → admin-only 退化态兜底直访/会话过期场景）。
//
// - `GET /audit`：按时间 since/until + 用户 + 项目 + 事件类型过滤 + limit/offset
//   分页，时间倒序（后端保证，新事件在前）。
// - 响应为审计条目数组（无 total）：下一页可用性由「本页条数 == limit」判定，
//   上一页由 offset > 0 判定——与有 total 的列表页不同，仅前后翻页按钮。
// - detail 为 JSON 对象（机密事件只记名、永不泄值）；事件类型取值域与后端
//   `AuditEvent::ALL` 同源（`AUDIT_EVENTS` 单点），过滤下拉与表格列均按此渲染。
//
// 事实态纪律：首载骨架屏、加载失败整页报错 + 重试、403 退化态、空态。

import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { NAlert, NButton, NEmpty, NIcon, NSelect, NSkeleton } from 'naive-ui'
import { RefreshOutline } from '@vicons/ionicons5'

import { auditApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import { AUDIT_EVENTS, type AuditEntryResponse, type AuditEventDto } from '@/api/types'
import { formatDateTime } from '@/utils/format'

const { t } = useI18n()

const PAGE_SIZE = 50

/** 时间范围（datetime-local 成对输入的值形态；空串 = 不过滤）。 */
const sinceLocal = ref('')
const untilLocal = ref('')
const userFilter = ref('')
const projectFilter = ref('')
const eventFilter = ref<AuditEventDto | ''>('')

const entries = ref<AuditEntryResponse[] | null>(null)
const errorMessage = ref('')
/** 403（非全局 admin）→ admin-only 退化态：不渲染过滤条/表格。 */
const adminOnly = ref(false)
const loading = ref(false)
const offset = ref(0)

/** 下一页可用：本页恰满 limit 即可能还有更多（无 total，保守判定）。 */
const hasNext = computed(() => entries.value != null && entries.value.length === PAGE_SIZE)
/** 上一页可用：offset > 0。 */
const hasPrev = computed(() => offset.value > 0)

/** 回放卡副标（本页条数；与机密/构建机卡副标同形态）。 */
const countText = computed(() =>
  entries.value != null ? t('audit.count', { n: entries.value.length }) : '',
)

/** 事件类型下拉选项（全部 + 各事件；NSelect value 为空串 = 全部）。 */
const eventOptions = computed(() => [
  { label: t('audit.allEvents'), value: '' },
  ...AUDIT_EVENTS.map((e) => ({ label: t(eventLabelKey(e)), value: e })),
])

/** 事件类型 → 胶囊徽章类（语义色与既有 badge 全集对应：认证事件蓝/绿/
 *  红按结果、删除/禁用红、建立/启用绿、覆写/重置橙、其余中性）。 */
function eventBadgeClass(event: AuditEventDto): string {
  switch (event) {
    case 'login_success':
    case 'user_created':
    case 'user_enabled':
    case 'pat_created':
    case 'project_created':
    case 'agent_created':
    case 'agent_registered':
    case 'secret_created':
      return 'success'
    case 'login_failure':
    case 'user_disabled':
    case 'pat_revoked':
    case 'agent_disabled':
    case 'secret_deleted':
    case 'upgrade_package_deleted':
      return 'failed'
    case 'secret_overwritten':
    case 'password_reset':
    case 'scm_credential_set':
    case 'member_roles_changed':
    case 'upgrade_command_issued':
      return 'warning'
    default:
      // logout / agent_enabled / upgrade_package_uploaded 等中性事件。
      return 'neutral'
  }
}

/** 事件类型人读标签键（auditEvent.<event>，与 buildStatus/jobStatus 同纪律）。 */
function eventLabelKey(event: AuditEventDto): string {
  return `auditEvent.${event}`
}

onMounted(() => {
  void load()
})

/** datetime-local 值（分秒可选——`YYYY-MM-DDTHH:mm` 前缀即合法）→ Unix 毫秒。
 *  空串返回 undefined（不过滤该边界）。 */
function localToMs(value: string): number | undefined {
  if (value === '') return undefined
  const ms = new Date(value).getTime()
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

/** 过滤器是否全空（空态下隐藏重置按钮——无可重置的事实时不摆死按钮）。 */
const hasActiveFilter = computed(
  () =>
    sinceLocal.value !== '' ||
    untilLocal.value !== '' ||
    userFilter.value.trim() !== '' ||
    projectFilter.value.trim() !== '' ||
    eventFilter.value !== '',
)

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
      since: localToMs(sinceLocal.value),
      until: localToMs(untilLocal.value),
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

/** detail JSON 人读形态（机密事件只记名，值形态永不出现）。 */
function detailText(detail: AuditEntryResponse['detail']): string {
  if (detail == null) return ''
  return JSON.stringify(detail)
}
</script>

<template>
  <div class="admin-page audit-page">
    <!-- 403 退化态：仅全局管理员可见（审计仅全局 admin 可读）。 -->
    <p v-if="adminOnly" class="form-hint" data-testid="audit-admin-only">{{ t('admin.adminOnly') }}</p>

    <template v-else>
      <!-- 加载失败：整页报错 + 重试（事实态纪律）。 -->
      <n-alert v-if="errorMessage" type="error" :title="errorMessage" role="alert" data-testid="audit-error">
        <n-button size="small" name="audit-retry" @click="load">
          <template #icon>
            <n-icon :component="RefreshOutline" />
          </template>
          {{ t('audit.retry') }}
        </n-button>
      </n-alert>

      <!-- 首载骨架屏（数据到达后替换）。 -->
      <div v-else-if="loading && entries == null" class="audit-skeleton" data-testid="audit-skeleton">
        <n-skeleton text height="32px" width="520px" class="audit-skeleton-row" />
        <n-skeleton text :repeat="6" height="44px" class="audit-skeleton-row" />
      </div>

      <template v-else>
        <!-- 过滤工具栏（原型 toolbar-row 形态）：时间区间 + 操作人 + 项目 +
             事件类型，显式应用/重置（显式提交——部分过滤组合代价高，不自动打）。 -->
        <div class="toolbar-row audit-filters">
          <div class="audit-filter-fields">
            <input
              v-model="sinceLocal"
              type="datetime-local"
              class="audit-input audit-datetime"
              name="audit-since"
              :aria-label="t('audit.since')"
            />
            <span class="audit-range-sep">–</span>
            <input
              v-model="untilLocal"
              type="datetime-local"
              class="audit-input audit-datetime"
              name="audit-until"
              :aria-label="t('audit.until')"
            />
            <input
              v-model="userFilter"
              type="text"
              class="audit-input"
              name="audit-user"
              :placeholder="t('audit.userPlaceholder')"
              :aria-label="t('audit.user')"
            />
            <input
              v-model="projectFilter"
              type="text"
              class="audit-input"
              name="audit-project"
              :placeholder="t('audit.projectPlaceholder')"
              :aria-label="t('audit.project')"
            />
            <n-select
              v-model:value="eventFilter"
              :options="eventOptions"
              class="audit-event-select"
              :virtual-scroll="false"
              :aria-label="t('audit.event')"
            />
          </div>
          <div class="audit-filter-actions">
            <button type="button" class="btn-outline blue" name="audit-apply" @click="applyFilters">
              {{ t('audit.apply') }}
            </button>
            <button
              v-if="hasActiveFilter"
              type="button"
              class="btn-outline"
              name="audit-clear"
              @click="clearFilters"
            >
              {{ t('audit.clear') }}
            </button>
          </div>
        </div>

        <!-- 审计回放卡（时间倒序回放：行序即响应序，信任契约的倒序）。 -->
        <section class="sisy-card audit-table-card" aria-label="audit entries">
          <div class="card-header">
            <div>
              <h2 class="card-title">{{ t('audit.listTitle') }}</h2>
              <div v-if="countText" class="card-subtitle">{{ countText }}</div>
            </div>
          </div>

          <div v-if="loading" class="card-skeleton">
            <n-skeleton text :repeat="4" height="40px" />
          </div>

          <div v-else-if="entries && entries.length === 0" class="audit-empty" data-testid="audit-empty">
            <n-empty :description="t('audit.empty')" />
          </div>

          <template v-else-if="entries">
            <div class="audit-thead">
              <span class="ac-time">{{ t('audit.colTime') }}</span>
              <span class="ac-actor">{{ t('audit.colActor') }}</span>
              <span class="ac-event">{{ t('audit.colEvent') }}</span>
              <span class="ac-project">{{ t('audit.colProject') }}</span>
              <span class="ac-detail">{{ t('audit.colDetail') }}</span>
            </div>
            <div v-for="row in entries" :key="row.id" class="audit-row" :data-testid="`audit-row-${row.id}`">
              <span class="ac-time mono">{{ formatDateTime(row.ts) }}</span>
              <span class="ac-actor">{{ row.actor }}</span>
              <span class="ac-event">
                <span class="badge" :class="eventBadgeClass(row.event)">{{ t(eventLabelKey(row.event)) }}</span>
              </span>
              <span class="ac-project">{{ row.project ?? '—' }}</span>
              <span class="ac-detail">
                <code
                  v-if="row.detail != null"
                  class="audit-detail mono"
                  :title="detailText(row.detail)"
                >{{ detailText(row.detail) }}</code>
                <span v-else class="audit-detail-none">—</span>
              </span>
            </div>

            <!-- 前后翻页（无 total：按本页条数 == limit 判下一页、offset > 0 判上一页）。 -->
            <div v-if="entries.length > 0" class="audit-pagination">
              <button type="button" class="btn-outline" name="audit-prev" :disabled="!hasPrev" @click="prevPage">
                {{ t('audit.prev') }}
              </button>
              <button type="button" class="btn-outline" name="audit-next" :disabled="!hasNext" @click="nextPage">
                {{ t('audit.next') }}
              </button>
            </div>
          </template>
        </section>
      </template>
    </template>
  </div>
</template>

<style scoped>
.audit-page {
  gap: 16px;
}

.audit-skeleton {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.audit-skeleton-row {
  width: 100%;
}

/* 工具栏（原型 toolbar-row）。 */
.toolbar-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  flex-wrap: wrap;
}

.audit-filter-fields {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}

.audit-filter-actions {
  display: flex;
  align-items: center;
  gap: 8px;
}

/* 原生输入（datetime-local/文本）：定稿工具栏小输入形态。 */
.audit-input {
  height: 32px;
  padding: 0 10px;
  border-radius: var(--sisy-radius-small);
  border: 1px solid var(--sisy-color-border);
  background: var(--sisy-color-surface);
  color: var(--sisy-color-text);
  font-family: inherit;
  font-size: 12px;
  outline: none;
  transition: border-color 0.15s;
}

.audit-input:focus {
  border-color: var(--sisy-color-primary);
}

.audit-datetime {
  width: 190px;
}

.audit-input[type='text'] {
  width: 150px;
}

.audit-range-sep {
  color: var(--sisy-color-text-tertiary);
  font-size: 12px;
}

.audit-event-select {
  width: 170px;
}

/* 回放卡。 */
.audit-table-card {
  min-width: 0;
}

.card-subtitle {
  font-size: 12px;
  color: var(--sisy-color-text-secondary);
  margin-top: 2px;
}

.card-skeleton {
  padding: 0 20px 16px;
}

/* 表头（原型 table-head 形态）。 */
.audit-thead {
  display: flex;
  align-items: center;
  padding: 0 20px;
  height: 40px;
  border-top: 1px solid var(--sisy-color-border);
  border-bottom: 1px solid var(--sisy-color-border);
}

.audit-thead span {
  font-size: 12px;
  font-weight: 500;
  color: var(--sisy-color-text-secondary);
}

.audit-row {
  display: flex;
  align-items: center;
  padding: 0 20px;
  min-height: 44px;
  border-bottom: 1px solid var(--sisy-color-border-light);
  transition: background 0.15s;
}

.audit-row:last-of-type {
  border-bottom: none;
}

.audit-row:hover {
  background: var(--sisy-color-bg);
}

.ac-time {
  width: 170px;
  flex-shrink: 0;
  font-size: 12px;
  color: var(--sisy-color-text-secondary);
}

.ac-actor {
  width: 110px;
  flex-shrink: 0;
  font-size: 13px;
  font-weight: 500;
  color: var(--sisy-color-text);
}

.ac-event {
  width: 140px;
  flex-shrink: 0;
  display: flex;
}

.ac-project {
  width: 130px;
  flex-shrink: 0;
  font-size: 13px;
  color: var(--sisy-color-text);
}

.ac-detail {
  flex: 1;
  min-width: 0;
  display: flex;
  align-items: center;
}

.audit-detail {
  max-width: 100%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 11px;
  color: var(--sisy-color-text-secondary);
  cursor: default;
}

.audit-detail-none {
  color: var(--sisy-color-text-tertiary);
  font-size: 13px;
}

.mono {
  font-family: ui-monospace, 'Cascadia Code', Consolas, monospace;
  word-break: break-all;
}

/* 翻页收在卡片尾部（右对齐，与既有列表分页同形态）。 */
.audit-pagination {
  display: flex;
  gap: 8px;
  justify-content: flex-end;
  padding: 12px 20px 16px;
}

/* 空态。 */
.audit-empty {
  padding: 24px 0 32px;
}

/* 平板档（~768px）：详情列最先收起（时间/操作人/事件恒在——回放锚点）。 */
@media (max-width: 900px) {
  .ac-detail {
    display: none;
  }

  .audit-row,
  .audit-thead {
    min-height: 44px;
  }
}

@media (max-width: 780px) {
  .ac-project {
    display: none;
  }
}
</style>

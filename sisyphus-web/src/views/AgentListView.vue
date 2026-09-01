<script setup lang="ts">
// 构建机页（原型页三，spec #99，ADR-0008/0010/0017 数据面不变）：
// 四张指标卡（总数/在线/离线/异常）+ 资源表（状态徽章 / 槽位进度 / CPU /
// 内存 / 磁盘进度 / 当前任务 / 运行时长 / 最后心跳 / 操作）。
//
// 与原型的字段映射（CPU/内存自契约票 #102 起为真数据）：
// - 原型 CPU 列 → cpu_usage 利用率（心跳上报真值；未上报「—」）。
// - 原型 内存列 → memory_usage 利用率（同上）。
// - 原型 磁盘列 → disk_usage 卷聚合进度条（used / total 字节）。
// - 槽位列 = active_jobs / max_concurrency（调度真值，非原型映射）。
// - 温度类数据 v1 裁定不做（契约票 #102），不造假。
//
// 功能沿用既有流（票 B4-T5 / #94），定稿调整（票 #106）：
// - 列表 `GET /agents`（全局 admin；403 → admin-only 退化态）。
// - 接入构建机（顶栏 CTA `?create=1` 或空态按钮）→ 建条目弹窗 → 一次性
//   token + 注册码 + 按 OS 复制注册命令；页内点 CTA 同样弹窗（watch 查询参，
//   M2）。
// - 停用/启用 `PATCH { disabled }`：成功行内刷新；失败 toast 行内感知，
//   不再整页报错（M4）。
// - 离线机器 CPU/内存显示「—」（最后上报值已过期，不造假；磁盘可保留，
//   M5）；平板档降级次要列（M5/G2）。
// - 编辑槽位/标签 `PATCH`；行/详情进 Agent 详情页。
// - 顶栏搜索（`?q=`）按名称过滤。
// - 排空/版本不兼容机器就地「去升级」（M7 后置项，票 #111）：动作列深链
//   升级页并预选该机器为单台目标。

import { computed, h, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'
import {
  NAlert,
  NButton,
  NCode,
  NDataTable,
  NDescriptions,
  NDescriptionsItem,
  NEmpty,
  NForm,
  NFormItem,
  NIcon,
  NInput,
  NModal,
  NSelect,
  NSkeleton,
  NSwitch,
  useMessage,
  type DataTableColumns,
} from 'naive-ui'
import { ClipboardOutline } from '@vicons/ionicons5'

import { agentsApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import type { AgentResponse, PatchAgentRequest } from '@/api/types'
import { formatBytes, formatDuration, relativeAge, relativeAgeKey } from '@/utils/format'
import { buildAgentRegisterCommand, type AgentTargetOs } from '@/utils/agentCommand'
import { formatLabelLines, parseLabelLines } from '@/utils/agentLabels'

const { t } = useI18n()
const route = useRoute()
const router = useRouter()
const message = useMessage()

const agents = ref<AgentResponse[] | null>(null)
const loading = ref(true)
const listError = ref('')
/** 403（非全局 admin）→ admin-only 退化态：不报错、不渲染表/动作。 */
const adminOnly = ref(false)

/** 建条目弹窗（名 + 自定义标签 + 槽位）。 */
const showForm = ref(false)
const newName = ref('')
const newLabels = ref('')
/** 并发槽位（NInput value 恒为 string；parseConcurrency 归一为 number）。 */
const newConcurrency = ref('1')
const creating = ref(false)
const createError = ref('')
/** 建条目响应：一次性 token + 注册码（明文仅此一次，展示后即清）。 */
const createdCreds = ref<{
  token: string
  registerCode: string
  agentName: string
} | null>(null)
const targetOs = ref<AgentTargetOs>('linux')

/** 编辑弹窗（槽位 + 自定义标签；editingAgent 非空即开）。 */
const editingAgent = ref<AgentResponse | null>(null)
const editLabels = ref('')
const editConcurrency = ref('1')
const savingEdit = ref(false)
const editError = ref('')

/** 停用/启用 busy（按名标记，开关转圈）。 */
const togglingName = ref<string | null>(null)

/** 加载时间锚（运行时长/相对心跳的统一基准）。 */
const now = ref(Date.now())

onMounted(() => {
  void load()
})

// 顶栏 CTA `?create=1` → 打开建条目弹窗（收编入口，不改流）。watch 而非
// onMounted 读参（票 #106，M2）：页内点 CTA 时组件不重挂载，仅 onMounted
// 读参会导致弹窗不出现；直链进入同样生效。
watch(
  () => route.query.create,
  (v) => {
    if (v === '1') {
      showForm.value = true
      void router.replace({ query: { ...route.query, create: undefined } })
    }
  },
  { immediate: true },
)

const canCreate = computed(() => newName.value.trim() !== '' && !creating.value)

/** 按 OS 复制即用注册命令（注入一次性注册码；与 SetupView 同源）。 */
const registerCommand = computed(() =>
  buildAgentRegisterCommand(targetOs.value, createdCreds.value?.registerCode ?? ''),
)

const osOptions = [
  { label: 'Linux / macOS', value: 'linux' },
  { label: 'Windows', value: 'windows' },
]

/** 加载 Agent 清单（全局 admin 专属）。403 → admin-only 退化；其它失败 →
 *  就地错误。 */
async function load(): Promise<void> {
  listError.value = ''
  adminOnly.value = false
  try {
    agents.value = await agentsApi.list()
    now.value = Date.now()
  } catch (err) {
    if (err instanceof ApiError && err.status === 403) {
      agents.value = null
      adminOnly.value = true
      return
    }
    agents.value = null
    listError.value = describeSubmitError(err)
  } finally {
    loading.value = false
  }
}

// ===== 指标卡（原型 4 卡：总数/在线/离线/异常） =====

const metrics = computed(() => {
  const list = agents.value ?? []
  const enabled = list.filter((a) => !a.disabled)
  const online = enabled.filter((a) => a.online).length
  const offline = enabled.length - online
  const abnormal = enabled.filter((a) => a.draining || !a.version_compatible).length
  const disabled = list.length - enabled.length
  const onlinePct = enabled.length > 0 ? Math.round((online / enabled.length) * 100) : 0
  return { total: list.length, online, offline, abnormal, disabled, onlinePct }
})

// ===== 顶栏搜索（`?q=`） =====

const searchQuery = computed(() => (typeof route.query.q === 'string' ? route.query.q.trim() : ''))

const visibleAgents = computed(() => {
  const list = agents.value ?? []
  const q = searchQuery.value.toLowerCase()
  if (q === '') return list
  return list.filter((a) => a.name.toLowerCase().includes(q))
})

// ===== 展示派生（原型徽章 + 进度条） =====

type MachineBadge = 'building' | 'online' | 'offline' | 'warning' | 'failed' | 'neutral'

/** 展示态（优先级沿用 ADR-0017 派生 + 停用最高，追加「构建中」细分）：
 *  停用 > 版本不兼容 > 排空 > 离线 > 构建中 > 在线。 */
function displayBadge(agent: AgentResponse): MachineBadge {
  if (agent.disabled) return 'neutral'
  if (!agent.version_compatible) return 'failed'
  if (agent.online && agent.draining) return 'warning'
  if (!agent.online) return 'offline'
  if (agent.active_jobs > 0) return 'building'
  return 'online'
}

function badgeClass(state: MachineBadge): string {
  switch (state) {
    case 'online':
      return 'success'
    case 'building':
      return 'building'
    case 'offline':
      return 'offline'
    case 'warning':
      return 'draining'
    case 'failed':
      return 'failed'
    default:
      return 'neutral'
  }
}

function badgeLabel(state: MachineBadge): string {
  switch (state) {
    case 'online':
      return t('agents.stateOnline')
    case 'building':
      return t('agents.stateBuilding')
    case 'offline':
      return t('agents.stateOffline')
    case 'warning':
      return t('agents.stateDraining')
    case 'failed':
      return t('agents.stateIncompatible')
    default:
      return t('agents.stateDisabled')
  }
}

/** 槽位占用（调度真值列）：百分比 + 红色阈值 90%。 */
function slotUsage(agent: AgentResponse): { pct: number; red: boolean } {
  const pct =
    agent.max_concurrency > 0
      ? Math.round((agent.active_jobs / agent.max_concurrency) * 100)
      : 0
  return { pct: Math.min(100, pct), red: pct >= 90 }
}

/** 利用率文本（CPU/内存列，契约票 #102）：未上报（null）→「—」。离线
 *  机器一律「—」（票 #106，M5：最后上报值已过期，实时利用率不造假；
 *  磁盘可保留最后上报值）。 */
function usageText(value: number | null, online: boolean): string {
  return online && value != null ? `${value}%` : '—'
}

/** 利用率红色阈值 90%（与槽位/磁盘一致）。 */
function usageRed(value: number | null, online: boolean): boolean {
  return online && value != null && value >= 90
}

/** CPU/内存列单元格（契约票 #102 真形态消费；列工厂消双列逐字重复）。 */
function usageCell(value: number | null, agent: AgentResponse): ReturnType<typeof h> {
  return h(
    'span',
    { class: `machine-cell${usageRed(value, agent.online) ? ' red' : ''}` },
    usageText(value, agent.online),
  )
}

/** 磁盘占用（卷聚合；未上报为 null）。 */
function diskUsage(agent: AgentResponse): { pct: number; red: boolean; usedText: string; totalText: string } | null {
  const volumes = agent.disk_usage?.volumes ?? []
  const total = volumes.reduce((sum, v) => sum + v.total_bytes, 0)
  if (total <= 0) return null
  const free = volumes.reduce((sum, v) => sum + v.free_bytes, 0)
  const used = Math.max(0, total - free)
  const pct = Math.round((used / total) * 100)
  return {
    pct,
    red: pct >= 90,
    usedText: formatBytes(used),
    totalText: formatBytes(total),
  }
}

/** 当前任务列：离线 → 无；有在途 → n 个任务；否则空闲。 */
function taskText(agent: AgentResponse): string {
  if (!agent.online) return t('agents.taskNone')
  if (agent.active_jobs > 0) return t('agents.tasksRunning', { n: agent.active_jobs })
  return t('agents.idle')
}

function lastSeenText(agent: AgentResponse): string {
  if (!agent.last_seen_at) return t('agents.neverSeen')
  const age = relativeAge(agent.last_seen_at, now.value)
  return t(relativeAgeKey(age), { n: age.n })
}

/** 运行时长：天级以上收敛到「n d n h」，避免「12d 0s」的零秒尾缀。 */
function uptimeText(agent: AgentResponse): string {
  const ms = Math.max(0, now.value - agent.created_at)
  if (ms >= 86_400_000) {
    return `${Math.floor(ms / 86_400_000)}d ${Math.floor((ms % 86_400_000) / 3_600_000)}h`
  }
  if (ms >= 3_600_000) {
    return `${Math.floor(ms / 3_600_000)}h ${Math.floor((ms % 3_600_000) / 60_000)}m`
  }
  return formatDuration(ms)
}

// ===== 既有动作流（票 B4-T5 不变） =====

/** 并发槽位（string）→ number（空 = 不传，后端缺省/保留原值）。 */
function parseConcurrency(value: string | null | undefined): number | undefined {
  if (value == null) return undefined
  const trimmed = value.trim()
  if (trimmed === '') return undefined
  const n = Number(trimmed)
  return Number.isFinite(n) ? n : undefined
}

/** 建条目：`POST /agents` → 一次性 token + 注册码（明文仅此一次）+ 刷新列表。 */
async function createAgent(): Promise<void> {
  createError.value = ''
  creating.value = true
  try {
    const created = await agentsApi.create({
      name: newName.value.trim(),
      custom_labels: parseLabelLines(newLabels.value),
      max_concurrency: parseConcurrency(newConcurrency.value),
    })
    createdCreds.value = {
      token: created.token,
      registerCode: created.register_code,
      agentName: created.agent.name,
    }
    showForm.value = false
    newName.value = ''
    newLabels.value = ''
    newConcurrency.value = '1'
    await load()
  } catch (err) {
    createError.value = describeSubmitError(err)
  } finally {
    creating.value = false
  }
}

/** 丢弃一次性凭据面板（token/注册码此后任何端点都无法找回）。 */
function dismissCreds(): void {
  createdCreds.value = null
}

/** 复制文本到剪贴板（不可用时静默——内容在 NCode 框内可手选）。 */
async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text)
    message.success(t('agents.copied'))
  } catch {
    // 剪贴板 API 不可用（非安全上下文等）：不打断流程。
  }
}

/** 打开编辑弹窗（预填当前槽位/标签）。 */
function startEdit(agent: AgentResponse): void {
  editingAgent.value = agent
  editLabels.value = formatLabelLines(agent.custom_labels)
  editConcurrency.value = String(agent.max_concurrency)
  editError.value = ''
}

function cancelEdit(): void {
  editingAgent.value = null
}

/** 保存编辑：`PATCH` 槽位 + 自定义标签（整组替换）。空槽位回落原值（不误改）。 */
async function saveEdit(): Promise<void> {
  const agent = editingAgent.value
  if (!agent) return
  editError.value = ''
  savingEdit.value = true
  try {
    const req: PatchAgentRequest = {
      max_concurrency: parseConcurrency(editConcurrency.value) ?? agent.max_concurrency,
      custom_labels: parseLabelLines(editLabels.value),
    }
    await agentsApi.patch(agent.name, req)
    editingAgent.value = null
    await load()
  } catch (err) {
    editError.value = describeSubmitError(err)
  } finally {
    savingEdit.value = false
  }
}

/** 停用/启用切换：`PATCH` { disabled }，停用即踢线。成功行内刷新；失败
 *  toast 行内感知（票 #106，M4：不再整页报错——开关失败只影响该行）。 */
async function toggleDisabled(agent: AgentResponse): Promise<void> {
  togglingName.value = agent.name
  try {
    await agentsApi.patch(agent.name, { disabled: !agent.disabled })
    await load()
  } catch (err) {
    message.error(describeSubmitError(err))
  } finally {
    togglingName.value = null
  }
}

function openDetail(agent: AgentResponse): void {
  void router.push({ name: 'agent-detail', params: { name: agent.name } })
}

// ===== 平板/窄桌面档降级（票 #106，G2）：列集合按视口剔除次要列 =====
// 单一断点表：scroll-x 与剔除列集合同源同档（全量 1140 恰收进 1440 桌面
// 内容区；≤1280 去 运行时长/最后心跳；≤1120 再去 CPU/内存），改档一处
// 生效。直接过滤列而非 CSS 隐藏——NDataTable colgroup 仍会给隐藏列分配
// 宽度、等比挤压可见列。
const windowWidth = ref(typeof window !== 'undefined' ? window.innerWidth : 1280)
function onWindowResize(): void {
  windowWidth.value = window.innerWidth
}
onMounted(() => window.addEventListener('resize', onWindowResize))
onBeforeUnmount(() => window.removeEventListener('resize', onWindowResize))

interface TableBreakpoint {
  /** 视口宽上限（含）；末档 Infinity 兜底。 */
  max: number
  scrollX: number
  hidden: string[]
}
const TABLE_BREAKPOINTS: TableBreakpoint[] = [
  { max: 1120, scrollX: 780, hidden: ['runtime', 'last_seen_at', 'cpu', 'mem'] },
  { max: 1280, scrollX: 940, hidden: ['runtime', 'last_seen_at'] },
  { max: Infinity, scrollX: 1140, hidden: [] },
]
const tableBreakpoint = computed(
  () => TABLE_BREAKPOINTS.find((b) => windowWidth.value <= b.max) ?? TABLE_BREAKPOINTS[0]!,
)

/** 资源表列（NDataTable + 原型徽章/进度条渲染）。 */
const columns = computed<DataTableColumns<AgentResponse>>(() => {
  const all: DataTableColumns<AgentResponse> = [
  {
    title: t('agents.colMachine'),
    key: 'name',
    // M3（票 #106）：给足列宽，machine-name-btn 已 nowrap，不再折行。
    width: 100,
    render: (row) =>
      h(
        'button',
        { type: 'button', class: 'machine-name-btn', onClick: () => openDetail(row) },
        row.name,
      ),
  },
  {
    title: t('agents.colState'),
    key: 'state',
    width: 90,
    render: (row) => {
      const state = displayBadge(row)
      return h('span', { class: `badge ${badgeClass(state)}` }, badgeLabel(state))
    },
  },
  {
    title: t('agents.colSlots'),
    key: 'slots',
    width: 140,
    render: (row) => {
      const usage = slotUsage(row)
      return h('div', { class: 'usage-cell' }, [
        h('div', { class: 'usage-row' }, [
          h('span', { class: 'track' }, [
            h('span', {
              class: `fill${usage.red ? ' red' : ''}`,
              style: { width: `${usage.pct}%` },
            }),
          ]),
          h('span', { class: `pct${usage.red ? ' red' : ''}` }, `${usage.pct}%`),
        ]),
        h('span', { class: 'usage-sub' }, `${row.active_jobs} / ${row.max_concurrency}`),
      ])
    },
  },
  {
    title: t('agents.colCpu'),
    key: 'cpu',
    width: 80,
    render: (row) => usageCell(row.cpu_usage, row),
  },
  {
    title: t('agents.colMem'),
    key: 'mem',
    width: 80,
    render: (row) => usageCell(row.memory_usage, row),
  },
  {
    title: t('agents.colDisk'),
    key: 'disk',
    width: 150,
    render: (row) => {
      const disk = diskUsage(row)
      if (!disk) {
        return h('span', { class: 'offline-cell' }, '—')
      }
      return h('div', { class: 'usage-cell' }, [
        h('div', { class: 'usage-row' }, [
          h('span', { class: 'track' }, [
            h('span', {
              class: `fill${disk.red ? ' red' : ''}`,
              style: { width: `${disk.pct}%` },
            }),
          ]),
          h('span', { class: `pct${disk.red ? ' red' : ''}` }, `${disk.pct}%`),
        ]),
        h('span', { class: 'usage-sub' }, `${disk.usedText} / ${disk.totalText}`),
      ])
    },
  },
  {
    title: t('agents.colTask'),
    key: 'task',
    width: 120,
    render: (row) =>
      h(
        'span',
        { class: `machine-task${row.online && row.active_jobs > 0 ? '' : ' gray'}` },
        taskText(row),
      ),
  },
  {
    title: t('agents.colRuntime'),
    key: 'runtime',
    width: 100,
    render: (row) => h('span', { class: 'machine-cell' }, uptimeText(row)),
  },
  {
    title: t('agents.colLastSeen'),
    key: 'last_seen_at',
    width: 100,
    render: (row) => h('span', { class: 'machine-cell gray' }, lastSeenText(row)),
  },
  {
    title: t('agents.colActions'),
    key: 'actions',
    width: 200,
    render: (row) =>
      h('div', { class: 'machine-row-actions' }, [
        // M7（票 #106 后置到 #111）：排空/版本不兼容机器就地「去升级」入口
        // ——深链 /admin/upgrade?agent=<name> 预选单台目标。
        ...(row.draining || !row.version_compatible
          ? [
              h(
                NButton,
                {
                  size: 'tiny',
                  type: 'warning',
                  secondary: true,
                  name: `agent-upgrade-${row.name}`,
                  onClick: () =>
                    void router.push({ path: '/admin/upgrade', query: { agent: row.name } }),
                },
                { default: () => t('agents.upgradeAction') },
              ),
            ]
          : []),
        h(NSwitch, {
          size: 'small',
          class: 'machine-toggle',
          value: !row.disabled,
          loading: togglingName.value === row.name,
          'onUpdate:value': () => void toggleDisabled(row),
        }),
        h(
          NButton,
          { size: 'tiny', onClick: () => startEdit(row) },
          { default: () => t('agents.edit') },
        ),
        h(
          NButton,
          { size: 'tiny', quaternary: true, type: 'primary', onClick: () => openDetail(row) },
          { default: () => t('agents.detailAction') },
        ),
      ]),
  },
    ]

  // 按列 key 剔除（动作列恒保留，不能按位置切尾）。
  const { hidden } = tableBreakpoint.value
  return hidden.length === 0
    ? all
    : all.filter((c) => !('key' in c) || !hidden.includes(String(c.key)))
})

const rowKey = (row: AgentResponse): string => row.name
</script>

<template>
  <div class="machines-page">
    <n-alert v-if="listError" type="error" :title="listError" role="alert" />

    <!-- 403 退化态：仅全局管理员可见（Agent 管理面全局 admin 专属）。 -->
    <p v-if="adminOnly" class="form-hint">{{ t('agents.adminOnly') }}</p>

    <!-- 首载骨架屏（与概览/流水线同纪律——数据到达后替换）。 -->
    <div v-if="loading && !listError" class="machines-skeleton" data-testid="agents-skeleton">
      <div class="metric-row">
        <n-skeleton v-for="i in 4" :key="i" text :repeat="3" height="40px" class="machines-skeleton-card" />
      </div>
      <n-skeleton text :repeat="5" height="44px" class="machines-skeleton-row" />
    </div>

    <template v-else-if="!adminOnly && !listError">
      <!-- 空列表：NEmpty + 注册引导（新建条目后在构建机执行注册命令接入）。 -->
      <div v-if="agents && agents.length === 0" class="machines-empty">
        <n-empty :description="t('agents.empty')">
          <template #extra>
            <p class="form-hint">{{ t('agents.emptyHint') }}</p>
            <n-button type="primary" name="agent-new-empty" @click="showForm = true">
              {{ t('agents.accessMachine') }}
            </n-button>
          </template>
        </n-empty>
      </div>

      <template v-else-if="agents && agents.length > 0">
        <!-- 指标卡行（原型 metrics-row）。 -->
        <section class="metric-row" aria-label="machine metrics">
          <div class="metric-card">
            <span class="metric-label">{{ t('agents.mTotal') }}</span>
            <span class="metric-value">{{ metrics.total }}</span>
            <span class="metric-sub">
              {{
                metrics.disabled > 0
                  ? t('agents.mTotalSubDisabled', { n: metrics.disabled })
                  : t('agents.mTotalSubAll')
              }}
            </span>
          </div>
          <div class="metric-card">
            <span class="metric-label">{{ t('agents.mOnline') }}</span>
            <span class="metric-value green">{{ metrics.online }}</span>
            <span class="metric-sub green">{{ t('agents.mOnlineSub', { pct: metrics.onlinePct }) }}</span>
          </div>
          <div class="metric-card">
            <span class="metric-label">{{ t('agents.mOffline') }}</span>
            <span class="metric-value gray">{{ metrics.offline }}</span>
            <span class="metric-sub">{{ t('agents.mOfflineSub') }}</span>
          </div>
          <div class="metric-card">
            <span class="metric-label">{{ t('agents.mAbnormal') }}</span>
            <span class="metric-value" :class="{ red: metrics.abnormal > 0 }">{{ metrics.abnormal }}</span>
            <span class="metric-sub">{{ t('agents.mAbnormalSub') }}</span>
          </div>
        </section>

        <!-- 资源表（原型 machine-table 卡片形态）。 -->
        <section class="sisy-card machine-table" aria-label="machine list">
          <div class="card-header">
            <div>
              <h2 class="card-title">{{ t('agents.machineList') }}</h2>
              <div class="card-subtitle">{{ t('agents.machineCount', { n: agents.length }) }}</div>
            </div>
          </div>
          <n-data-table
            :columns="columns"
            :data="visibleAgents"
            :row-key="rowKey"
            :bordered="false"
            :single-line="true"
            size="small"
            :scroll-x="tableBreakpoint.scrollX"
            class="machine-data-table"
          />
        </section>
      </template>
    </template>

    <!-- 建条目弹窗（名 + 自定义标签 + 槽位）。 -->
    <n-modal
      v-model:show="showForm"
      preset="card"
      :title="t('agents.newAgent')"
      style="width: 480px"
      :bordered="false"
    >
      <n-form label-placement="top" @submit.prevent="createAgent">
        <n-form-item :label="t('agents.name')" :show-require-mark="true">
          <n-input
            v-model:value="newName"
            :input-props="{ name: 'agent-name' }"
            :placeholder="t('agents.namePlaceholder')"
          />
        </n-form-item>
        <n-form-item :label="t('agents.customLabels')">
          <n-input
            v-model:value="newLabels"
            type="textarea"
            :rows="3"
            :input-props="{ name: 'agent-labels' }"
            :placeholder="t('agents.customLabelsPlaceholder')"
          />
        </n-form-item>
        <p class="form-hint">{{ t('agents.customLabelsHint') }}</p>
        <n-form-item :label="t('agents.maxConcurrency')">
          <n-input
            v-model:value="newConcurrency"
            :input-props="{ name: 'agent-concurrency', type: 'number', min: '1' }"
          />
        </n-form-item>
        <n-alert v-if="createError" type="error" :title="createError" role="alert" class="machines-modal-alert" />
        <div class="modal-actions">
          <n-button @click="showForm = false">{{ t('common.cancel') }}</n-button>
          <n-button
            type="primary"
            name="agent-create"
            :disabled="!canCreate"
            :loading="creating"
            @click="createAgent"
          >
            {{ creating ? t('agents.creating') : t('agents.create') }}
          </n-button>
        </div>
      </n-form>
    </n-modal>

    <!-- 一次性凭据弹窗（token + 注册码明文仅此一次 + 按 OS 复制命令）。 -->
    <n-modal
      :show="createdCreds !== null"
      preset="card"
      :title="t('agents.credentialsOneTime')"
      style="width: 560px"
      :bordered="false"
      :mask-closable="false"
      @update:show="(show: boolean) => { if (!show) dismissCreds() }"
    >
      <n-alert type="warning" :show-icon="true" class="machines-modal-alert">
        {{ t('agents.credentialsWarn') }}
      </n-alert>
      <n-descriptions :column="1" size="small" bordered class="machines-creds-desc">
        <n-descriptions-item :label="t('agents.name')">
          <span class="mono">{{ createdCreds?.agentName }}</span>
        </n-descriptions-item>
        <n-descriptions-item :label="t('agents.registerCodeLabel')">
          <n-code :code="createdCreds?.registerCode ?? ''" class="machines-cred-code" />
          <n-button
            size="tiny"
            quaternary
            type="primary"
            name="agent-copy-code"
            @click="copyText(createdCreds?.registerCode ?? '')"
          >
            <template #icon><n-icon :component="ClipboardOutline" /></template>
          </n-button>
        </n-descriptions-item>
        <n-descriptions-item :label="t('agents.agentTokenLabel')">
          <n-code :code="createdCreds?.token ?? ''" class="machines-cred-code" />
          <n-button
            size="tiny"
            quaternary
            type="primary"
            name="agent-copy-token"
            @click="copyText(createdCreds?.token ?? '')"
          >
            <template #icon><n-icon :component="ClipboardOutline" /></template>
          </n-button>
        </n-descriptions-item>
      </n-descriptions>
      <div class="machines-os-row">
        <span>{{ t('agents.targetOs') }}</span>
        <n-select v-model:value="targetOs" :options="osOptions" style="width: 180px" :virtual-scroll="false" />
        <n-button size="small" name="agent-copy" @click="copyText(registerCommand)">
          {{ t('agents.copy') }}
        </n-button>
      </div>
      <n-code :code="registerCommand" language="bash" word-wrap class="machines-cmd-code" />
      <p class="form-hint">{{ t('agents.cmdNote') }}</p>
      <div class="modal-actions">
        <n-button type="primary" name="agent-creds-dismiss" @click="dismissCreds">
          {{ t('agents.credsDismiss') }}
        </n-button>
      </div>
    </n-modal>

    <!-- 编辑弹窗（槽位 + 自定义标签）。 -->
    <n-modal
      :show="editingAgent !== null"
      preset="card"
      :title="t('agents.editTitle')"
      style="width: 480px"
      :bordered="false"
      @update:show="(show: boolean) => { if (!show) cancelEdit() }"
    >
      <n-form label-placement="top" @submit.prevent="saveEdit">
        <n-form-item :label="t('agents.maxConcurrency')">
          <n-input
            v-model:value="editConcurrency"
            :input-props="{ name: 'edit-concurrency', type: 'number', min: '1' }"
          />
        </n-form-item>
        <n-form-item :label="t('agents.customLabels')">
          <n-input
            v-model:value="editLabels"
            type="textarea"
            :rows="3"
            :input-props="{ name: 'edit-labels' }"
            :placeholder="t('agents.customLabelsPlaceholder')"
          />
        </n-form-item>
        <p class="form-hint">{{ t('agents.customLabelsHint') }}</p>
        <n-alert v-if="editError" type="error" :title="editError" role="alert" class="machines-modal-alert" />
        <div class="modal-actions">
          <n-button @click="cancelEdit">{{ t('common.cancel') }}</n-button>
          <n-button type="primary" :loading="savingEdit" @click="saveEdit">
            {{ savingEdit ? t('agents.saving') : t('agents.save') }}
          </n-button>
        </div>
      </n-form>
    </n-modal>
  </div>
</template>

<style scoped>
.machines-page {
  display: flex;
  flex-direction: column;
  gap: 20px;
}

.machines-skeleton {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.machines-skeleton-card {
  width: 100%;
}

.machines-skeleton-row {
  width: 100%;
}

.machines-empty {
  padding: 48px 0;
}

.machine-table {
  padding-bottom: 4px;
}

.card-subtitle {
  font-size: 12px;
  color: var(--sisy-color-text-secondary);
  margin-top: 2px;
}

.usage-cell .usage-sub {
  font-size: 11px;
  font-weight: 500;
  color: var(--sisy-color-text-secondary);
}

.usage-cell .usage-sub .pct {
  font-size: 11px;
}

/* 表内单元格文字。 */
.machine-cell {
  font-size: 13px;
  color: var(--sisy-color-text);
}

.machine-cell.gray {
  color: var(--sisy-color-text-secondary);
}

/* CPU/内存利用率红色阈值（与进度条 pct.red 同语义色）。 */
.machine-cell.red {
  color: var(--sisy-color-danger-text);
}

.machine-task {
  font-size: 13px;
  color: var(--sisy-color-text);
}

.machine-task.gray {
  color: var(--sisy-color-text-secondary);
}

.offline-cell {
  font-size: 13px;
  color: var(--sisy-color-text-tertiary);
}

.machine-name-btn {
  border: none;
  background: none;
  padding: 0;
  cursor: pointer;
  text-align: left;
  font-weight: 600;
  font-size: 13px;
  font-family: inherit;
  color: var(--sisy-color-text);
  white-space: nowrap;
}

.machine-name-btn:hover {
  color: var(--sisy-color-primary);
  text-decoration: underline;
}

.machine-row-actions {
  display: flex;
  align-items: center;
  gap: 10px;
}

.mono {
  font-family: ui-monospace, 'Cascadia Code', Consolas, monospace;
  font-size: 12px;
  word-break: break-all;
}

.modal-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 8px;
}

.machines-modal-alert {
  margin-bottom: 8px;
}

.machines-creds-desc {
  margin-bottom: 4px;
}

.machines-os-row {
  display: flex;
  align-items: center;
  gap: 10px;
  margin: 12px 0 8px;
}

.machines-cmd-code {
  margin-top: 4px;
}
</style>

<script setup lang="ts">
// Agent 列表 + 建条目 + 停用/启用 + 编辑（ADR-0008/0010/0017，票 B4-T5）。
//
// 列表四态（在线/离线/排空/不兼容）由 `deriveAgentState`/`agentBadgeState` 派生
// （ADR-0017）；停用（disabled）是独立管理态，停用时徽标优先显示「停用」
// （停用即踢线，ADR-0008）。排空/不兼容 依赖后端排空标志与版本窗口字段，
// 当前 REST 契约未暴露 → 退化标注（与概览页 alert-degraded 同纪律）。
//
// - 列表：`GET /agents`（全局 admin 专属；403 → admin-only 退化态，不报错、
//   不渲染表/动作——对齐 overview store 的 403 退化纪律）。
// - 建条目：`POST /agents` → 一次性 token + 注册码（明文仅此一次，展示后即
//   丢弃）+ 按 OS 复制即用注册命令（`buildAgentRegisterCommand`，与 SetupView
//   同源，不复制漂移）+ 刷新列表。
// - 停用/启用：`PATCH /agents/{name}` { disabled }。
// - 编辑槽位/自定义标签：编辑弹窗 → `PATCH` { max_concurrency,
//   custom_labels }（整组替换）；key=value 形态校验由后端做（422 定位到
//   custom_labels，`describeSubmitError` 拼接清单就地展示）。
// - 点击行名 → Agent 详情页（全貌 + 工作区/缓存清理）。
// #94: 使用 Naive UI 组件重写——NDataTable（状态列 NTag 颜色编码）、
// 建条目/编辑改 NModal、一次性凭据 NCode + 复制按钮、停用/启用改 NSwitch、
// 空列表 NEmpty + 注册引导、NAlert 错误态、NSkeleton 首载骨架屏。

import { computed, h, onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
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
  NTag,
  useMessage,
  type DataTableColumns,
} from 'naive-ui'
import { ClipboardOutline } from '@vicons/ionicons5'

import { agentsApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import type { AgentResponse, PatchAgentRequest } from '@/api/types'
import {
  agentBadgeState,
  agentStateLabelKey,
  agentStateTagType,
} from '@/utils/agentState'
import { buildAgentRegisterCommand, type AgentTargetOs } from '@/utils/agentCommand'
import { formatLabelLines, parseLabelLines } from '@/utils/agentLabels'
import { formatDateTime } from '@/utils/format'

const { t } = useI18n()
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

onMounted(load)

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

/** 停用/启用切换：`PATCH` { disabled }，停用即踢线。 */
async function toggleDisabled(agent: AgentResponse): Promise<void> {
  togglingName.value = agent.name
  try {
    await agentsApi.patch(agent.name, { disabled: !agent.disabled })
    await load()
  } catch (err) {
    listError.value = describeSubmitError(err)
  } finally {
    togglingName.value = null
  }
}

function openDetail(agent: AgentResponse): void {
  void router.push({ name: 'agent-detail', params: { name: agent.name } })
}

/** NDataTable 列（状态列 NTag 色标；动作列 NSwitch 切换 + 编辑按钮）。 */
const columns = computed<DataTableColumns<AgentResponse>>(() => [
  {
    title: t('agents.name'),
    key: 'name',
    render: (row) =>
      h(
        'button',
        { type: 'button', class: 'agent-name-btn', onClick: () => openDetail(row) },
        row.name,
      ),
  },
  {
    title: t('agents.colState'),
    key: 'state',
    render: (row) => {
      const state = agentBadgeState(row)
      return h(
        NTag,
        { size: 'small', type: agentStateTagType(state), bordered: false },
        { default: () => t(agentStateLabelKey(state)) },
      )
    },
  },
  {
    title: t('agents.slotUsage'),
    key: 'slots',
    render: (row) => `${row.active_jobs} / ${row.max_concurrency}`,
  },
  {
    title: t('agents.lastSeen'),
    key: 'last_seen_at',
    render: (row) => (row.last_seen_at ? formatDateTime(row.last_seen_at) : t('agents.neverSeen')),
  },
  {
    title: t('agents.colActions'),
    key: 'actions',
    render: (row) =>
      h('div', { class: 'agent-row-actions' }, [
        h(NSwitch, {
          size: 'small',
          class: 'agent-toggle',
          value: !row.disabled,
          loading: togglingName.value === row.name,
          'onUpdate:value': () => void toggleDisabled(row),
        }),
        h(
          NButton,
          { size: 'small', onClick: () => startEdit(row) },
          { default: () => t('agents.edit') },
        ),
      ]),
  },
])

const rowKey = (row: AgentResponse): string => row.name
</script>

<template>
  <div class="agents-page">
    <div class="page-header">
      <h1 class="page-title">{{ t('routes.agents') }}</h1>
      <n-button
        v-if="!adminOnly"
        type="primary"
        name="agent-new"
        @click="showForm = true"
      >
        {{ t('agents.newAgent') }}
      </n-button>
    </div>

    <n-alert v-if="listError" type="error" :title="listError" role="alert" />

    <!-- 403 退化态：仅全局管理员可见（Agent 管理面全局 admin 专属）。 -->
    <p v-if="adminOnly" class="form-hint">{{ t('agents.adminOnly') }}</p>

    <!-- 首载骨架屏（与 #91/#93 同纪律——数据到达后替换）。 -->
    <div v-if="loading && !listError" class="agents-skeleton" data-testid="agents-skeleton">
      <n-skeleton v-for="i in 4" :key="i" text :repeat="2" height="28px" class="agents-skeleton-row" />
    </div>

    <!-- 空列表：NEmpty + 注册引导（新建条目后在构建机执行注册命令接入）。 -->
    <div v-else-if="!adminOnly && !listError && agents && agents.length === 0" class="agents-empty">
      <n-empty :description="t('agents.empty')">
        <template #extra>
          <p class="form-hint">{{ t('agents.emptyHint') }}</p>
          <n-button type="primary" name="agent-new-empty" @click="showForm = true">
            {{ t('agents.newAgent') }}
          </n-button>
        </template>
      </n-empty>
    </div>

    <!-- Agent 列表（按名排序）。 -->
    <n-data-table
      v-else-if="agents && agents.length > 0"
      :columns="columns"
      :data="agents"
      :row-key="rowKey"
      :bordered="false"
      :single-line="true"
      size="small"
      class="agents-table"
    />

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
        <n-alert v-if="createError" type="error" :title="createError" role="alert" class="agents-modal-alert" />
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
      <n-alert type="warning" :show-icon="true" class="agents-modal-alert">
        {{ t('agents.credentialsWarn') }}
      </n-alert>
      <n-descriptions :column="1" size="small" bordered class="agents-creds-desc">
        <n-descriptions-item :label="t('agents.name')">
          <span class="mono">{{ createdCreds?.agentName }}</span>
        </n-descriptions-item>
        <n-descriptions-item :label="t('agents.registerCodeLabel')">
          <n-code :code="createdCreds?.registerCode ?? ''" class="agents-cred-code" />
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
          <n-code :code="createdCreds?.token ?? ''" class="agents-cred-code" />
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
      <div class="agents-os-row">
        <span>{{ t('agents.targetOs') }}</span>
        <n-select v-model:value="targetOs" :options="osOptions" style="width: 180px" :virtual-scroll="false" />
        <n-button size="small" name="agent-copy" @click="copyText(registerCommand)">
          {{ t('agents.copy') }}
        </n-button>
      </div>
      <n-code :code="registerCommand" language="bash" word-wrap class="agents-cmd-code" />
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
        <n-alert v-if="editError" type="error" :title="editError" role="alert" class="agents-modal-alert" />
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
.agents-page {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.agents-skeleton {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin: 8px 0;
}

.agents-skeleton-row {
  width: 100%;
}

.agents-empty {
  padding: 32px 0;
}

/* 表格行内动作：开关 + 编辑按钮。 */
.agent-row-actions {
  display: flex;
  align-items: center;
  gap: 10px;
}

.agent-name-btn {
  border: none;
  background: none;
  padding: 0;
  cursor: pointer;
  text-align: left;
  font-weight: 600;
  color: inherit;
}

.agent-name-btn:hover {
  color: var(--n-text-color-link, #2b5797);
  text-decoration: underline;
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

.agents-modal-alert {
  margin-bottom: 8px;
}

.agents-os-row {
  display: flex;
  align-items: center;
  gap: 10px;
  margin: 12px 0 8px;
}

.agents-cmd-code {
  margin-top: 4px;
}
</style>

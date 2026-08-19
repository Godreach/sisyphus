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
// - 编辑槽位/自定义标签：行内展开表单 → `PATCH` { max_concurrency,
//   custom_labels }（整组替换）；key=value 形态校验由后端做（422 定位到
//   custom_labels，`describeSubmitError` 拼接清单就地展示）。
// - 点击行名 → Agent 详情页（全貌 + 工作区/缓存清理）。

import { computed, onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'

import { agentsApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import type { AgentResponse, PatchAgentRequest } from '@/api/types'
import {
  agentBadgeState,
  agentStateClass,
  agentStateLabelKey,
} from '@/utils/agentState'
import { buildAgentRegisterCommand, type AgentTargetOs } from '@/utils/agentCommand'
import { formatLabelLines, parseLabelLines } from '@/utils/agentLabels'
import { formatDateTime } from '@/utils/format'

const { t } = useI18n()
const router = useRouter()

const agents = ref<AgentResponse[] | null>(null)
const listError = ref('')
/** 403（非全局 admin）→ admin-only 退化态：不报错、不渲染表/动作。 */
const adminOnly = ref(false)

/** 建条目表单。 */
const showForm = ref(false)
const newName = ref('')
const newLabels = ref('')
/** 并发槽位（`<input type="number">` 经 Vue 自动转 number，故 ref 形态为
 *  number | string；parseConcurrency 归一）。 */
const newConcurrency = ref<number | string>(1)
const creating = ref(false)
const createError = ref('')
/** 建条目响应：一次性 token + 注册码（明文仅此一次，展示后即清）。 */
const createdCreds = ref<{
  token: string
  registerCode: string
  agentName: string
} | null>(null)
const targetOs = ref<AgentTargetOs>('linux')

/** 行内编辑（槽位 + 自定义标签）。 */
const editingName = ref<string | null>(null)
const editLabels = ref('')
const editConcurrency = ref<number | string>(1)
const savingEdit = ref(false)
const editError = ref('')

/** 停用/启用 busy（按名标记，禁对应按钮）。 */
const togglingName = ref<string | null>(null)

onMounted(load)

const canCreate = computed(() => newName.value.trim() !== '' && !creating.value)

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
  }
}

/** 并发槽位（number | string）→ number（空 = 不传，后端缺省/保留原值）。
 *  Vue 对 `<input type="number">` 自动转 number，故入参可能是 number；
 *  `String()` 归一两种形态，空串/空值回落 undefined。 */
function parseConcurrency(value: number | string | null | undefined): number | undefined {
  if (value == null) return undefined
  const trimmed = String(value).trim()
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
    newConcurrency.value = 1
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

/** 复制当前目标 OS 的注册命令到剪贴板（不可用时静默——命令在框内可手选）。 */
async function copyCommand(): Promise<void> {
  if (!createdCreds.value) return
  try {
    await navigator.clipboard.writeText(
      buildAgentRegisterCommand(targetOs.value, createdCreds.value.registerCode),
    )
  } catch {
    // 剪贴板 API 不可用（非安全上下文等）：不打断流程。
  }
}

/** 展开行内编辑表单（预填当前槽位/标签）。 */
function startEdit(agent: AgentResponse): void {
  editingName.value = agent.name
  editLabels.value = formatLabelLines(agent.custom_labels)
  editConcurrency.value = agent.max_concurrency
  editError.value = ''
}

function cancelEdit(): void {
  editingName.value = null
}

/** 保存编辑：`PATCH` 槽位 + 自定义标签（整组替换）。空槽位回落原值（不误改）。 */
async function saveEdit(agent: AgentResponse): Promise<void> {
  editError.value = ''
  savingEdit.value = true
  try {
    const req: PatchAgentRequest = {
      max_concurrency: parseConcurrency(editConcurrency.value) ?? agent.max_concurrency,
      custom_labels: parseLabelLines(editLabels.value),
    }
    await agentsApi.patch(agent.name, req)
    editingName.value = null
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
</script>

<template>
  <div class="agents-page">
    <div class="page-header">
      <h1 class="page-title">{{ t('routes.agents') }}</h1>
      <button
        v-if="!adminOnly"
        type="button"
        class="btn-primary"
        name="agent-new"
        @click="showForm = !showForm"
      >
        {{ t('agents.newAgent') }}
      </button>
    </div>

    <p v-if="listError" class="form-error" role="alert">{{ listError }}</p>

    <!-- 403 退化态：仅全局管理员可见（Agent 管理面全局 admin 专属）。 -->
    <p v-if="adminOnly" class="form-hint">{{ t('agents.adminOnly') }}</p>

    <!-- 建条目表单（名 + 自定义标签 + 槽位）。 -->
    <form v-if="showForm && !adminOnly" class="agent-form" @submit.prevent>
      <label class="field">
        <span>{{ t('agents.name') }}</span>
        <input v-model="newName" name="agent-name" :placeholder="t('agents.namePlaceholder')" />
      </label>
      <label class="field">
        <span>{{ t('agents.customLabels') }}</span>
        <textarea
          v-model="newLabels"
          name="agent-labels"
          rows="3"
          :placeholder="t('agents.customLabelsPlaceholder')"
        />
      </label>
      <p class="form-hint">{{ t('agents.customLabelsHint') }}</p>
      <label class="field">
        <span>{{ t('agents.maxConcurrency') }}</span>
        <input
          v-model="newConcurrency"
          type="number"
          min="1"
          name="agent-concurrency"
        />
      </label>
      <div class="agent-form-actions">
        <button
          type="button"
          class="btn-primary"
          name="agent-create"
          :disabled="!canCreate"
          @click="createAgent"
        >
          {{ creating ? t('agents.creating') : t('agents.create') }}
        </button>
      </div>
      <p v-if="createError" class="form-error" role="alert">{{ createError }}</p>
    </form>

    <!-- 一次性凭据（建条目响应：token + 注册码明文仅此一次 + 按 OS 复制命令）。 -->
    <div v-if="createdCreds" class="agent-creds" role="alert">
      <p class="agent-creds-title">{{ t('agents.credentialsOneTime') }}</p>
      <dl>
        <dt>{{ t('agents.name') }}</dt>
        <dd class="mono">{{ createdCreds.agentName }}</dd>
        <dt>{{ t('agents.registerCodeLabel') }}</dt>
        <dd class="mono">{{ createdCreds.registerCode }}</dd>
        <dt>{{ t('agents.agentTokenLabel') }}</dt>
        <dd class="mono">{{ createdCreds.token }}</dd>
      </dl>
      <p class="agent-creds-warn">{{ t('agents.credentialsWarn') }}</p>
      <label class="field">
        <span>{{ t('agents.targetOs') }}</span>
        <select v-model="targetOs" name="agent-target-os">
          <option value="linux">Linux / macOS</option>
          <option value="windows">Windows</option>
        </select>
      </label>
      <div class="agent-cmd">
        <code>{{ buildAgentRegisterCommand(targetOs, createdCreds.registerCode) }}</code>
        <button type="button" name="agent-copy" @click="copyCommand">
          {{ t('agents.copy') }}
        </button>
      </div>
      <p class="form-hint">{{ t('agents.cmdNote') }}</p>
      <div class="agent-creds-actions">
        <button
          type="button"
          class="btn-secondary"
          name="agent-creds-dismiss"
          @click="dismissCreds"
        >
          {{ t('agents.credsDismiss') }}
        </button>
      </div>
    </div>

    <!-- 排空/不兼容 退化标注（REST 契约未暴露排空/版本字段）。 -->
    <p v-if="agents && agents.length > 0" class="form-hint">{{ t('agents.statesDegraded') }}</p>

    <!-- Agent 列表（按名排序）。 -->
    <ul v-if="agents" class="agent-list">
      <li v-for="agent in agents" :key="agent.name" class="agent-item">
        <div class="agent-row-head">
          <button type="button" class="agent-name-btn" @click="openDetail(agent)">
            <span class="agent-name">{{ agent.name }}</span>
          </button>
          <span
            class="agent-state-badge"
            :class="agentStateClass(agentBadgeState(agent))"
          >
            {{ t(agentStateLabelKey(agentBadgeState(agent))) }}
          </span>
          <span class="agent-slots">
            {{ t('agents.slotUsage') }}: {{ agent.active_jobs }} / {{ agent.max_concurrency }}
          </span>
          <span v-if="agent.last_seen_at" class="agent-seen">
            {{ t('agents.lastSeen') }}: {{ formatDateTime(agent.last_seen_at) }}
          </span>
          <span v-else class="agent-seen">{{ t('agents.neverSeen') }}</span>
          <div class="agent-row-actions">
            <button
              type="button"
              class="btn-secondary agent-toggle"
              :disabled="togglingName === agent.name"
              @click="toggleDisabled(agent)"
            >
              {{ agent.disabled ? t('agents.enable') : t('agents.disable') }}
            </button>
            <button
              v-if="editingName !== agent.name"
              type="button"
              class="btn-secondary agent-edit"
              @click="startEdit(agent)"
            >
              {{ t('agents.edit') }}
            </button>
          </div>
        </div>

        <!-- 行内编辑表单（槽位 + 自定义标签）。 -->
        <form v-if="editingName === agent.name" class="agent-edit-form" @submit.prevent>
          <label class="field">
            <span>{{ t('agents.maxConcurrency') }}</span>
            <input
              v-model="editConcurrency"
              type="number"
              min="1"
              name="edit-concurrency"
            />
          </label>
          <label class="field">
            <span>{{ t('agents.customLabels') }}</span>
            <textarea
              v-model="editLabels"
              rows="3"
              name="edit-labels"
              :placeholder="t('agents.customLabelsPlaceholder')"
            />
          </label>
          <p class="form-hint">{{ t('agents.customLabelsHint') }}</p>
          <div class="agent-form-actions">
            <button
              type="button"
              class="btn-primary agent-save"
              :disabled="savingEdit"
              @click="saveEdit(agent)"
            >
              {{ savingEdit ? t('agents.saving') : t('agents.save') }}
            </button>
            <button type="button" class="btn-secondary agent-cancel" @click="cancelEdit">
              {{ t('agents.cancel') }}
            </button>
          </div>
          <p v-if="editError" class="form-error" role="alert">{{ editError }}</p>
        </form>
      </li>
    </ul>
    <p v-else-if="!adminOnly && !listError" class="form-hint">{{ t('agents.empty') }}</p>
  </div>
</template>

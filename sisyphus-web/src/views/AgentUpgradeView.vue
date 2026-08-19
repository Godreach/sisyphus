<script setup lang="ts">
// Agent 升级页（ADR-0017，票 B4-T6）：包上传 + 全量/单台升级指令 + 排空状态列。
//
// 管理区全局 admin 面。**升级端点未交付**（ADR-0017：管理员上传 agent 发行包
// 到 Server、升级指令经既有 gRPC 通道下发；REST 面暂无升级端点）→ 按
// 「包上传 + 指令」形态搭好、动作区占位 + 显式退化标注（端点交付后接上，
// B4 纯前端消费既有契约、不补后端）。
//
// - 包上传：文件选择器（形态搭好，上传按钮占位禁用——上传端点未交付）。
// - 全量升级指令：按钮占位禁用（下发端点未交付）。
// - 单台升级指令：Agent 下拉 + 按钮占位禁用（下发端点未交付）。
// - 排空状态列 + 升级阶段列：`GET /agents` 取 Agent 清单渲染两列，但排空标志
//   与升级阶段字段未进 REST 契约（ADR-0017 语义，与 AgentListView 同款退化）
//   → 两列今日标「—」+ 退化标注，后端补字段后接上。

import { onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'

import { agentsApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import type { AgentResponse } from '@/api/types'
import { agentBadgeState, agentStateClass, agentStateLabelKey } from '@/utils/agentState'

const { t } = useI18n()

const agents = ref<AgentResponse[] | null>(null)
const listError = ref('')
/** 403（非全局 admin）→ admin-only 退化态：不渲染动作/表格。 */
const adminOnly = ref(false)

/** 包上传（形态搭好）：所选文件名，仅展示用——上传端点未交付，不上送。 */
const packageName = ref('')
/** 单台升级目标（下拉源 = Agent 清单）。 */
const targetAgent = ref('')

onMounted(load)

/** 加载 Agent 清单（全局 admin 专属，渲染排空/升级两列的行）。 */
async function load(): Promise<void> {
  listError.value = ''
  adminOnly.value = false
  try {
    agents.value = await agentsApi.list()
    if (targetAgent.value === '' && agents.value.length > 0) {
      targetAgent.value = agents.value[0]!.name
    }
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

/** 文件选择：仅记文件名供展示（上传端点未交付，不上送）。 */
function onPackageChange(event: Event): void {
  const input = event.target as HTMLInputElement
  packageName.value = input.files && input.files.length > 0 ? input.files[0]!.name : ''
}
</script>

<template>
  <div class="admin-page upgrade-page">
    <div class="page-header">
      <h1 class="page-title">{{ t('routes.adminUpgrade') }}</h1>
    </div>

    <!-- 退化标注：升级端点未交付，整页动作区占位。 -->
    <p class="upgrade-degraded">{{ t('upgrade.degraded') }}</p>

    <p v-if="listError" class="form-error" role="alert">{{ listError }}</p>

    <!-- 403 退化态：仅全局管理员可见。 -->
    <p v-if="adminOnly" class="form-hint">{{ t('admin.adminOnly') }}</p>

    <template v-else>
      <!-- 包上传（形态搭好，动作占位）。 -->
      <section class="detail-section upgrade-section">
        <h2>{{ t('upgrade.uploadTitle') }}</h2>
        <div class="upgrade-upload">
          <input
            type="file"
            name="upgrade-package"
            @change="onPackageChange"
          />
          <button
            type="button"
            class="btn-primary"
            name="upgrade-upload"
            disabled
            :title="t('upgrade.actionUnavailable')"
          >
            {{ t('upgrade.upload') }}
          </button>
        </div>
        <p v-if="packageName" class="form-hint">{{ t('upgrade.selectedPackage', { name: packageName }) }}</p>
        <p class="form-hint">{{ t('upgrade.uploadHint') }}</p>
      </section>

      <!-- 升级指令（全量 + 单台，动作占位）。 -->
      <section class="detail-section upgrade-section">
        <h2>{{ t('upgrade.commandsTitle') }}</h2>
        <div class="upgrade-command-row">
          <button
            type="button"
            class="btn-primary"
            name="upgrade-all"
            disabled
            :title="t('upgrade.actionUnavailable')"
          >
            {{ t('upgrade.upgradeAll') }}
          </button>
        </div>
        <div class="upgrade-command-row">
          <label class="field upgrade-target-field">
            <span>{{ t('upgrade.targetAgent') }}</span>
            <select v-model="targetAgent" name="upgrade-target" :disabled="!agents || agents.length === 0">
              <option v-for="a in agents ?? []" :key="a.name" :value="a.name">{{ a.name }}</option>
            </select>
          </label>
          <button
            type="button"
            class="btn-primary"
            name="upgrade-one"
            disabled
            :title="t('upgrade.actionUnavailable')"
          >
            {{ t('upgrade.upgradeOne') }}
          </button>
        </div>
        <p class="form-hint">{{ t('upgrade.commandsHint') }}</p>
      </section>

      <!-- 排空状态列 + 升级阶段列（退化：两字段未进 REST 契约）。 -->
      <section class="detail-section upgrade-section">
        <h2>{{ t('upgrade.agentsTitle') }}</h2>
        <table class="upgrade-table">
          <thead>
            <tr>
              <th>{{ t('upgrade.colAgent') }}</th>
              <th>{{ t('upgrade.colState') }}</th>
              <th>{{ t('upgrade.colDrain') }}</th>
              <th>{{ t('upgrade.colStage') }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="a in agents ?? []" :key="a.name">
              <td class="mono">{{ a.name }}</td>
              <td>
                <span
                  class="agent-state-badge"
                  :class="agentStateClass(agentBadgeState(a))"
                >
                  {{ t(agentStateLabelKey(agentBadgeState(a))) }}
                </span>
              </td>
              <td class="upgrade-na">—</td>
              <td class="upgrade-na">—</td>
            </tr>
          </tbody>
        </table>
        <p v-if="agents && agents.length === 0" class="form-hint">{{ t('upgrade.empty') }}</p>
        <p class="form-hint">{{ t('upgrade.columnsDegraded') }}</p>
      </section>
    </template>
  </div>
</template>

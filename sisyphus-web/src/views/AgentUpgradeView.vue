<script setup lang="ts">
// Agent 升级页（ADR-0017，票 B5-T4）：包上传 + 全量/单台升级指令 + 排空/升级阶段列。
//
// 管理区全局 admin 面。升级端点自 B5-T4 起交付（ADR-0017：管理员上传 agent
// 发行包到 Server、升级指令经既有 gRPC 通道下发、Agent 自排空 → 下载校验 →
// 原子换入 → 重启）。
//
// - 包上传：文件选择器 + 上传按钮（raw octet body + X-Sisyphus-Filename 头；
//   后端按 ADR-0010 文件名规范解析版本/目标三元组、窗口校验、记 sha256）。
// - 全量升级：选包 + 按钮 → 向所有版本非目标包的未停用 Agent 下发。
// - 单台升级：选 Agent + 选包 + 按钮 → 强制该 Agent 升级。
// - 排空/升级阶段列：取 Agent 清单的 draining / upgrade_phase 真值渲染。
// - 升级是异步推进（Agent 侧排空/下载/换入），页面提供刷新按钮重读进度。

import { onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'

import { agentsApi, upgradePackagesApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import type {
  AgentResponse,
  UpgradeIssuedSummary,
  UpgradePackageResponse,
} from '@/api/types'
import { agentBadgeState, agentStateClass, agentStateLabelKey } from '@/utils/agentState'
import { formatBytes } from '@/utils/format'

const { t } = useI18n()

const agents = ref<AgentResponse[] | null>(null)
const packages = ref<UpgradePackageResponse[] | null>(null)
const listError = ref('')
/** 403（非全局 admin）→ admin-only 退化态：不渲染动作/表格。 */
const adminOnly = ref(false)

/** 选中的目标升级包（全量与单台共用）。 */
const selectedPackage = ref('')
/** 单台升级目标 Agent。 */
const targetAgent = ref('')

/** 上传中文件 + 反馈。 */
const uploadFile = ref<File | null>(null)
const uploading = ref(false)
const uploadMsg = ref('')
/** 升级指令反馈。 */
const upgradeMsg = ref('')
const upgrading = ref(false)

onMounted(load)

/** 加载 Agent 清单 + 升级包清单（全局 admin 专属）。 */
async function load(): Promise<void> {
  listError.value = ''
  adminOnly.value = false
  try {
    const [agentList, pkgList] = await Promise.all([agentsApi.list(), upgradePackagesApi.list()])
    agents.value = agentList
    packages.value = pkgList
    if (targetAgent.value === '' && agentList.length > 0) {
      targetAgent.value = agentList[0]!.name
    }
    if (selectedPackage.value === '' && pkgList.length > 0) {
      selectedPackage.value = pkgList[0]!.package_name
    }
  } catch (err) {
    if (err instanceof ApiError && err.status === 403) {
      agents.value = null
      packages.value = null
      adminOnly.value = true
      return
    }
    agents.value = null
    packages.value = null
    listError.value = describeSubmitError(err)
  }
}

/** 文件选择：记待上传文件。 */
function onPackageChange(event: Event): void {
  const input = event.target as HTMLInputElement
  uploadFile.value = input.files && input.files.length > 0 ? input.files[0]! : null
  uploadMsg.value = ''
}

/** 上传升级包（raw octet body + X-Sisyphus-Filename 头）。 */
async function uploadPackage(): Promise<void> {
  if (!uploadFile.value) return
  uploading.value = true
  uploadMsg.value = ''
  try {
    await upgradePackagesApi.upload(uploadFile.value)
    uploadMsg.value = t('upgrade.uploaded', { name: uploadFile.value.name })
    uploadFile.value = null
    // 重置 file input（同值再选触发 change）。
    const input = document.querySelector('input[name="upgrade-package"]') as HTMLInputElement | null
    if (input) input.value = ''
    await load()
  } catch (err) {
    uploadMsg.value = describeSubmitError(err)
  } finally {
    uploading.value = false
  }
}

/** 全量升级。 */
async function upgradeAll(): Promise<void> {
  if (!selectedPackage.value) return
  upgrading.value = true
  upgradeMsg.value = ''
  try {
    const summary: UpgradeIssuedSummary = await agentsApi.upgradeAll({
      package_name: selectedPackage.value,
    })
    upgradeMsg.value = t('upgrade.upgradeIssued', {
      package: summary.package_name,
      issued: summary.issued,
      skipped: summary.skipped,
    })
    await load()
  } catch (err) {
    upgradeMsg.value = describeSubmitError(err)
  } finally {
    upgrading.value = false
  }
}

/** 单台升级。 */
async function upgradeOne(): Promise<void> {
  if (!targetAgent.value || !selectedPackage.value) return
  upgrading.value = true
  upgradeMsg.value = ''
  try {
    await agentsApi.upgradeOne(targetAgent.value, { package_name: selectedPackage.value })
    upgradeMsg.value = t('upgrade.upgradeOneDone', { agent: targetAgent.value })
    await load()
  } catch (err) {
    upgradeMsg.value = describeSubmitError(err)
  } finally {
    upgrading.value = false
  }
}

/** 升级阶段 → i18n key（无升级 → —）。 */
function stageKey(phase: string | null | undefined): string {
  switch (phase) {
    case 'draining':
      return 'upgrade.stageDraining'
    case 'downloading':
      return 'upgrade.stageDownloading'
    case 'swapping':
      return 'upgrade.stageSwapping'
    case 'restarting':
      return 'upgrade.stageRestarting'
    case 'fallback':
      return 'upgrade.stageFallback'
    default:
      return ''
  }
}

/** 版本号 → "major.minor.patch"。 */
function versionStr(v: { major: number; minor: number; patch: number } | null | undefined): string {
  return v ? `${v.major}.${v.minor}.${v.patch}` : '—'
}

/** 删除升级包（旧包清理；刷新清单）。 */
async function deletePackage(packageName: string): Promise<void> {
  try {
    await upgradePackagesApi.delete(packageName)
    uploadMsg.value = ''
    await load()
  } catch (err) {
    uploadMsg.value = describeSubmitError(err)
  }
}
</script>

<template>
  <div class="admin-page upgrade-page">
    <div class="page-header">
      <h1 class="page-title">{{ t('routes.adminUpgrade') }}</h1>
      <button type="button" class="btn-secondary" name="upgrade-refresh" @click="load">
        {{ t('upgrade.refresh') }}
      </button>
    </div>

    <p v-if="listError" class="form-error" role="alert">{{ listError }}</p>

    <!-- 403 退化态：仅全局管理员可见。 -->
    <p v-if="adminOnly" class="form-hint">{{ t('admin.adminOnly') }}</p>

    <template v-else>
      <!-- 升级包上传。 -->
      <section class="detail-section upgrade-section">
        <h2>{{ t('upgrade.uploadTitle') }}</h2>
        <div class="upgrade-upload">
          <input type="file" name="upgrade-package" @change="onPackageChange" />
          <button
            type="button"
            class="btn-primary"
            name="upgrade-upload"
            :disabled="!uploadFile || uploading"
            @click="uploadPackage"
          >
            {{ uploading ? t('upgrade.uploading') : t('upgrade.upload') }}
          </button>
        </div>
        <p class="form-hint">{{ t('upgrade.uploadHint') }}</p>
        <p v-if="uploadMsg" class="form-hint">{{ uploadMsg }}</p>
      </section>

      <!-- 升级指令（全量 + 单台）。 -->
      <section class="detail-section upgrade-section">
        <h2>{{ t('upgrade.commandsTitle') }}</h2>
        <div class="upgrade-command-row">
          <label class="field upgrade-target-field">
            <span>{{ t('upgrade.targetPackage') }}</span>
            <select
              v-model="selectedPackage"
              name="upgrade-package-select"
              :disabled="!packages || packages.length === 0"
            >
              <option v-for="p in packages ?? []" :key="p.package_name" :value="p.package_name">
                {{ p.package_name }}
              </option>
            </select>
          </label>
          <button
            type="button"
            class="btn-primary"
            name="upgrade-all"
            :disabled="!selectedPackage || upgrading"
            @click="upgradeAll"
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
            :disabled="!targetAgent || !selectedPackage || upgrading"
            @click="upgradeOne"
          >
            {{ t('upgrade.upgradeOne') }}
          </button>
        </div>
        <p class="form-hint">{{ t('upgrade.commandsHint') }}</p>
        <p v-if="upgradeMsg" class="form-hint">{{ upgradeMsg }}</p>
      </section>

      <!-- 升级包清单。 -->
      <section v-if="packages && packages.length > 0" class="detail-section upgrade-section">
        <h2>{{ t('upgrade.packagesTitle') }}</h2>
        <table class="upgrade-table">
          <thead>
            <tr>
              <th>{{ t('upgrade.colPackage') }}</th>
              <th>{{ t('upgrade.colVersion') }}</th>
              <th>{{ t('upgrade.colTarget') }}</th>
              <th>{{ t('upgrade.colSize') }}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="p in packages" :key="p.package_name">
              <td class="mono">{{ p.package_name }}</td>
              <td>{{ versionStr(p.version) }}</td>
              <td>{{ p.target_os }}/{{ p.target_arch }}</td>
              <td>{{ formatBytes(p.size) }}</td>
              <td>
                <button
                  type="button"
                  class="btn-secondary"
                  :name="`upgrade-package-delete-${p.package_name}`"
                  @click="deletePackage(p.package_name)"
                >
                  {{ t('upgrade.deletePackage') }}
                </button>
              </td>
            </tr>
          </tbody>
        </table>
      </section>

      <!-- Agent 排空与升级状态。 -->
      <section class="detail-section upgrade-section">
        <h2>{{ t('upgrade.agentsTitle') }}</h2>
        <table class="upgrade-table">
          <thead>
            <tr>
              <th>{{ t('upgrade.colAgent') }}</th>
              <th>{{ t('upgrade.colState') }}</th>
              <th>{{ t('upgrade.colVersion') }}</th>
              <th>{{ t('upgrade.colDrain') }}</th>
              <th>{{ t('upgrade.colStage') }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="a in agents ?? []" :key="a.name">
              <td class="mono">{{ a.name }}</td>
              <td>
                <span class="agent-state-badge" :class="agentStateClass(agentBadgeState(a))">
                  {{ t(agentStateLabelKey(agentBadgeState(a))) }}
                </span>
              </td>
              <td>{{ versionStr(a.agent_version) }}</td>
              <td>{{ a.draining ? t('upgrade.drainYes') : '—' }}</td>
              <td>
                <span v-if="stageKey(a.upgrade_phase)">{{ t(stageKey(a.upgrade_phase)) }}</span>
                <span v-else class="upgrade-na">—</span>
                <span v-if="a.upgrade_error" class="form-hint">{{ a.upgrade_error }}</span>
              </td>
            </tr>
          </tbody>
        </table>
        <p v-if="agents && agents.length === 0" class="form-hint">{{ t('upgrade.empty') }}</p>
        <p v-if="packages && packages.length === 0" class="form-hint">{{ t('upgrade.noPackages') }}</p>
      </section>
    </template>
  </div>
</template>

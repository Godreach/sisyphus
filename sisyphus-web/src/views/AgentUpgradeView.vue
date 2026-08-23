<script setup lang="ts">
// Agent 升级页（ADR-0017，票 B5-T4）：包上传 + 全量/单台升级指令 + 排空/升级阶段列。
//
// 管理区全局 admin 面。升级端点自 B5-T4 起交付（ADR-0017：管理员上传 agent
// 发行包到 Server、升级指令经既有 gRPC 通道下发、Agent 自排空 → 下载校验 →
// 原子换入 → 重启）。
//
// - 包上传：NUpload 文件选择即传（raw octet body + X-Sisyphus-Filename 头；
//   后端按 ADR-0010 文件名规范解析版本/目标三元组、窗口校验、记 sha256）。
// - 全量升级：选包 + 按钮 → 向所有版本非目标包的未停用 Agent 下发。
// - 单台升级：选 Agent + 选包 + 按钮 → 强制该 Agent 升级。
// - 排空/升级阶段列：取 Agent 清单的 draining / upgrade_phase 真值渲染；
//   升级阶段经 NProgress 进度条可视化（排空 25% → 下载 50% → 换入 75% →
//   重启 90%；fallback 100% 红条——阶段是离散推进，条长示意进程而非精确值）。
// - 升级是异步推进（Agent 侧排空/下载/换入），页面提供刷新按钮重读进度。
// #95: 使用 Naive UI 组件重写——各分区改 NCard、包上传改 NUpload、两张清单
// 改 NDataTable（Agent 状态列 NTag、升级阶段列 NProgress）、删除包经
// NPopconfirm 确认、成功操作 NMessage toast、错误态 NAlert。

import { computed, h, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  NAlert,
  NButton,
  NCard,
  NDataTable,
  NEmpty,
  NPopconfirm,
  NProgress,
  NSelect,
  NSkeleton,
  NTag,
  NUpload,
  useMessage,
  type DataTableColumns,
  type UploadCustomRequestOptions,
} from 'naive-ui'

import { agentsApi, upgradePackagesApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import type {
  AgentResponse,
  UpgradeIssuedSummary,
  UpgradePackageResponse,
} from '@/api/types'
import { agentBadgeState, agentStateLabelKey, agentStateTagType } from '@/utils/agentState'
import { formatBytes } from '@/utils/format'

const { t } = useI18n()
const message = useMessage()

const agents = ref<AgentResponse[] | null>(null)
const packages = ref<UpgradePackageResponse[] | null>(null)
const loading = ref(true)
const listError = ref('')
/** 403（非全局 admin）→ admin-only 退化态：不渲染动作/表格。 */
const adminOnly = ref(false)

/** 选中的目标升级包（全量与单台共用）。 */
const selectedPackage = ref('')
/** 单台升级目标 Agent。 */
const targetAgent = ref('')

/** 上传中 + 上传反馈（错误就地 NAlert；成功走 NMessage）。 */
const uploading = ref(false)
const uploadError = ref('')
/** 升级指令反馈（错误就地 NAlert；成功走 NMessage）。 */
const commandError = ref('')
const upgrading = ref(false)

/** 升级阶段 → 单点登记：i18n key + NProgress 百分比（离散阶段 → 示意性进程；
 *  fallback 红条 100%）。stageKey/percent 同表，防两处分发漂移。 */
const STAGES: Record<string, { key: string; percent: number }> = {
  draining: { key: 'upgrade.stageDraining', percent: 25 },
  downloading: { key: 'upgrade.stageDownloading', percent: 50 },
  swapping: { key: 'upgrade.stageSwapping', percent: 75 },
  restarting: { key: 'upgrade.stageRestarting', percent: 90 },
  fallback: { key: 'upgrade.stageFallback', percent: 100 },
}

onMounted(load)

const packageOptions = computed(() =>
  (packages.value ?? []).map((p) => ({ label: p.package_name, value: p.package_name })),
)

const agentOptions = computed(() =>
  (agents.value ?? []).map((a) => ({ label: a.name, value: a.name })),
)

/** 已上传升级包表列（删除经 NPopconfirm——旧包清理，删元数据 + 字节）。 */
const packageColumns = computed<DataTableColumns<UpgradePackageResponse>>(() => [
  {
    title: t('upgrade.colPackage'),
    key: 'package_name',
    render: (row) => h('span', { class: 'mono' }, row.package_name),
  },
  {
    title: t('upgrade.colVersion'),
    key: 'version',
    render: (row) => versionStr(row.version),
  },
  {
    title: t('upgrade.colTarget'),
    key: 'target',
    render: (row) => `${row.target_os}/${row.target_arch}`,
  },
  {
    title: t('upgrade.colSize'),
    key: 'size',
    render: (row) => formatBytes(row.size),
  },
  {
    title: '',
    key: 'actions',
    width: 100,
    render: (row) =>
      h(
        NPopconfirm,
        {
          positiveText: t('common.confirm'),
          negativeText: t('common.cancel'),
          onPositiveClick: () => void deletePackage(row.package_name),
        },
        {
          trigger: () =>
            h(
              NButton,
              { size: 'small', name: `upgrade-package-delete-${row.package_name}` },
              { default: () => t('upgrade.deletePackage') },
            ),
          default: () => t('upgrade.packageDeleteConfirm', { name: row.package_name }),
        },
      ),
  },
])

const packageRowKey = (row: UpgradePackageResponse): string => row.package_name

/** Agent 排空与升级状态表列（状态列 NTag 色标，与 Agent 列表页同源；
 * 升级阶段列 NProgress 进度条 + 阶段文案）。 */
const agentColumns = computed<DataTableColumns<AgentResponse>>(() => [
  {
    title: t('upgrade.colAgent'),
    key: 'name',
    render: (row) => h('span', { class: 'mono' }, row.name),
  },
  {
    title: t('upgrade.colState'),
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
    title: t('upgrade.colVersion'),
    key: 'version',
    render: (row) => versionStr(row.agent_version),
  },
  {
    title: t('upgrade.colDrain'),
    key: 'draining',
    width: 80,
    render: (row) => (row.draining ? t('upgrade.drainYes') : '—'),
  },
  {
    title: t('upgrade.colStage'),
    key: 'upgrade_phase',
    render: (row) => renderStage(row),
  },
])

const agentRowKey = (row: AgentResponse): string => row.name

/** 升级阶段列：NProgress 进度条 + 阶段文案（无升级 → —；fallback → 红条
 *  100% + 退回文案；upgrade_error 就地灰字提示）。 */
function renderStage(row: AgentResponse) {
  const stage = STAGES[row.upgrade_phase ?? '']
  if (!stage) return h('span', { class: 'upgrade-stage-na' }, '—')
  const children = [
    h(NProgress, {
      type: 'line',
      percentage: stage.percent,
      status: row.upgrade_phase === 'fallback' ? 'error' : 'default',
      showIndicator: false,
      height: 6,
      class: 'upgrade-stage-bar',
    }),
    h('span', { class: 'upgrade-stage-label' }, t(stage.key)),
  ]
  if (row.upgrade_error) {
    children.push(h('span', { class: 'form-hint upgrade-stage-error' }, row.upgrade_error))
  }
  return h('div', { class: 'upgrade-stage-cell' }, children)
}

/** 加载 Agent 清单 + 升级包清单（全局 admin 专属）。每次加载（含刷新/上传后
 *  重载）都回置 loading——Agent 卡骨架屏替换旧表，与构建列表页同纪律。 */
async function load(): Promise<void> {
  loading.value = true
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
  } finally {
    loading.value = false
  }
}

/** NUpload 自定义上传：raw octet body + X-Sisyphus-Filename 头（api client
 *  内拼），成功 → NMessage + 刷新清单；失败 → 就地 NAlert。 */
async function uploadRequest({ file, onFinish, onError }: UploadCustomRequestOptions): Promise<void> {
  const raw = file.file
  if (!(raw instanceof File)) {
    onError()
    return
  }
  uploading.value = true
  uploadError.value = ''
  try {
    await upgradePackagesApi.upload(raw)
    message.success(t('upgrade.uploaded', { name: raw.name }))
    onFinish()
    await load()
  } catch (err) {
    uploadError.value = describeSubmitError(err)
    onError()
  } finally {
    uploading.value = false
  }
}

/** 全量升级。 */
async function upgradeAll(): Promise<void> {
  if (!selectedPackage.value) return
  upgrading.value = true
  commandError.value = ''
  try {
    const summary: UpgradeIssuedSummary = await agentsApi.upgradeAll({
      package_name: selectedPackage.value,
    })
    message.success(
      t('upgrade.upgradeIssued', {
        package: summary.package_name,
        issued: summary.issued,
        skipped: summary.skipped,
      }),
    )
    await load()
  } catch (err) {
    commandError.value = describeSubmitError(err)
  } finally {
    upgrading.value = false
  }
}

/** 单台升级。 */
async function upgradeOne(): Promise<void> {
  if (!targetAgent.value || !selectedPackage.value) return
  upgrading.value = true
  commandError.value = ''
  try {
    await agentsApi.upgradeOne(targetAgent.value, { package_name: selectedPackage.value })
    message.success(t('upgrade.upgradeOneDone', { agent: targetAgent.value }))
    await load()
  } catch (err) {
    commandError.value = describeSubmitError(err)
  } finally {
    upgrading.value = false
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
    uploadError.value = ''
    message.success(t('upgrade.packageDeleted'))
    await load()
  } catch (err) {
    uploadError.value = describeSubmitError(err)
  }
}
</script>

<template>
  <div class="admin-page upgrade-page">
    <div class="page-header">
      <h1 class="page-title">{{ t('routes.adminUpgrade') }}</h1>
      <n-button name="upgrade-refresh" @click="load">
        {{ t('upgrade.refresh') }}
      </n-button>
    </div>

    <n-alert v-if="listError" type="error" :title="listError" role="alert" />

    <!-- 403 退化态：仅全局管理员可见。 -->
    <p v-if="adminOnly" class="form-hint">{{ t('admin.adminOnly') }}</p>

    <template v-else>
      <!-- 升级包上传（文件选择即传：NUpload custom-request 直发 API）。 -->
      <n-card :title="t('upgrade.uploadTitle')" size="small" class="upgrade-card">
        <n-upload
          :show-file-list="false"
          :custom-request="uploadRequest"
          accept=".tar.gz,.zip,.tar"
          class="upgrade-uploader"
          :disabled="uploading"
        >
          <n-button type="primary" name="upgrade-upload" :loading="uploading">
            {{ uploading ? t('upgrade.uploading') : t('upgrade.upload') }}
          </n-button>
        </n-upload>
        <p class="form-hint">{{ t('upgrade.uploadHint') }}</p>
        <n-alert v-if="uploadError" type="error" :title="uploadError" role="alert" class="upgrade-card-alert" />
      </n-card>

      <!-- 升级指令（全量 + 单台）。 -->
      <n-card :title="t('upgrade.commandsTitle')" size="small" class="upgrade-card">
        <div class="upgrade-command-row">
          <span class="upgrade-command-label">{{ t('upgrade.targetPackage') }}</span>
          <n-select
            v-model:value="selectedPackage"
            :options="packageOptions"
            class="upgrade-command-select"
            :disabled="!packages || packages.length === 0"
            :virtual-scroll="false"
          />
          <n-button
            type="primary"
            name="upgrade-all"
            :disabled="!selectedPackage"
            :loading="upgrading"
            @click="upgradeAll"
          >
            {{ t('upgrade.upgradeAll') }}
          </n-button>
        </div>
        <div class="upgrade-command-row">
          <span class="upgrade-command-label">{{ t('upgrade.targetAgent') }}</span>
          <n-select
            v-model:value="targetAgent"
            :options="agentOptions"
            class="upgrade-command-select"
            :disabled="!agents || agents.length === 0"
            :virtual-scroll="false"
          />
          <n-button
            type="primary"
            name="upgrade-one"
            :disabled="!targetAgent || !selectedPackage"
            :loading="upgrading"
            @click="upgradeOne"
          >
            {{ t('upgrade.upgradeOne') }}
          </n-button>
        </div>
        <p class="form-hint">{{ t('upgrade.commandsHint') }}</p>
        <n-alert v-if="commandError" type="error" :title="commandError" role="alert" class="upgrade-card-alert" />
      </n-card>

      <!-- 已上传升级包清单（无包 → 空态文案）。 -->
      <n-card :title="t('upgrade.packagesTitle')" size="small" class="upgrade-card">
        <n-data-table
          v-if="packages && packages.length > 0"
          :columns="packageColumns"
          :data="packages"
          :row-key="packageRowKey"
          :bordered="false"
          :single-line="true"
          size="small"
          class="upgrade-packages-table"
        />
        <!-- 首载中不闪空态文案（loading 结束后才回落「暂无包」）。 -->
        <p v-else-if="!loading" class="form-hint">{{ t('upgrade.noPackages') }}</p>
      </n-card>

      <!-- Agent 排空与升级状态（阶段列 NProgress 进度条）。 -->
      <n-card :title="t('upgrade.agentsTitle')" size="small" class="upgrade-card">
        <div v-if="loading && !listError" class="upgrade-skeleton">
          <n-skeleton v-for="i in 3" :key="i" text :repeat="1" height="28px" class="upgrade-skeleton-row" />
        </div>
        <div v-else-if="agents && agents.length === 0" class="upgrade-empty">
          <n-empty :description="t('upgrade.empty')" />
        </div>
        <n-data-table
          v-else-if="agents"
          :columns="agentColumns"
          :data="agents"
          :row-key="agentRowKey"
          :bordered="false"
          :single-line="true"
          size="small"
          class="upgrade-agents-table"
        />
      </n-card>
    </template>
  </div>
</template>

<style scoped>
.upgrade-card {
  margin-top: 4px;
}

.upgrade-card-alert {
  margin-top: 8px;
}

.upgrade-uploader {
  width: fit-content;
}

.upgrade-command-row {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-bottom: 10px;
}

.upgrade-command-label {
  font-size: 14px;
  white-space: nowrap;
  color: var(--n-text-color-3, #7f8792);
}

.upgrade-command-select {
  width: 320px;
}

.upgrade-skeleton {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.upgrade-skeleton-row {
  width: 100%;
}

.upgrade-empty {
  padding: 16px 0;
}

/* 升级阶段列：NProgress 进度条 + 阶段文案竖排。 */
.upgrade-stage-cell {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 140px;
}

.upgrade-stage-bar {
  width: 120px;
}

.upgrade-stage-label {
  font-size: 12px;
}

.upgrade-stage-na {
  color: var(--n-text-color-3, #7f8792);
}

.mono {
  font-family: ui-monospace, 'Cascadia Code', Consolas, monospace;
  font-size: 12px;
  word-break: break-all;
}
</style>

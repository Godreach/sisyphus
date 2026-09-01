<script setup lang="ts">
// Agent 升级页（ADR-0017，票 B5-T4；spec #111 定稿设计语言铺开）：包上传 +
// 全量/单台升级指令 + 排空/升级阶段列。设计语言与三主页面/机密审计页同源
// ——页头 + sisy-card 卡片区（行式清单 + 胶囊徽章 + 双层进度条 + 描边小按钮，
// 票 #106–#110 同形态）。
//
// 管理区全局 admin 面。升级端点自 B5-T4 起交付（ADR-0017：管理员上传 agent
// 发行包到 Server、升级指令经既有 gRPC 通道下发、Agent 自排空 → 下载校验 →
// 原子换入 → 重启）。
//
// - 包上传：NUpload 文件选择即传（raw octet body + X-Sisyphus-Filename 头；
//   后端按 ADR-0010 文件名规范解析版本/目标三元组、窗口校验、记 sha256）。
// - 全量升级：选包 + 按钮 → 向所有版本非目标包的未停用 Agent 下发。
// - 单台升级：选 Agent + 选包 + 按钮 → 强制该 Agent 升级（构建机列表页
//   排空/不兼容机器经 `?agent=` 深链预选，票 #106 M7）。
// - 排空/升级阶段列：取 Agent 清单的 draining / upgrade_phase 真值渲染；
//   升级阶段经双层进度条可视化（排空 25% → 下载 50% → 换入 75% → 重启 90%；
//   fallback 100% 红条——阶段是离散推进，条长示意进程而非精确值）。
// - 升级是异步推进（Agent 侧排空/下载/换入），页面提供刷新按钮重读进度。
//
// 事实态纪律：首载骨架屏、清单失败整页报错 + 重试、上传/指令失败卡内就地
// 报错、空态文案首载不闪。403（非全局 admin）→ admin-only 退化态。

import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { useRoute } from 'vue-router'
import { NAlert, NEmpty, NPopconfirm, NSelect, NSkeleton, NUpload, useMessage, type UploadCustomRequestOptions } from 'naive-ui'

import { agentsApi, upgradePackagesApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import type {
  AgentResponse,
  UpgradeIssuedSummary,
  UpgradePackageResponse,
} from '@/api/types'
import { agentBadgeState, agentStateBadgeClass, agentStateLabelKey } from '@/utils/agentState'
import { formatBytes } from '@/utils/format'

const { t } = useI18n()
const message = useMessage()
const route = useRoute()

const agents = ref<AgentResponse[] | null>(null)
const packages = ref<UpgradePackageResponse[] | null>(null)
const loading = ref(true)
const listError = ref('')
/** 403（非全局 admin）→ admin-only 退化态：不渲染动作/表格。 */
const adminOnly = ref(false)

/** 选中的目标升级包（全量与单台共用）。 */
const selectedPackage = ref('')
/** 单台升级目标 Agent（构建机列表页 M7 深链 ?agent= 预选）。 */
const targetAgent = ref('')

/** 上传中 + 上传反馈（错误就地 NAlert；成功走 NMessage）。 */
const uploading = ref(false)
const uploadError = ref('')
/** 升级指令反馈（错误就地 NAlert；成功走 NMessage）。 */
const commandError = ref('')
const upgrading = ref(false)

/** 升级阶段 → 单点登记：i18n key + 进度条百分比（离散阶段 → 示意性进程；
 *  fallback 红条 100%）。stageKey/percent 同表，防两处分发漂移。 */
const STAGES: Record<string, { key: string; percent: number }> = {
  draining: { key: 'upgrade.stageDraining', percent: 25 },
  downloading: { key: 'upgrade.stageDownloading', percent: 50 },
  swapping: { key: 'upgrade.stageSwapping', percent: 75 },
  restarting: { key: 'upgrade.stageRestarting', percent: 90 },
  fallback: { key: 'upgrade.stageFallback', percent: 100 },
}

onMounted(load)

// M7 深链预选：`?agent=` 变化时（同组件复用——浏览器前进/后退在两个机器的
// 深链间切换）重选单台目标并重载，与构建机页 `?create=1` watch 同纪律。
watch(
  () => route.query.agent,
  (v) => {
    const name = typeof v === 'string' ? v : ''
    if (name !== '' && (agents.value ?? []).some((a) => a.name === name)) {
      targetAgent.value = name
    }
  },
)

const packageOptions = computed(() =>
  (packages.value ?? []).map((p) => ({ label: p.package_name, value: p.package_name })),
)

const agentOptions = computed(() =>
  (agents.value ?? []).map((a) => ({ label: a.name, value: a.name })),
)

/** 包清单卡副标（计数；与机密页 card-subtitle 同形态）。 */
const packagesCountText = computed(() =>
  packages.value != null ? t('upgrade.packagesCount', { n: packages.value.length }) : '',
)

/** Agent 卡副标（计数）。 */
const agentsCountText = computed(() =>
  agents.value != null ? t('upgrade.agentsCount', { n: agents.value.length }) : '',
)

/** Agent 名 → 升级阶段登记（行渲染单点；无升级的机器不在 map）。 */
const stageByAgent = computed(() => {
  const map = new Map<string, { key: string; percent: number; fallback: boolean }>()
  for (const agent of agents.value ?? []) {
    const stage = STAGES[agent.upgrade_phase ?? '']
    if (stage != null) {
      map.set(agent.name, { ...stage, fallback: agent.upgrade_phase === 'fallback' })
    }
  }
  return map
})

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
    if (selectedPackage.value === '' && pkgList.length > 0) {
      selectedPackage.value = pkgList[0]!.package_name
    }
    // M7 深链预选：构建机列表页「去升级」带 ?agent=<name> 直达单台目标。
    const deepAgent = typeof route.query.agent === 'string' ? route.query.agent : ''
    if (deepAgent !== '' && agentList.some((a) => a.name === deepAgent)) {
      targetAgent.value = deepAgent
    } else if (targetAgent.value === '' && agentList.length > 0) {
      targetAgent.value = agentList[0]!.name
    }
  } catch (err) {    if (err instanceof ApiError && err.status === 403) {
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

/** 删除升级包（旧包清理；失败就地 NAlert，刷新清单）。 */
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
    <div class="page-header header-end">
      <button type="button" class="btn-outline" name="upgrade-refresh" @click="load">
        {{ t('upgrade.refresh') }}
      </button>
    </div>

    <!-- 双清单任一失败：整页报错 + 重试（事实态纪律）。 -->
    <n-alert v-if="listError" type="error" :title="listError" role="alert">
      <button type="button" class="btn-outline upgrade-retry" name="upgrade-retry" @click="load">
        {{ t('upgrade.retry') }}
      </button>
    </n-alert>

    <!-- 403 退化态：仅全局管理员可见。 -->
    <p v-else-if="adminOnly" class="form-hint">{{ t('admin.adminOnly') }}</p>

    <template v-else>
      <!-- 升级包上传 + 升级指令（同一张卡：上传即入库，指令即刻可选新包）。 -->
      <section class="sisy-card upgrade-command-card" aria-label="upgrade commands">
        <div class="card-header">
          <div>
            <h2 class="card-title">{{ t('upgrade.uploadTitle') }}</h2>
            <div class="card-subtitle">{{ t('upgrade.commandsTitle') }}</div>
          </div>
        </div>

        <div class="upgrade-command-body">
          <!-- 文件选择即传（NUpload custom-request 直发 API；一次多包 = 连续
               多次上传）。 -->
          <n-upload
            :show-file-list="false"
            :custom-request="uploadRequest"
            accept=".tar.gz,.zip,.tar"
            class="upgrade-uploader"
            :disabled="uploading"
          >
            <button type="button" class="btn-outline blue" name="upgrade-upload" :disabled="uploading">
              {{ uploading ? t('upgrade.uploading') : t('upgrade.upload') }}
            </button>
          </n-upload>
          <p class="form-hint">{{ t('upgrade.uploadHint') }}</p>

          <div class="upgrade-command-row">
            <span class="upgrade-command-label">{{ t('upgrade.targetPackage') }}</span>
            <n-select
              v-model:value="selectedPackage"
              :options="packageOptions"
              class="upgrade-command-select"
              :disabled="!packages || packages.length === 0"
              :virtual-scroll="false"
            />
            <button
              type="button"
              class="btn-outline blue"
              name="upgrade-all"
              :disabled="!selectedPackage || upgrading"
              @click="upgradeAll"
            >
              {{ t('upgrade.upgradeAll') }}
            </button>
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
            <button
              type="button"
              class="btn-outline blue"
              name="upgrade-one"
              :disabled="!targetAgent || !selectedPackage || upgrading"
              @click="upgradeOne"
            >
              {{ t('upgrade.upgradeOne') }}
            </button>
          </div>
          <p class="form-hint">{{ t('upgrade.commandsHint') }}</p>

          <!-- 上传/指令各自就地报错（不互相掩盖——上传反馈与指令反馈是两条
               独立链路）。 -->
          <n-alert v-if="uploadError" type="error" :title="uploadError" role="alert" class="upgrade-card-alert" />
          <n-alert v-if="commandError" type="error" :title="commandError" role="alert" class="upgrade-card-alert" />
        </div>
      </section>

      <!-- 已上传升级包清单（行式清单 + mono 包名 + 删除经原生气泡确认）。 -->
      <section class="sisy-card upgrade-table-card" aria-label="upgrade packages">
        <div class="card-header">
          <div>
            <h2 class="card-title">{{ t('upgrade.packagesTitle') }}</h2>
            <div v-if="packagesCountText" class="card-subtitle">{{ packagesCountText }}</div>
          </div>
        </div>

        <div v-if="loading" class="card-skeleton">
          <n-skeleton text :repeat="2" height="40px" />
        </div>

        <template v-else-if="packages && packages.length > 0">
          <div class="upgrade-thead upgrade-pkg-thead">
            <span>{{ t('upgrade.colPackage') }}</span>
            <span>{{ t('upgrade.colVersion') }}</span>
            <span>{{ t('upgrade.colTarget') }}</span>
            <span>{{ t('upgrade.colSize') }}</span>
            <span class="upgrade-thead-actions" />
          </div>
          <div
            v-for="row in packages"
            :key="row.package_name"
            class="upgrade-row upgrade-pkg-row"
            :data-testid="`upgrade-package-${row.package_name}`"
          >
            <span class="mono upgrade-pkg-name">{{ row.package_name }}</span>
            <span class="upgrade-cell">{{ versionStr(row.version) }}</span>
            <span class="upgrade-cell">{{ row.target_os }}/{{ row.target_arch }}</span>
            <span class="upgrade-cell">{{ formatBytes(row.size) }}</span>
            <div class="upgrade-row-actions">
              <n-popconfirm
                :positive-text="t('common.confirm')"
                :negative-text="t('common.cancel')"
                @positive-click="deletePackage(row.package_name)"
              >
                <template #trigger>
                  <button
                    type="button"
                    class="btn-outline red"
                    name="upgrade-package-delete"
                    :data-testid="`upgrade-package-delete-${row.package_name}`"
                  >
                    {{ t('upgrade.deletePackage') }}
                  </button>
                </template>
                {{ t('upgrade.packageDeleteConfirm', { name: row.package_name }) }}
              </n-popconfirm>
            </div>
          </div>
        </template>

        <!-- 首载中不闪空态文案（loading 结束后才回落「暂无包」）。 -->
        <p v-else class="form-hint upgrade-empty-hint">{{ t('upgrade.noPackages') }}</p>
      </section>

      <!-- Agent 排空与升级状态（胶囊徽章 + 双层进度条阶段列）。 -->
      <section class="sisy-card upgrade-table-card" aria-label="agent upgrade status">
        <div class="card-header">
          <div>
            <h2 class="card-title">{{ t('upgrade.agentsTitle') }}</h2>
            <div v-if="agentsCountText" class="card-subtitle">{{ agentsCountText }}</div>
          </div>
        </div>

        <div v-if="loading" class="card-skeleton">
          <n-skeleton text :repeat="3" height="44px" />
        </div>

        <div v-else-if="agents && agents.length === 0" class="upgrade-empty">
          <n-empty :description="t('upgrade.empty')" />
        </div>

        <template v-else-if="agents">
          <div class="upgrade-thead upgrade-agent-thead">
            <span>{{ t('upgrade.colAgent') }}</span>
            <span>{{ t('upgrade.colState') }}</span>
            <span>{{ t('upgrade.colVersion') }}</span>
            <span>{{ t('upgrade.colDrain') }}</span>
            <span>{{ t('upgrade.colStage') }}</span>
          </div>
          <div
            v-for="row in agents"
            :key="row.name"
            class="upgrade-row upgrade-agent-row"
            :data-testid="`upgrade-agent-${row.name}`"
          >
            <span class="mono upgrade-agent-name">{{ row.name }}</span>
            <span class="badge" :class="agentStateBadgeClass(agentBadgeState(row))">
              {{ t(agentStateLabelKey(agentBadgeState(row))) }}
            </span>
            <span class="upgrade-cell">{{ versionStr(row.agent_version) }}</span>
            <span class="upgrade-cell upgrade-cell-gray">{{ row.draining ? t('upgrade.drainYes') : '—' }}</span>
            <!-- 升级阶段：双层进度条示意进程（离散阶段 → 示意性百分比；
                 fallback 红条 + 退回文案；无升级 → —）。 -->
            <div v-if="stageByAgent.get(row.name)" class="usage-cell upgrade-stage-cell">
              <div class="usage-row">
                <span class="track">
                  <span
                    class="fill"
                    :class="{ red: stageByAgent.get(row.name)!.fallback }"
                    :style="{ width: `${stageByAgent.get(row.name)!.percent}%` }"
                  />
                </span>
                <span class="pct" :class="{ red: stageByAgent.get(row.name)!.fallback }">
                  {{ t(stageByAgent.get(row.name)!.key) }}
                </span>
              </div>
              <p v-if="row.upgrade_error" class="form-hint">{{ row.upgrade_error }}</p>
            </div>
            <span v-else class="upgrade-cell upgrade-cell-gray">—</span>
          </div>
        </template>
      </section>
    </template>
  </div>
</template>

<style scoped>
.upgrade-page {
  gap: 16px;
}

.card-subtitle {
  font-size: 12px;
  color: var(--sisy-color-text-secondary);
  margin-top: 2px;
}

.card-skeleton {
  padding: 0 20px 16px;
}

/* 上传 + 指令卡体。 */
.upgrade-command-body {
  padding: 0 20px 16px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.upgrade-uploader {
  width: fit-content;
}

.upgrade-command-row {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}

.upgrade-command-label {
  font-size: 13px;
  white-space: nowrap;
  color: var(--sisy-color-text-secondary);
}

.upgrade-command-select {
  width: 320px;
  max-width: 100%;
}

.upgrade-card-alert {
  margin-top: 4px;
}

/* 行式清单（表头 + 分隔行；与机密页 secrets-thead/row 同形态）。 */
.upgrade-thead {
  display: grid;
  align-items: center;
  padding: 0 20px;
  height: 40px;
  border-top: 1px solid var(--sisy-color-border);
  border-bottom: 1px solid var(--sisy-color-border);
  font-size: 12px;
  font-weight: 500;
  color: var(--sisy-color-text-secondary);
}

.upgrade-pkg-thead {
  grid-template-columns: minmax(200px, 1.6fr) 80px 120px 90px 90px;
}

.upgrade-agent-thead {
  grid-template-columns: minmax(110px, 1fr) 110px 80px 60px minmax(150px, 1.4fr);
}

.upgrade-row {
  display: grid;
  align-items: center;
  gap: 12px;
  padding: 0 20px;
  min-height: 48px;
  border-bottom: 1px solid var(--sisy-color-border-light);
  transition: background 0.15s;
}

.upgrade-pkg-row {
  grid-template-columns: minmax(200px, 1.6fr) 80px 120px 90px 90px;
}

.upgrade-agent-row {
  grid-template-columns: minmax(110px, 1fr) 110px 80px 60px minmax(150px, 1.4fr);
}

.upgrade-row:last-of-type {
  border-bottom: none;
}

.upgrade-row:hover {
  background: var(--sisy-color-bg);
}

.upgrade-cell {
  font-size: 13px;
}

.upgrade-cell-gray {
  color: var(--sisy-color-text-secondary);
}

.upgrade-thead-actions {
  justify-self: end;
}

.upgrade-row-actions {
  display: flex;
  justify-content: flex-end;
}

.upgrade-pkg-name {
  word-break: break-all;
}

/* 升级阶段列（双层进度条：示意性进程 + 阶段文案）。 */
.upgrade-stage-cell {
  justify-content: center;
}

.mono {
  font-family: ui-monospace, 'Cascadia Code', Consolas, monospace;
  font-size: 12px;
  word-break: break-all;
}

/* 空态。 */
.upgrade-empty {
  padding: 24px 0 32px;
}

.upgrade-empty-hint {
  padding: 12px 20px 16px;
  margin: 0;
}

.upgrade-retry {
  margin-top: 8px;
}
</style>

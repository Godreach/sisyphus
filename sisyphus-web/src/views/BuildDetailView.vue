<script setup lang="ts">
// 构建详情页（票 #107，spec #100 定稿铺开；ADR-0006/0008/0013/0020）。
//
// 视觉：以定稿后的三主页面为推导源，复用共享组件类——badge 胶囊状态徽章、
// sisy-card 卡片、btn-outline 描边动作按钮、usage-row 双层进度条、breadcrumb
// 面包屑；Naive UI 仅保留定稿页同集（NAlert/NSkeleton/NEmpty/NModal/NForm/
// NPopconfirm/useMessage toast）。
//
// - 面包屑：项目 > pipeline > 构建号（ADR-0020 原型唯一 IA 修正）。
// - 阶段/任务卡：按构建快照阶段序（REST 详情 `stages`）；排队任务显示缺失
//   标签等待态（ADR-0008「等待匹配 agent：缺标签 X」——REST 详情不含
//   waiting_detail，从 pipeline 定义的 labels 声明派生，定义缺失时显式退化
//   标注）；任务状态含 attempt 历史（重跑后同任务多行并列）。
// - SSE 日志流：BuildLogView（步骤生命周期与输出块交织、折叠、截断、重连）。
// - 产物下载（票 #74 解禁）：任务声明 × 已上传产物比对——已上传接同源下载
//   链接（cookie 会话自动携带、大小/校验和提示）；未上传展示占位。
// - 动作闭环：触发（NModal 表单：参数覆盖 + 分支/commit）、取消/重跑
//   （描边按钮 + toast 反馈，202 受理 / 409 拒绝）、删除（NPopconfirm 确认）。
// - 事实态纪律：首载骨架屏、加载失败整页报错 + 重试、403 退化态、404
//   「构建不存在」；进行中构建轮询刷新（store）。

import { computed, h, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'
import {
  NAlert,
  NButton,
  NEmpty,
  NForm,
  NFormItem,
  NInput,
  NModal,
  NPopconfirm,
  NSelect,
  NSkeleton,
  useMessage,
} from 'naive-ui'

import { artifactsApi } from '@/api/client'
import { describeActionError } from '@/api/errors'
import { ApiError } from '@/api/http'
import { useBuildDetailStore } from '@/stores/buildDetail'
import {
  formatBytes,
  formatDateTime,
  formatDuration,
  isLiveStatus,
  settledPercent,
  statusBadgeClass,
} from '@/utils/format'
import BuildLogView from '@/components/BuildLogView.vue'
import type {
  JobViewDto,
  RerunBuildRequest,
  StageViewDto,
  TriggerBuildRequest,
} from '@/api/types'

const route = useRoute()
const router = useRouter()
const { t } = useI18n()
const message = useMessage()
const store = useBuildDetailStore()

const project = computed(() => String(route.params.name ?? ''))
const pipeline = computed(() => String(route.params.pipeline ?? ''))
const buildNumber = computed(() => Number(route.params.number))

const build = computed(() => store.build)
const definition = computed(() => store.definition)

// ---------------------------------------------------------------------------
// 动作反馈 / 提交态
// ---------------------------------------------------------------------------

const busy = ref<'trigger' | 'cancel' | 'rerun' | 'delete' | null>(null)
const triggerOpen = ref(false)
const triggerParams = ref<Record<string, string>>({})
const triggerBranch = ref('')
const triggerCommit = ref('')
const triggerError = ref('')

/** 已展开日志的任务集合（按 `job:attempt` 键；点任务卡日志按钮切换）。 */
const openLogs = ref<Set<string>>(new Set())

function logKey(job: JobViewDto): string {
  return `${job.name}:${job.attempt}`
}

function toggleLog(job: JobViewDto): void {
  const key = logKey(job)
  const next = new Set(openLogs.value)
  if (next.has(key)) next.delete(key)
  else next.add(key)
  openLogs.value = next
}

function isLogOpen(job: JobViewDto): boolean {
  return openLogs.value.has(logKey(job))
}

/** 产物下载 URL（相对路径——cookie 会话随同源 `<a href>` 自动携带，票 #74）。 */
function artifactDownloadUrl(name: string): string {
  return artifactsApi.downloadUrl(
    project.value,
    pipeline.value,
    buildNumber.value,
    name,
  )
}

/** 触发对话框表单：从 pipeline 定义参数声明派生（ADR-0006 参数化）。 */
const parameterDecls = computed(() => definition.value?.parameters ?? [])

function openTrigger(): void {
  // 预填参数默认值（手动触发可覆盖默认值，ADR-0006）。
  const prefill: Record<string, string> = {}
  for (const p of parameterDecls.value) {
    if (p.default != null) prefill[p.name] = String(p.default)
  }
  triggerParams.value = prefill
  triggerBranch.value = ''
  triggerCommit.value = ''
  triggerError.value = ''
  triggerOpen.value = true
}

async function submitTrigger(): Promise<void> {
  triggerError.value = ''
  busy.value = 'trigger'
  try {
    // 参数覆盖统一字符串形态（后端 BTreeMap<String,String>；v-model 在
    // number/bool 输入上是数值，须归一）。
    const params: Record<string, string> = {}
    for (const [name, value] of Object.entries(triggerParams.value)) {
      if (value != null && value !== '') params[name] = String(value)
    }
    const req: TriggerBuildRequest = {
      params,
      branch: triggerBranch.value || null,
      commit: triggerCommit.value || null,
    }
    const accepted = await store.trigger(project.value, pipeline.value, req)
    triggerOpen.value = false
    // 触发返回新构建号：跳转到新构建详情（202 受理反馈）。
    await router.push({
      name: 'build-detail',
      params: {
        name: project.value,
        pipeline: pipeline.value,
        number: String(accepted.number),
      },
    })
  } catch (err) {
    // 409 触发冲突（同号在跑）与重跑冲突共用 describeActionError 会串味，
    // 触发单独给文案；其余按统一分支。
    triggerError.value =
      err instanceof ApiError && err.status === 409
        ? t('buildDetail.triggerConflict')
        : describeActionError(err)
  } finally {
    busy.value = null
  }
}

async function cancelBuild(): Promise<void> {
  busy.value = 'cancel'
  try {
    await store.cancel(project.value, pipeline.value, buildNumber.value)
    message.success(t('buildDetail.cancelledAccepted'))
  } catch (err) {
    message.error(describeActionError(err))
  } finally {
    busy.value = null
  }
}

async function rerunBuild(mode: RerunBuildRequest['mode']): Promise<void> {
  busy.value = 'rerun'
  try {
    const accepted = await store.rerun(project.value, pipeline.value, buildNumber.value, {
      mode,
    })
    if (mode === 'from_scratch') {
      // 从头重跑：新构建号，跳转新构建详情。
      await router.push({
        name: 'build-detail',
        params: {
          name: project.value,
          pipeline: pipeline.value,
          number: String(accepted.number),
        },
      })
    } else {
      // 从失败重跑：同号 attempt+1，store 已原地刷新详情。
      message.success(t('buildDetail.rerunAccepted'))
    }
  } catch (err) {
    message.error(describeActionError(err))
  } finally {
    busy.value = null
  }
}

/** 删除构建（票 #78，ADR-0013）：项目 admin 档全删该构建的日志与产物
 *  （记录保留）；运行中/排队禁用 + 后端 409 兜底。204 后跳回构建列表。 */
async function submitDelete(): Promise<void> {
  busy.value = 'delete'
  try {
    await store.remove(project.value, pipeline.value, buildNumber.value)
    await router.push({
      name: 'build-list',
      params: { name: project.value, pipeline: pipeline.value },
    })
  } catch (err) {
    message.error(describeActionError(err))
  } finally {
    busy.value = null
  }
}

// ---------------------------------------------------------------------------
// 状态徽章 / 阶段派生
// ---------------------------------------------------------------------------

function buildStatusKey(status: string): string {
  return `buildStatus.${status}`
}
function jobStatusKey(status: JobViewDto['status']): string {
  return `jobStatus.${status}`
}
function triggerKey(trigger: string): string {
  return `triggerSource.${trigger}`
}

/** 任务是否可从失败重跑（仅 failed / cancelled / timeout 终态；其余禁用）。 */
const rerunFailedEligible = computed(() => {
  const s = build.value?.status
  return s === 'failed' || s === 'cancelled' || s === 'timeout'
})

/** 阶段内当前 attempt 的任务进度（已落定 / 总数；与流水线页 P3 同口径，
 *  共享 settledPercent）。非运行中构建不展示（静态终态无进度语义）。 */
function stageProgress(stage: StageViewDto): number | null {
  if (build.value == null || !isLiveStatus(build.value.status)) return null
  return settledPercent(stage.jobs.filter((j) => j.attempt === build.value?.attempt))
}

/** 排队任务等待态：从 pipeline 定义 labels 声明派生缺失标签（ADR-0008）。
 *  详情 REST 不含 waiting_detail，定义缺失时退化为空并显式标注（waitingDegraded）。 */
function jobWaitingLabels(stage: StageViewDto, job: JobViewDto): string[] {
  return store.jobLabels(stage.index, job.name)
}

const waitingDegraded = computed(
  () => definition.value == null && build.value?.stages.some((s) => s.jobs.some((j) => j.status === 'queued')),
)

// ---------------------------------------------------------------------------
// 生命周期
// ---------------------------------------------------------------------------

onMounted(() => {
  void store.load(project.value, pipeline.value, buildNumber.value)
})

watch(
  () => [project.value, pipeline.value, buildNumber.value],
  () => {
    void store.load(project.value, pipeline.value, buildNumber.value)
  },
)

onBeforeUnmount(() => {
  store.dispose()
})

/** 参数表单：按类型给输入控件（string/number → NInput，bool → NSelect 真假，
 *  enum → NSelect 选项；均字符串形态回填 triggerParams）。 */
function paramControl(p: { name: string; type: 'string' | 'number' | 'bool' | 'enum'; choices?: string[] }) {
  if (p.type === 'bool') {
    return h(
      NSelect,
      {
        value: triggerParams.value[p.name] ?? '',
        'onUpdate:value': (v: string | null) => {
          triggerParams.value[p.name] = v ?? ''
        },
        options: [
          { label: 'true', value: 'true' },
          { label: 'false', value: 'false' },
        ],
        'virtual-scroll': false,
      },
    )
  }
  if (p.type === 'enum') {
    return h(
      NSelect,
      {
        value: triggerParams.value[p.name] ?? '',
        'onUpdate:value': (v: string | null) => {
          triggerParams.value[p.name] = v ?? ''
        },
        options: (p.choices ?? []).map((c) => ({ label: c, value: c })),
        'virtual-scroll': false,
      },
    )
  }
  return h(NInput, {
    value: triggerParams.value[p.name] ?? '',
    'onUpdate:value': (v: string | null) => {
      triggerParams.value[p.name] = v ?? ''
    },
    'input-props': { name: `param-${p.name}` },
    type: 'text',
    placeholder: p.name,
  })
}
</script>

<template>
  <!-- 首载骨架屏（事实态纪律——数据到达后替换）。 -->
  <div v-if="store.status === 'loading'" class="build-page" data-testid="build-detail-skeleton">
    <n-skeleton text :repeat="1" height="32px" class="build-skeleton-row" />
    <n-skeleton text :repeat="2" height="56px" class="build-skeleton-row" />
    <n-skeleton text :repeat="4" height="72px" class="build-skeleton-row" />
  </div>

  <div v-else-if="store.status === 'not-found'" class="build-page">
    <n-alert type="error" :title="t('buildDetail.notFound')" role="alert" />
  </div>

  <!-- 403 退化态：项目不可见（会话过期/无项目权限），不渲染详情体。 -->
  <div v-else-if="store.status === 'forbidden'" class="build-page">
    <p class="form-hint" data-testid="build-detail-forbidden">{{ t('buildDetail.forbidden') }}</p>
    <router-link class="card-link" :to="{ name: 'projects' }">
      {{ t('buildDetail.backToProjects') }}
    </router-link>
  </div>

  <!-- 加载失败：整页报错 + 重试（事实态纪律，与流水线页同形）。 -->
  <div v-else-if="store.status === 'error'" class="build-page">
    <n-alert type="error" :title="store.errorMessage" role="alert">
      <button type="button" class="btn-outline blue" data-testid="build-detail-retry" @click="store.load(project, pipeline, buildNumber)">
        {{ t('buildDetail.retry') }}
      </button>
    </n-alert>
  </div>

  <div v-else-if="build" class="build-page">
    <!-- 面包屑：项目 > pipeline > 构建号（ADR-0020 唯一 IA 修正）。 -->
    <nav class="breadcrumb" aria-label="Breadcrumb">
      <router-link :to="{ name: 'projects' }">{{ t('routes.projects') }}</router-link>
      <span class="breadcrumb-sep">/</span>
      <router-link :to="{ name: 'project-detail', params: { name: project } }">
        {{ project }}
      </router-link>
      <span class="breadcrumb-sep">/</span>
      <router-link :to="{ name: 'build-list', params: { name: project, pipeline } }">
        {{ pipeline }}
      </router-link>
      <span class="breadcrumb-sep">/</span>
      <span class="breadcrumb-current">{{ t('buildDetail.buildLabel') }} #{{ build.number }}</span>
    </nav>

    <!-- 页头：标题 + 状态胶囊 + 动作（描边小按钮，与流水线页行内动作同集）。 -->
    <header class="page-header build-header">
      <div class="build-title-row">
        <h1 class="page-title build-title">{{ pipeline }} #{{ build.number }}</h1>
        <span class="badge" :class="statusBadgeClass(build.status)">
          {{ t(buildStatusKey(build.status)) }}
        </span>
      </div>
      <div class="build-actions">
        <button type="button" class="btn-outline blue" data-testid="trigger-btn" :disabled="busy !== null" @click="openTrigger">
          {{ t('buildDetail.trigger') }}
        </button>
        <button
          type="button"
          class="btn-outline red"
          data-testid="cancel-btn"
          :disabled="busy !== null || !isLiveStatus(build.status)"
          @click="cancelBuild"
        >
          {{ t('buildDetail.cancel') }}
        </button>
        <button type="button" class="btn-outline blue" data-testid="rerun-scratch-btn" :disabled="busy !== null" @click="rerunBuild('from_scratch')">
          {{ t('buildDetail.rerunFromScratch') }}
        </button>
        <button
          type="button"
          class="btn-outline orange"
          data-testid="rerun-failed-btn"
          :disabled="busy !== null || !rerunFailedEligible"
          @click="rerunBuild('from_failed')"
        >
          {{ t('buildDetail.rerunFromFailed') }}
        </button>
        <n-popconfirm
          :positive-text="t('common.confirm')"
          :negative-text="t('common.cancel')"
          @positive-click="submitDelete"
        >
          <template #trigger>
            <button
              type="button"
              class="btn-outline red"
              data-testid="delete-btn"
              :disabled="busy !== null || isLiveStatus(build.status)"
            >
              {{ t('buildDetail.delete') }}
            </button>
          </template>
          {{ t('buildDetail.deleteConfirm', { number: build.number }) }}
        </n-popconfirm>
      </div>
    </header>

    <!-- 元信息条（触发人 / 触发源 / attempt / 开始 / 结束 / 耗时）。 -->
    <section class="sisy-card build-meta-card" aria-label="build meta">
      <dl class="build-meta">
        <div class="build-meta-item">
          <dt>{{ t('buildDetail.triggerBy') }}</dt>
          <dd>{{ build.trigger_by }}</dd>
        </div>
        <div class="build-meta-item">
          <dt>{{ t('buildDetail.triggerSource') }}</dt>
          <dd>
            <span class="trigger-tag">{{ t(triggerKey(build.trigger)) }}</span>
          </dd>
        </div>
        <div class="build-meta-item">
          <dt>{{ t('buildDetail.attempt') }}</dt>
          <dd>{{ build.attempt }}</dd>
        </div>
        <div class="build-meta-item">
          <dt>{{ t('buildDetail.startedAt') }}</dt>
          <dd>{{ build.started_at ? formatDateTime(build.started_at) : '—' }}</dd>
        </div>
        <div class="build-meta-item">
          <dt>{{ t('buildDetail.finishedAt') }}</dt>
          <dd>{{ build.finished_at ? formatDateTime(build.finished_at) : '—' }}</dd>
        </div>
        <div class="build-meta-item">
          <dt>{{ t('buildDetail.elapsed') }}</dt>
          <dd>{{ formatDuration(build.elapsed_ms) }}</dd>
        </div>
      </dl>
    </section>

    <!-- 阶段/任务卡：按快照阶段序；排队任务缺失标签等待态。 -->
    <section class="sisy-card build-stages" aria-label="stages and jobs">
      <div class="card-header">
        <h2 class="card-title">{{ t('buildDetail.stages') }}</h2>
      </div>

      <div class="build-stages-body">
        <div v-if="waitingDegraded" class="state-note" role="status">
          {{ t('buildDetail.waitingDegraded') }}
        </div>

        <!-- 空态：构建无阶段/任务记录。 -->
        <div v-if="build.stages.length === 0" class="build-stages-empty">
          <n-empty :description="t('buildDetail.emptyStages')" />
        </div>

        <section v-for="stage in build.stages" :key="stage.index" class="stage-block">
          <header class="stage-head">
            <span class="stage-index">{{ stage.index + 1 }}</span>
            <span class="stage-name">{{ stage.name || t('buildDetail.unnamedStage') }}</span>
            <span v-if="stageProgress(stage) != null" class="usage-row stage-progress">
              <span class="track">
                <span class="fill" :style="{ width: `${stageProgress(stage)}%` }" />
              </span>
              <span class="pct">{{ stageProgress(stage) }}%</span>
            </span>
          </header>

          <ul class="job-list">
            <li
              v-for="job in stage.jobs"
              :key="`${job.name}-${job.attempt}`"
              class="job-card"
              :class="`job-${job.status}`"
            >
              <div class="job-head">
                <span class="job-name">{{ job.name }}</span>
                <span class="badge" :class="statusBadgeClass(job.status)">
                  {{ t(jobStatusKey(job.status)) }}
                </span>
                <span v-if="job.attempt > 1" class="job-attempt">
                  {{ t('buildDetail.attemptLabel', { attempt: job.attempt }) }}
                </span>
                <span v-if="job.allow_failure" class="badge neutral allow-failure">
                  {{ t('buildDetail.allowFailure') }}
                </span>
              </div>

              <!-- 排队等待态：缺失标签等待匹配 agent（ADR-0008）。 -->
              <div v-if="job.status === 'queued'" class="state-note job-waiting">
                <template v-if="jobWaitingLabels(stage, job).length > 0">
                  {{ t('buildDetail.waitingMissingLabels', {
                    labels: jobWaitingLabels(stage, job).join(', '),
                  }) }}
                </template>
                <template v-else>
                  {{ t('buildDetail.waitingNoLabels') }}
                </template>
              </div>

              <div v-if="job.detail" class="job-detail">{{ job.detail }}</div>

              <div class="job-meta">
                <span v-if="job.started_at">
                  {{ t('buildDetail.startedAt') }}: {{ formatDateTime(job.started_at) }}
                </span>
                <span v-if="job.finished_at">
                  {{ t('buildDetail.finishedAt') }}: {{ formatDateTime(job.finished_at) }}
                </span>
                <span v-if="job.exit_code != null">
                  {{ t('buildDetail.exitCode') }}: {{ job.exit_code }}
                </span>
                <span v-if="job.agent_id != null">
                  {{ t('buildDetail.agentId') }}: {{ job.agent_id }}
                </span>
              </div>

              <!-- 产物区（票 #74 解禁）：任务声明 × 已上传产物比对——已上传接
                   下载链接（大小/校验和提示，cookie 会话随同源导航自动携带）；
                   未上传展示占位（构建进行中/任务未成功）。 -->
              <div
                v-if="store.jobArtifactUploads(stage.index, job.name).length > 0"
                class="job-artifacts"
              >
                <span class="job-artifacts-label">{{ t('buildDetail.artifacts') }}:</span>
                <template
                  v-for="(art, i) in store.jobArtifactUploads(stage.index, job.name)"
                  :key="`${art.name}-${i}`"
                >
                  <a
                    v-if="store.uploadedArtifact(art.name)"
                    class="btn-outline blue artifact-link"
                    :href="artifactDownloadUrl(art.name)"
                    :title="`${art.path}\n${store.uploadedArtifact(art.name)!.sha256}`"
                    :download="art.name"
                  >
                    {{ art.name }}
                    <span class="artifact-size">{{ formatBytes(store.uploadedArtifact(art.name)!.size) }}</span>
                  </a>
                  <span v-else class="artifact-chip" :title="art.path">
                    {{ art.name }}
                    <span class="artifact-placeholder">{{ t('buildDetail.artifactPlaceholder') }}</span>
                  </span>
                </template>
              </div>

              <!-- 日志入口：展开/收起该任务的 SSE 日志流（步骤折叠/ANSI/截断/重连）。 -->
              <button
                type="button"
                class="btn-outline job-log-toggle"
                data-testid="job-log-toggle"
                :aria-expanded="isLogOpen(job)"
                @click="toggleLog(job)"
              >
                {{ isLogOpen(job) ? t('buildLog.hideLog') : t('buildLog.showLog') }}
              </button>
              <BuildLogView
                v-if="isLogOpen(job)"
                :project="project"
                :pipeline="pipeline"
                :build-number="build.number"
                :job="job.name"
                :attempt="job.attempt"
              />
            </li>
          </ul>
        </section>
      </div>
    </section>

    <!-- 触发对话框（NModal + NForm：参数覆盖 + 分支/commit）。 -->
    <n-modal
      v-model:show="triggerOpen"
      preset="card"
      :title="t('buildDetail.triggerTitle')"
      style="width: 480px"
      :bordered="false"
    >
      <n-form
        :model="{ params: triggerParams }"
        label-placement="top"
        @submit.prevent="submitTrigger"
      >
        <template v-if="parameterDecls.length > 0">
          <n-form-item
            v-for="p in parameterDecls"
            :key="p.name"
            :label="p.name"
            :show-require-mark="p.required"
          >
            <component :is="paramControl(p)" />
          </n-form-item>
        </template>
        <p v-else class="form-hint">{{ t('buildDetail.noParams') }}</p>

        <n-form-item :label="t('buildDetail.branch')" path="triggerBranch">
          <n-input
            v-model:value="triggerBranch"
            :input-props="{ name: 'trigger-branch' }"
            :placeholder="t('buildDetail.branch')"
          />
        </n-form-item>

        <n-form-item :label="t('buildDetail.commit')" path="triggerCommit">
          <n-input
            v-model:value="triggerCommit"
            :input-props="{ name: 'trigger-commit' }"
            :placeholder="t('buildDetail.commit')"
          />
        </n-form-item>

        <p v-if="triggerError" class="build-error" role="alert">{{ triggerError }}</p>

        <div class="modal-actions">
          <n-button @click="triggerOpen = false">
            {{ t('common.cancel') }}
          </n-button>
          <n-button
            type="primary"
            :disabled="busy !== null"
            :loading="busy === 'trigger'"
            @click="submitTrigger"
          >
            {{ t('buildDetail.trigger') }}
          </n-button>
        </div>
      </n-form>
    </n-modal>
  </div>
</template>

<style scoped>
.build-page {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.build-skeleton-row {
  width: 100%;
}

/* 页头：标题 + 状态胶囊 + 动作。 */
.build-header {
  align-items: flex-start;
  flex-wrap: wrap;
}

.build-title-row {
  display: flex;
  align-items: center;
  gap: 12px;
  min-width: 0;
}

.build-title {
  margin-bottom: 0;
  font-size: 22px;
}

.build-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}

/* 元信息条。 */
.build-meta {
  margin: 0;
  display: flex;
  gap: 8px 32px;
  flex-wrap: wrap;
  padding: 14px 20px;
}

.build-meta-item {
  display: flex;
  flex-direction: column;
  gap: 3px;
  min-width: 96px;
}

.build-meta-item dt {
  color: var(--sisy-color-text-secondary);
  font-size: 11px;
}

.build-meta-item dd {
  margin: 0;
  font-size: 13px;
  font-weight: 500;
  color: var(--sisy-color-text);
}

.trigger-tag {
  display: inline-flex;
  align-items: center;
  height: 22px;
  padding: 0 10px;
  border-radius: var(--sisy-radius-pill);
  background: var(--sisy-color-bg);
  font-size: 11px;
  font-weight: 400;
  color: var(--sisy-color-text-secondary);
}

/* 阶段/任务卡。 */
.build-stages-body {
  display: flex;
  flex-direction: column;
  gap: 20px;
  padding: 4px 20px 20px;
}

.build-stages-empty {
  padding: 24px 0;
}

/* 事实态提示条（等待态退化标注等）：见 main.css 共享类 .state-note。 */

.stage-block {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.stage-head {
  display: flex;
  align-items: center;
  gap: 10px;
}

.stage-index {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 22px;
  height: 22px;
  border-radius: 50%;
  background: var(--sisy-color-primary-soft);
  color: var(--sisy-color-primary);
  font-size: 12px;
  font-weight: 600;
  flex-shrink: 0;
}

.stage-name {
  font-size: 14px;
  font-weight: 600;
  color: var(--sisy-color-text);
}

.stage-progress {
  margin-left: auto;
}

.job-list {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.job-card {
  border: 1px solid var(--sisy-color-border);
  border-left: 3px solid var(--sisy-color-border);
  border-radius: var(--sisy-radius);
  padding: 10px 14px;
  background: var(--sisy-color-surface);
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.job-card.job-succeeded {
  border-left-color: var(--sisy-color-success);
}

.job-card.job-failed,
.job-card.job-aborted {
  border-left-color: var(--sisy-color-danger);
}

.job-card.job-running {
  border-left-color: var(--sisy-color-primary);
}

.job-card.job-queued {
  border-left-color: var(--sisy-color-warning);
}

.job-card.job-cancelled,
.job-card.job-skipped {
  border-left-color: var(--sisy-color-offline);
}

.job-card.job-timeout,
.job-card.job-unknown {
  border-left-color: var(--sisy-color-warning);
}

.job-head {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}

.job-name {
  font-size: 13px;
  font-weight: 600;
  color: var(--sisy-color-text);
}

.job-attempt {
  font-size: 11px;
  color: var(--sisy-color-text-secondary);
}

.badge.allow-failure {
  /* 允失败角标：中性胶囊，弱于状态徽章。 */
  font-weight: 400;
}

.job-waiting {
  align-self: flex-start;
}

.job-detail {
  font-size: 12px;
  color: var(--sisy-color-text-secondary);
}

.job-meta {
  display: flex;
  gap: 12px;
  font-size: 11px;
  color: var(--sisy-color-text-tertiary);
  flex-wrap: wrap;
}

.job-artifacts {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-wrap: wrap;
  font-size: 12px;
}

.job-artifacts-label {
  color: var(--sisy-color-text-secondary);
}

.artifact-chip {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  border: 1px dashed var(--sisy-color-border);
  border-radius: var(--sisy-radius-pill);
  padding: 2px 10px;
  background: var(--sisy-color-bg);
  font-size: 11px;
  color: var(--sisy-color-text-secondary);
}

.artifact-placeholder {
  color: var(--sisy-color-text-tertiary);
  font-size: 10px;
}

.artifact-link {
  text-decoration: none;
  height: 24px;
  padding: 0 10px;
}

.artifact-size {
  color: var(--sisy-color-text-tertiary);
  font-size: 10px;
}

.job-log-toggle {
  align-self: flex-start;
}

.build-error {
  margin: 0;
  color: var(--sisy-color-danger-text);
  font-size: 13px;
}

.modal-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding-top: 8px;
}
</style>

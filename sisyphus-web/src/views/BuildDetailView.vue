<script setup lang="ts">
// 构建详情页（票 B4-T4，ADR-0006/0008/0013/0020）。
//
// - 面包屑：项目 > pipeline > 构建号（ADR-0020 原型唯一 IA 修正）。
// - 阶段/任务卡：按构建快照阶段序（REST 详情 `stages`）；排队任务显示
//   缺失标签等待态（ADR-0008「等待匹配 agent：缺标签 X」——REST 详情不含
//   waiting_detail，从 pipeline 定义的 labels 声明派生，定义缺失时显式标注）；
//   任务状态含 attempt 历史（重跑后同任务多行并列）。
// - 触发/取消/重跑入口：触发带参数覆盖/分支/commit（`POST .../builds`）、
//   `POST .../builds/{number}/cancel`、`POST .../builds/{number}/rerun` 含
//   from_scratch / from_failed 两模式；操作结果 202 受理 / 409 拒绝正确反馈。
// - 产物区（票 #74 解禁）：任务声明 × 已上传产物比对——已上传接下载链接
//   （大小/校验和提示、cookie 会话随同源导航自动携带）；未上传展示占位
//   （构建进行中/任务未成功）。
// #93: 使用 Naive UI 组件重写——阶段/任务卡改 NCard + NTag 状态徽章、触发
// 弹窗改 NModal + NForm（参数覆盖 + 分支/提交输入）、产物下载改 NButton +
// 下载图标、删除/取消/重跑改 NPopconfirm 确认、状态统一色标 NTag；
// 视觉与 #84/#86 主题一致。

import { computed, h, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'
import {
  NAlert,
  NButton,
  NCard,
  NForm,
  NFormItem,
  NIcon,
  NInput,
  NModal,
  NPopconfirm,
  NSelect,
  NTag,
} from 'naive-ui'
import { CloudDownloadOutline, PlayOutline, RefreshOutline } from '@vicons/ionicons5'

import { artifactsApi } from '@/api/client'
import { describeActionError } from '@/api/errors'
import { useBuildDetailStore } from '@/stores/buildDetail'
import {
  formatBytes,
  formatDateTime,
  formatDuration,
  isLiveStatus,
} from '@/utils/format'
import BuildLogView from '@/components/BuildLogView.vue'
import type {
  BuildDetailResponse,
  JobViewDto,
  RerunBuildRequest,
  StageViewDto,
  TriggerBuildRequest,
} from '@/api/types'

const route = useRoute()
const router = useRouter()
const { t } = useI18n()
const store = useBuildDetailStore()

const project = computed(() => String(route.params.name ?? ''))
const pipeline = computed(() => String(route.params.pipeline ?? ''))
const buildNumber = computed(() => Number(route.params.number))

const build = computed(() => store.build)
const definition = computed(() => store.definition)

// ---------------------------------------------------------------------------
// 触发对话框 / 操作反馈
// ---------------------------------------------------------------------------

const triggerOpen = ref(false)
const triggerParams = ref<Record<string, string>>({})
const triggerBranch = ref('')
const triggerCommit = ref('')
const triggerError = ref('')
const deleteError = ref('')
const actionBusy = ref<'trigger' | 'cancel' | 'rerun' | 'delete' | null>(null)
const actionMessage = ref('')
const actionError = ref('')

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
  actionBusy.value = 'trigger'
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
    triggerError.value = describeActionError(err)
  } finally {
    actionBusy.value = null
  }
}

async function cancelBuild(): Promise<void> {
  actionBusy.value = 'cancel'
  actionError.value = ''
  actionMessage.value = ''
  try {
    await store.cancel(project.value, pipeline.value, buildNumber.value)
    actionMessage.value = t('buildDetail.cancelledAccepted')
  } catch (err) {
    actionError.value = describeActionError(err)
  } finally {
    actionBusy.value = null
  }
}

async function rerunBuild(mode: RerunBuildRequest['mode']): Promise<void> {
  actionBusy.value = 'rerun'
  actionError.value = ''
  actionMessage.value = ''
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
      // 从失败任务重跑：同号 attempt+1，原地刷新详情。
      actionMessage.value = t('buildDetail.rerunAccepted')
    }
  } catch (err) {
    actionError.value = describeActionError(err)
  } finally {
    actionBusy.value = null
  }
}

/** 打开删除确认（NPopconfirm 内触发；运行中/排队已禁用按钮，此处为兜底语义）。 */
async function submitDelete(): Promise<void> {
  deleteError.value = ''
  actionBusy.value = 'delete'
  try {
    // 项目 admin 档全删该构建的日志与产物（记录保留，ADR-0013）；
    // 204 后跳回构建列表。运行中/排队后端 409 在此反馈。
    await store.remove(project.value, pipeline.value, buildNumber.value)
    await router.push({
      name: 'build-list',
      params: { name: project.value, pipeline: pipeline.value },
    })
  } catch (err) {
    deleteError.value = describeActionError(err)
  } finally {
    actionBusy.value = null
  }
}

// ---------------------------------------------------------------------------
// 阶段/任务卡派生
// ---------------------------------------------------------------------------

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

// 供模板使用的辅助
function buildStatusKey(status: BuildDetailResponse['status']): string {
  return `buildStatus.${status}`
}
function jobStatusKey(status: JobViewDto['status']): string {
  return `jobStatus.${status}`
}
function triggerKey(trigger: BuildDetailResponse['trigger']): string {
  return `triggerSource.${trigger}`
}

/** 构建/任务状态 → NTag 状态色（成功=绿 / 失败=红 / 运行=蓝 / 取消=灰 /
 *  排队/超时=黄，与 Overview 最近构建列同色系，主题 Token 驱动）。 */
function statusType(status: string): 'success' | 'error' | 'info' | 'warning' | 'default' {
  switch (status) {
    case 'succeeded':
      return 'success'
    case 'failed':
    case 'aborted':
      return 'error'
    case 'running':
      return 'info'
    case 'queued':
    case 'timeout':
    case 'unknown':
      return 'warning'
    default:
      return 'default'
  }
}

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
  <div v-if="store.status === 'loading'" class="build-page">
    <p class="build-muted">{{ t('buildDetail.loading') }}</p>
  </div>

  <div v-else-if="store.status === 'not-found'" class="build-page">
    <n-alert type="error" :title="t('buildDetail.notFound')" role="alert" />
  </div>

  <div v-else-if="store.status === 'error'" class="build-page">
    <n-alert type="error" :title="store.errorMessage" role="alert" />
  </div>

  <div v-else-if="build" class="build-page">
    <!-- 面包屑：项目 > pipeline > 构建号（ADR-0020 唯一 IA 修正）。 -->
    <nav class="breadcrumb" aria-label="Breadcrumb">
      <router-link :to="{ name: 'projects' }">{{ t('routes.projects') }}</router-link>
      <span class="breadcrumb-sep">/</span>
      <router-link
        :to="{ name: 'project-detail', params: { name: project } }"
      >
        {{ project }}
      </router-link>
      <span class="breadcrumb-sep">/</span>
      <router-link
        :to="{ name: 'pipeline-edit', params: { name: project, pipeline } }"
      >
        {{ pipeline }}
      </router-link>
      <span class="breadcrumb-sep">/</span>
      <span class="breadcrumb-current">{{ t('buildDetail.buildLabel') }} #{{ build.number }}</span>
    </nav>

    <header class="build-header">
      <h1 class="build-title">
        {{ pipeline }} #{{ build.number }}
      </h1>
      <n-tag :type="statusType(build.status)" size="small" :bordered="false">
        {{ t(buildStatusKey(build.status)) }}
      </n-tag>
    </header>

    <dl class="build-meta">
      <div class="build-meta-item">
        <dt>{{ t('buildDetail.triggerBy') }}</dt>
        <dd>{{ build.trigger_by }}</dd>
      </div>
      <div class="build-meta-item">
        <dt>{{ t('buildDetail.triggerSource') }}</dt>
        <dd>
          <n-tag size="small" type="default" :bordered="false">
            {{ t(triggerKey(build.trigger)) }}
          </n-tag>
        </dd>
      </div>
      <div class="build-meta-item">
        <dt>{{ t('buildDetail.attempt') }}</dt>
        <dd>{{ build.attempt }}</dd>
      </div>
      <div class="build-meta-item">
        <dt>{{ t('buildDetail.startedAt') }}</dt>
        <dd>{{ formatDateTime(build.started_at) }}</dd>
      </div>
      <div class="build-meta-item">
        <dt>{{ t('buildDetail.finishedAt') }}</dt>
        <dd>{{ formatDateTime(build.finished_at) }}</dd>
      </div>
      <div class="build-meta-item">
        <dt>{{ t('buildDetail.elapsed') }}</dt>
        <dd>{{ formatDuration(build.elapsed_ms) }}</dd>
      </div>
    </dl>

    <!-- 操作区：触发 / 取消 / 重跑 / 删除（runner 档动作；409 拒绝反馈）。 -->
    <div class="build-actions">
      <n-button type="primary" :disabled="actionBusy !== null" @click="openTrigger">
        <template #icon>
          <n-icon :component="PlayOutline" />
        </template>
        {{ t('buildDetail.trigger') }}
      </n-button>
      <n-popconfirm
        :positive-text="t('common.confirm')"
        :negative-text="t('common.cancel')"
        @positive-click="cancelBuild"
      >
        <template #trigger>
          <n-button
            :disabled="actionBusy !== null || !isLiveStatus(build.status)"
          >
            {{ t('buildDetail.cancel') }}
          </n-button>
        </template>
        {{ t('buildDetail.cancelConfirm') }}
      </n-popconfirm>
      <n-popconfirm
        :positive-text="t('common.confirm')"
        :negative-text="t('common.cancel')"
        @positive-click="rerunBuild('from_scratch')"
      >
        <template #trigger>
          <n-button :disabled="actionBusy !== null">
            <template #icon>
              <n-icon :component="RefreshOutline" />
            </template>
            {{ t('buildDetail.rerunFromScratch') }}
          </n-button>
        </template>
        {{ t('buildDetail.rerunConfirm') }}
      </n-popconfirm>
      <n-popconfirm
        :positive-text="t('common.confirm')"
        :negative-text="t('common.cancel')"
        @positive-click="rerunBuild('from_failed')"
      >
        <template #trigger>
          <n-button :disabled="actionBusy !== null">
            {{ t('buildDetail.rerunFromFailed') }}
          </n-button>
        </template>
        {{ t('buildDetail.rerunConfirm') }}
      </n-popconfirm>
      <!-- 手动删构建（票 #78，ADR-0013）：项目 admin 档；运行中/排队禁用 +
            后端 409 兜底。确认后 204 即跳回构建列表。 -->
      <n-popconfirm
        :positive-text="t('common.confirm')"
        :negative-text="t('common.cancel')"
        @positive-click="submitDelete"
      >
        <template #trigger>
          <n-button
            type="error"
            :disabled="actionBusy !== null || isLiveStatus(build.status)"
          >
            {{ t('buildDetail.delete') }}
          </n-button>
        </template>
        {{ t('buildDetail.deleteConfirm', { number: build.number }) }}
      </n-popconfirm>
      <span v-if="actionBusy" class="build-action-busy">
        {{ t('buildDetail.submitting') }}
      </span>
    </div>

    <p v-if="actionMessage" class="build-action-message" role="status">{{ actionMessage }}</p>
    <p v-if="actionError" class="build-error" role="alert">{{ actionError }}</p>
    <p v-if="deleteError" class="build-error" role="alert">{{ deleteError }}</p>

    <!-- 阶段/任务卡：按快照阶段序；排队任务缺失标签等待态。 -->
    <section class="build-stages">
      <h2>{{ t('buildDetail.stages') }}</h2>

      <div v-if="waitingDegraded" class="build-degraded" role="status">
        {{ t('buildDetail.waitingDegraded') }}
      </div>

      <n-card
        v-for="stage in build.stages"
        :key="stage.index"
        class="stage-card"
        size="small"
        :bordered="true"
      >
        <template #header>
          <span class="stage-name">
            <span class="stage-index">{{ stage.index + 1 }}</span>
            {{ stage.name || t('buildDetail.unnamedStage') }}
          </span>
        </template>

        <ul class="job-list">
          <li
            v-for="job in stage.jobs"
            :key="`${job.name}-${job.attempt}`"
            class="job-card"
            :class="`job-${job.status}`"
          >
            <div class="job-head">
              <span class="job-name">{{ job.name }}</span>
              <n-tag :type="statusType(job.status)" size="small" :bordered="false">
                {{ t(jobStatusKey(job.status)) }}
              </n-tag>
              <span v-if="job.attempt > 1" class="job-attempt">
                {{ t('buildDetail.attemptLabel', { attempt: job.attempt }) }}
              </span>
            </div>

            <div v-if="job.allow_failure" class="job-badge allow-failure">
              {{ t('buildDetail.allowFailure') }}
            </div>

            <!-- 排队等待态：缺失标签等待 agent（ADR-0008）。 -->
            <div v-if="job.status === 'queued'" class="job-waiting">
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

            <!-- 产物区（票 #74 解禁）：任务声明 × 已上传产物比对——已上传接下载
                 链接（大小/校验和提示，cookie 会话随同源导航自动携带）；未上传
                 展示占位（构建进行中/任务未成功）。 -->
            <div
              v-if="store.jobArtifactUploads(stage.index, job.name).length > 0"
              class="job-artifacts"
            >
              <span class="job-artifacts-label">{{ t('buildDetail.artifacts') }}:</span>
              <template
                v-for="(art, i) in store.jobArtifactUploads(stage.index, job.name)"
                :key="`${art.name}-${i}`"
              >
                <n-button
                  v-if="store.uploadedArtifact(art.name)"
                  size="tiny"
                  tag="a"
                  :href="artifactDownloadUrl(art.name)"
                  :title="`${art.path}\n${store.uploadedArtifact(art.name)!.sha256}`"
                  :download="art.name"
                  class="artifact-link"
                >
                  <template #icon>
                    <n-icon :component="CloudDownloadOutline" />
                  </template>
                  {{ art.name }}
                  <span class="artifact-size">{{ formatBytes(store.uploadedArtifact(art.name)!.size) }}</span>
                </n-button>
                <span
                  v-else
                  class="artifact-chip"
                  :title="art.path"
                >
                  {{ art.name }}
                  <span class="artifact-placeholder">
                    {{ t('buildDetail.artifactPlaceholder') }}
                  </span>
                </span>
              </template>
            </div>

            <!-- 日志入口：展开/收起该任务的 SSE 日志流（步骤折叠/ANSI/截断/重连）。 -->
            <n-button
              size="tiny"
              quaternary
              class="job-log-toggle"
              :aria-expanded="isLogOpen(job)"
              @click="toggleLog(job)"
            >
              {{ isLogOpen(job) ? t('buildLog.hideLog') : t('buildLog.showLog') }}
            </n-button>
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
      </n-card>
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
      >        <template v-if="parameterDecls.length > 0">
          <n-form-item
            v-for="p in parameterDecls"
            :key="p.name"
            :label="p.name"
            :show-require-mark="p.required"
          >
            <component :is="paramControl(p)" />
          </n-form-item>
        </template>
        <p v-else class="build-muted">{{ t('buildDetail.noParams') }}</p>

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
            :disabled="actionBusy !== null"
            :loading="actionBusy === 'trigger'"
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

.build-header {
  display: flex;
  align-items: center;
  gap: 12px;
}

.build-title {
  margin: 0;
  font-size: 22px;
}

.build-meta {
  margin: 0;
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
  gap: 8px 16px;
  font-size: 13px;
}

.build-meta-item {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.build-meta-item dt {
  color: var(--n-text-color-3, #999);
  font-size: 12px;
}

.build-meta-item dd {
  margin: 0;
}

.build-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}

.build-action-busy {
  font-size: 13px;
  color: var(--n-text-color-3, #999);
}

.build-action-message {
  margin: 0;
  color: #1a7f37;
  font-size: 13px;
}

.build-error {
  margin: 0;
  color: var(--n-text-color-error, #d03050);
}

.build-muted {
  color: var(--n-text-color-3, #999);
}

.build-degraded {
  color: #8a4a0f;
  background: #fdf1e3;
  border: 1px solid #e2a56a;
  border-radius: var(--n-border-radius, 6px);
  padding: 8px 12px;
  font-size: 13px;
}

.build-stages {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.build-stages h2 {
  margin: 0;
  font-size: 16px;
}

.stage-name {
  display: flex;
  align-items: center;
  gap: 8px;
  font-weight: 600;
}

.stage-index {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 20px;
  height: 20px;
  border-radius: 50%;
  background: var(--n-color-primary, #2b5797);
  color: #fff;
  font-size: 12px;
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
  border: 1px solid var(--n-border-color, #d9dce1);
  border-left: 3px solid var(--n-border-color, #d9dce1);
  border-radius: var(--n-border-radius, 6px);
  padding: 8px 12px;
  background: var(--n-card-color, #fff);
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.job-card.job-succeeded {
  border-left-color: #18a058;
}

.job-card.job-failed,
.job-card.job-aborted {
  border-left-color: #d03050;
}

.job-card.job-running {
  border-left-color: #2080f0;
}

.job-card.job-queued {
  border-left-color: #f0a020;
}

.job-card.job-cancelled,
.job-card.job-skipped {
  border-left-color: #c4c9cf;
}

.job-card.job-timeout,
.job-card.job-unknown {
  border-left-color: #e2a56a;
}

.job-head {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}

.job-name {
  font-weight: 600;
}

.job-attempt {
  font-size: 12px;
  color: var(--n-text-color-3, #999);
}

.job-badge.allow-failure {
  font-size: 12px;
  color: #8a4a0f;
  border: 1px dashed #e2a56a;
  border-radius: 999px;
  padding: 0 8px;
  align-self: flex-start;
}

.job-waiting {
  font-size: 13px;
  color: #8a6d00;
  background: #fff8d6;
  border: 1px solid #e0c000;
  border-radius: var(--n-border-radius, 6px);
  padding: 4px 8px;
}

.job-detail {
  font-size: 13px;
  color: var(--n-text-color-3, #999);
}

.job-meta {
  display: flex;
  gap: 12px;
  font-size: 12px;
  color: var(--n-text-color-3, #999);
  flex-wrap: wrap;
}

.job-artifacts {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-wrap: wrap;
  font-size: 13px;
}

.job-artifacts-label {
  color: var(--n-text-color-3, #999);
}

.artifact-chip {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  border: 1px solid var(--n-border-color, #d9dce1);
  border-radius: 999px;
  padding: 2px 8px;
  background: var(--n-card-color, #fff);
  font-size: 12px;
}

.artifact-placeholder {
  color: var(--n-text-color-3, #999);
  font-size: 11px;
}

.artifact-link {
  text-decoration: none;
}

.artifact-size {
  color: var(--n-text-color-3, #999);
  font-size: 11px;
}

.job-log-toggle {
  align-self: flex-start;
}

.modal-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding-top: 8px;
}
</style>

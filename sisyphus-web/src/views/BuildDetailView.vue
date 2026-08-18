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
// - 产物区：产物下载端点未交付（B4 纯前端契约）→ 按任务声明展示 + 下载占位
//   （Spec B4 缺端点纪律：退化态 + 显式标注）。

import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'

import { describeActionError } from '@/api/errors'
import { useBuildDetailStore } from '@/stores/buildDetail'
import {
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
const actionBusy = ref<'trigger' | 'cancel' | 'rerun' | null>(null)
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
</script>

<template>
  <div v-if="store.status === 'loading'" class="build-page">
    <p class="build-muted">{{ t('buildDetail.loading') }}</p>
  </div>

  <div v-else-if="store.status === 'not-found'" class="build-page">
    <p class="build-error" role="alert">{{ t('buildDetail.notFound') }}</p>
  </div>

  <div v-else-if="store.status === 'error'" class="build-page">
    <p class="build-error" role="alert">{{ store.errorMessage }}</p>
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
      <span class="build-status-badge" :class="`status-${build.status}`">
        {{ t(buildStatusKey(build.status)) }}
      </span>
    </header>

    <dl class="build-meta">
      <div class="build-meta-item">
        <dt>{{ t('buildDetail.triggerBy') }}</dt>
        <dd>{{ build.trigger_by }}</dd>
      </div>
      <div class="build-meta-item">
        <dt>{{ t('buildDetail.triggerSource') }}</dt>
        <dd>{{ t(triggerKey(build.trigger)) }}</dd>
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

    <!-- 操作区：触发 / 取消 / 重跑（runner 档动作；409 拒绝反馈）。 -->
    <div class="build-actions">
      <button type="button" class="btn" @click="openTrigger" :disabled="actionBusy !== null">
        {{ t('buildDetail.trigger') }}
      </button>
      <button
        type="button"
        class="btn"
        @click="cancelBuild"
        :disabled="actionBusy !== null || !isLiveStatus(build.status)"
      >
        {{ t('buildDetail.cancel') }}
      </button>
      <button
        type="button"
        class="btn"
        @click="rerunBuild('from_scratch')"
        :disabled="actionBusy !== null"
      >
        {{ t('buildDetail.rerunFromScratch') }}
      </button>
      <button
        type="button"
        class="btn"
        @click="rerunBuild('from_failed')"
        :disabled="actionBusy !== null"
      >
        {{ t('buildDetail.rerunFromFailed') }}
      </button>
      <span v-if="actionBusy" class="build-action-busy">
        {{ t('buildDetail.submitting') }}
      </span>
    </div>

    <p v-if="actionMessage" class="build-action-message" role="status">{{ actionMessage }}</p>
    <p v-if="actionError" class="build-error" role="alert">{{ actionError }}</p>

    <!-- 阶段/任务卡：按快照阶段序；排队任务缺失标签等待态。 -->
    <section class="build-stages">
      <h2>{{ t('buildDetail.stages') }}</h2>

      <div v-if="waitingDegraded" class="build-degraded" role="status">
        {{ t('buildDetail.waitingDegraded') }}
      </div>

      <article v-for="stage in build.stages" :key="stage.index" class="stage-card">
        <h3 class="stage-name">
          <span class="stage-index">{{ stage.index + 1 }}</span>
          {{ stage.name || t('buildDetail.unnamedStage') }}
        </h3>

        <ul class="job-list">
          <li
            v-for="job in stage.jobs"
            :key="`${job.name}-${job.attempt}`"
            class="job-card"
            :class="`job-${job.status}`"
          >
            <div class="job-head">
              <span class="job-name">{{ job.name }}</span>
              <span class="job-status" :class="`status-${job.status}`">
                {{ t(jobStatusKey(job.status)) }}
              </span>
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

            <!-- 产物区（任务级声明展示）：下载端点未交付 → 下载占位。 -->
            <div
              v-if="store.jobArtifactUploads(stage.index, job.name).length > 0"
              class="job-artifacts"
            >
              <span class="job-artifacts-label">{{ t('buildDetail.artifacts') }}:</span>
              <span
                v-for="(art, i) in store.jobArtifactUploads(stage.index, job.name)"
                :key="`${art.name}-${i}`"
                class="artifact-chip"
                :title="art.path"
              >
                {{ art.name }}
                <span class="artifact-placeholder">
                  {{ t('buildDetail.artifactPlaceholder') }}
                </span>
              </span>
            </div>

            <!-- 日志入口：展开/收起该任务的 SSE 日志流（步骤折叠/ANSI/截断/重连）。 -->
            <button
              type="button"
              class="job-log-toggle"
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
      </article>
    </section>

    <!-- 触发对话框（参数覆盖 + 分支/commit）。 -->
    <div v-if="triggerOpen" class="modal-backdrop" @click.self="triggerOpen = false">
      <form class="modal" @submit.prevent="submitTrigger">
        <h2>{{ t('buildDetail.triggerTitle') }}</h2>

        <template v-if="parameterDecls.length > 0">
          <div v-for="p in parameterDecls" :key="p.name" class="field">
            <label :for="`param-${p.name}`">
              {{ p.name }}<span v-if="p.required" class="required-mark">*</span>
            </label>
            <input
              v-if="p.type !== 'enum'"
              :id="`param-${p.name}`"
              v-model="triggerParams[p.name]"
              :type="p.type === 'number' ? 'number' : 'text'"
              :name="`param-${p.name}`"
            />
            <select
              v-else
              :id="`param-${p.name}`"
              v-model="triggerParams[p.name]"
              :name="`param-${p.name}`"
            >
              <option v-for="c in p.choices ?? []" :key="c" :value="c">{{ c }}</option>
            </select>
          </div>
        </template>
        <p v-else class="build-muted">{{ t('buildDetail.noParams') }}</p>

        <div class="field">
          <label for="trigger-branch">{{ t('buildDetail.branch') }}</label>
          <input id="trigger-branch" v-model="triggerBranch" name="trigger-branch" />
        </div>

        <div class="field">
          <label for="trigger-commit">{{ t('buildDetail.commit') }}</label>
          <input id="trigger-commit" v-model="triggerCommit" name="trigger-commit" />
        </div>

        <p v-if="triggerError" class="build-error" role="alert">{{ triggerError }}</p>

        <div class="modal-actions">
          <button type="button" class="btn" @click="triggerOpen = false">
            {{ t('buildDetail.cancel') }}
          </button>
          <button type="submit" class="btn btn-primary" :disabled="actionBusy !== null">
            {{ t('buildDetail.trigger') }}
          </button>
        </div>
      </form>
    </div>
  </div>
</template>

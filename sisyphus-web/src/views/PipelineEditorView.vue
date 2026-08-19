<script setup lang="ts">
// 混合式 pipeline 编辑器（票 B4-T8，ADR-0020 变体 C）。
//
// - 加载/保存真实定义：GET `.../pipelines/{pipeline}` 原样读入 model JSON（revision
//   来自顶层字段，非定义内）；PUT 原样提交（剥离 server 独占 revision）。404 → 空
//   定义开始（保存即创建，首存 revision=1）。
// - 保存校验消费 B4-T7 对账校验（`validatePipeline`，单一事实源）：保存时本地校验
//   非空即整组展示 + 字段路径定位、不提交；服务端 422 的 `detail.errors` 同形 path，
//   一并按字段定位展示（与 server 结论一致）。
// - revision 展示 + 并发保存冲突可见：保存响应 revision 与「加载版本 +1」不符即提示
//   （期间被他人保存），本次保存已覆盖，建议重新加载确认。
// - 三页签：任务（左轨道 + 右表单）/ 参数（四类型）/ 环境变量。

import { computed, onMounted, ref, watch } from 'vue'
import { useRoute } from 'vue-router'
import { useI18n } from 'vue-i18n'

import { pipelinesApi } from '@/api/client'
import { ApiError } from '@/api/http'
import { describeSubmitError } from '@/api/errors'
import type { ValidationIssue } from '@/api/types'
import type { Pipeline } from '@/model/pipeline'
import { validatePipeline, type ValidationError } from '@/model/validate'
import { newPipeline, newStage, newJob, toSavePayload, swap } from '@/model/editor'
import PipelineTrack from '@/components/editor/PipelineTrack.vue'
import JobFormPanel from '@/components/editor/JobFormPanel.vue'
import ParametersTab from '@/components/editor/ParametersTab.vue'
import EnvListEditor from '@/components/editor/EnvListEditor.vue'

const route = useRoute()
const { t } = useI18n()

const project = computed(() => String(route.params.name ?? ''))
const pipelineName = computed(() => String(route.params.pipeline ?? ''))

const pipeline = ref<Pipeline | null>(null)
const loadedRevision = ref<number | null>(null)
const loadedOperator = ref('')
const status = ref<'loading' | 'ready' | 'error'>('loading')
const loadError = ref('')

const selection = ref<{ stageIndex: number; jobIndex: number } | null>(null)
const activeTab = ref<'jobs' | 'params' | 'env'>('jobs')

const showErrorPanel = ref(false)
const serverErrors = ref<ValidationIssue[]>([])
const saving = ref(false)
const saveMessage = ref('')
const saveError = ref('')
const conflictMessage = ref('')

/** 本地实时校验（驱动 chip 红边 + 内联定位；错误**面板**仅在保存尝试后展示，
 *  平时不噪声）。与 sisyphus-model `validate` 同源对账（票 B4-T7）。 */
const localErrors = computed<ValidationError[]>(() =>
  pipeline.value ? validatePipeline(pipeline.value) : [],
)

/** 展示用错误：服务端 422 优先，否则保存尝试后的本地错；平时空（编辑中不噪声）。 */
const displayErrors = computed<{ path: string; message: string }[]>(() => {
  if (serverErrors.value.length > 0) return serverErrors.value
  return showErrorPanel.value ? localErrors.value : []
})

const selectedJob = computed(() => {
  if (!pipeline.value || !selection.value) return null
  const stage = pipeline.value.stages[selection.value.stageIndex]
  if (!stage) return null
  return stage.jobs[selection.value.jobIndex] ?? null
})

const jobPath = computed(() => {
  if (!selection.value) return ''
  return `stages[${selection.value.stageIndex}].jobs[${selection.value.jobIndex}]`
})

onMounted(load)
watch([project, pipelineName], load)

async function load(): Promise<void> {
  status.value = 'loading'
  loadError.value = ''
  serverErrors.value = []
  showErrorPanel.value = false
  saveMessage.value = ''
  saveError.value = ''
  conflictMessage.value = ''
  try {
    const resp = await pipelinesApi.getDefinition(project.value, pipelineName.value)
    const def = resp.definition as unknown as Pipeline
    // server 独占 revision：编辑态剥离（展示用顶层 loadedRevision，非定义内残留）。
    def.revision = undefined
    pipeline.value = def
    loadedRevision.value = resp.revision
    loadedOperator.value = resp.operator
    status.value = 'ready'
    selectFirstJob()
  } catch (err) {
    if (err instanceof ApiError && err.status === 404) {
      // pipeline 尚未配置 → 空定义开始（保存即创建）。
      pipeline.value = newPipeline(pipelineName.value)
      loadedRevision.value = null
      loadedOperator.value = ''
      status.value = 'ready'
      selection.value = null
    } else {
      status.value = 'error'
      loadError.value = describeSubmitError(err)
    }
  }
}

function selectFirstJob(): void {
  if (!pipeline.value || pipeline.value.stages.length === 0) {
    selection.value = null
    return
  }
  const firstStage = pipeline.value.stages[0]!
  selection.value = firstStage.jobs.length > 0 ? { stageIndex: 0, jobIndex: 0 } : null
}

// --- 轨道事件：结构变更（增删/重排/选中）就地 mutate 响应式 pipeline ---

function onSelect(si: number, ji: number): void {
  selection.value = { stageIndex: si, jobIndex: ji }
}
function onAddStage(): void {
  if (!pipeline.value) return
  pipeline.value.stages.push(newStage(`stage-${pipeline.value.stages.length + 1}`))
}
function onDeleteStage(si: number): void {
  pipeline.value?.stages.splice(si, 1)
  fixSelection()
}
function onMoveStage(si: number, dir: number): void {
  if (!pipeline.value) return
  swap(pipeline.value.stages, si, si + dir)
  fixSelection()
}
function onAddJob(si: number): void {
  const stage = pipeline.value?.stages[si]
  if (!stage) return
  stage.jobs.push(newJob(`job-${stage.jobs.length + 1}`))
  selection.value = { stageIndex: si, jobIndex: stage.jobs.length - 1 }
}
function onDeleteJob(si: number, ji: number): void {
  pipeline.value?.stages[si]?.jobs.splice(ji, 1)
  fixSelection()
}
function onMoveJob(si: number, ji: number, dir: number): void {
  const stage = pipeline.value?.stages[si]
  if (!stage) return
  const target = ji + dir
  if (target < 0 || target >= stage.jobs.length) return
  swap(stage.jobs, ji, target)
  selection.value = { stageIndex: si, jobIndex: target }
}

/** 删除/重排后修正选中索引：越界则回退到合法项或 null。 */
function fixSelection(): void {
  if (!pipeline.value || !selection.value) {
    selection.value = null
    return
  }
  const { stageIndex, jobIndex } = selection.value
  const stage = pipeline.value.stages[stageIndex]
  if (!stage) {
    selectFirstJob()
    return
  }
  if (jobIndex >= stage.jobs.length) {
    selection.value =
      stage.jobs.length > 0 ? { stageIndex, jobIndex: stage.jobs.length - 1 } : null
  }
}

// Pipeline 级 env（pipeline.env 始终为 []，model `serde default` 永发——无需懒初始化）。
function addPipeEnv(): void {
  if (!pipeline.value) return
  pipeline.value.env.push({ name: '', value: '' })
}
function removePipeEnv(i: number): void {
  pipeline.value?.env.splice(i, 1)
}

// --- 保存 ---

async function save(): Promise<void> {
  if (!pipeline.value) return
  saveError.value = ''
  saveMessage.value = ''
  conflictMessage.value = ''
  serverErrors.value = []
  // 本地校验先行：非空即整组展示 + 字段定位，不提交（与 server 422 同源结论）。
  const errs = validatePipeline(pipeline.value)
  if (errs.length > 0) {
    showErrorPanel.value = true
    return
  }
  showErrorPanel.value = false
  saving.value = true
  try {
    const resp = await pipelinesApi.saveDefinition(
      project.value,
      pipelineName.value,
      toSavePayload(pipeline.value),
    )
    // 并发冲突：响应 revision 应为「加载版本 + 1」；不符即期间被他人保存。
    const expected = (loadedRevision.value ?? 0) + 1
    if (resp.revision !== expected) {
      conflictMessage.value = t('editor.conflict', {
        loaded: loadedRevision.value ?? 0,
        prev: resp.revision - 1,
        revision: resp.revision,
      })
    } else {
      saveMessage.value = t('editor.saved', {
        revision: resp.revision,
        operator: resp.operator,
      })
    }
    loadedRevision.value = resp.revision
    loadedOperator.value = resp.operator
  } catch (err) {
    if (err instanceof ApiError) {
      if (err.status === 422 && err.code === 'VALIDATION_FAILED') {
        serverErrors.value = err.validationIssues
      } else if (err.status === 403) {
        saveError.value = t('editor.saveAdminOnly')
      } else if (err.status === 404) {
        saveError.value = t('editor.projectNotFound')
      } else {
        saveError.value = describeSubmitError(err)
      }
    } else {
      saveError.value = t('errors.generic')
    }
  } finally {
    saving.value = false
  }
}
</script>

<template>
  <div v-if="status === 'loading'" class="editor-page">
    <p class="form-hint">{{ t('editor.loading') }}</p>
  </div>

  <div v-else-if="status === 'error'" class="editor-page">
    <p class="form-error" role="alert">{{ loadError || t('editor.loadError') }}</p>
  </div>

  <div v-else-if="pipeline" class="editor-page">
    <header class="editor-header">
      <h1 class="page-title">{{ pipelineName }}</h1>
      <div class="editor-revision">
        <span class="editor-rev-label">{{ t('editor.revision') }}</span>
        <span class="editor-rev-value">{{ loadedRevision ?? t('editor.revisionUnknown') }}</span>
        <span v-if="loadedOperator" class="editor-rev-op">
          {{ t('editor.operator') }} {{ loadedOperator }}
        </span>
      </div>
      <div class="editor-header-actions">
        <button type="button" class="btn" name="editor-reload" :disabled="saving" @click="load">
          {{ t('editor.reload') }}
        </button>
        <button
          type="button"
          class="btn btn-primary"
          name="editor-save"
          :disabled="saving"
          @click="save"
        >
          {{ saving ? t('editor.saving') : t('editor.save') }}
        </button>
      </div>
    </header>

    <p v-if="loadedRevision === null" class="form-hint">{{ t('editor.notFound') }}</p>
    <p v-if="saveMessage" class="editor-save-ok" role="status">{{ saveMessage }}</p>
    <p v-if="conflictMessage" class="editor-conflict" role="alert">{{ conflictMessage }}</p>
    <p v-if="saveError" class="form-error" role="alert">{{ saveError }}</p>

    <!-- 整组校验错误面板（本地 + 服务端，含字段路径定位）。 -->
    <section v-if="displayErrors.length > 0" class="editor-errors" role="alert">
      <h3>
        {{ serverErrors.length > 0
          ? t('editor.serverErrorsTitle', { count: displayErrors.length })
          : t('editor.errorsTitle', { count: displayErrors.length }) }}
      </h3>
      <ul>
        <li v-for="(e, i) in displayErrors" :key="i">
          <code class="err-path">{{ e.path }}</code> {{ e.message }}
        </li>
      </ul>
    </section>

    <!-- 页签 -->
    <nav class="editor-tabs" role="tablist">
      <button
        type="button"
        role="tab"
        name="tab-jobs"
        :aria-selected="activeTab === 'jobs'"
        :class="{ active: activeTab === 'jobs' }"
        @click="activeTab = 'jobs'"
      >{{ t('editor.tabJobs') }}</button>
      <button
        type="button"
        role="tab"
        name="tab-params"
        :aria-selected="activeTab === 'params'"
        :class="{ active: activeTab === 'params' }"
        @click="activeTab = 'params'"
      >{{ t('editor.tabParams') }}</button>
      <button
        type="button"
        role="tab"
        name="tab-env"
        :aria-selected="activeTab === 'env'"
        :class="{ active: activeTab === 'env' }"
        @click="activeTab = 'env'"
      >{{ t('editor.tabEnv') }}</button>
    </nav>

    <!-- 任务页签：左轨道 + 右表单 -->
    <div v-show="activeTab === 'jobs'" class="editor-jobs-pane">
      <PipelineTrack
        :pipeline="pipeline"
        :selection="selection"
        :errors="displayErrors"
        @select="onSelect"
        @add-stage="onAddStage"
        @delete-stage="onDeleteStage"
        @move-stage="onMoveStage"
        @add-job="onAddJob"
        @delete-job="onDeleteJob"
        @move-job="onMoveJob"
      />
      <JobFormPanel :job="selectedJob" :job-path="jobPath" :errors="displayErrors" />
    </div>

    <!-- 参数页签 -->
    <section v-show="activeTab === 'params'" class="editor-tab-pane">
      <ParametersTab :parameters="pipeline.parameters" :errors="displayErrors" />
    </section>

    <!-- 环境变量页签 -->
    <section v-show="activeTab === 'env'" class="editor-tab-pane">
      <h2>{{ t('editor.envTabTitle') }}</h2>
      <p class="form-hint">{{ t('editor.envTabHint') }}</p>
      <EnvListEditor
        :env="pipeline.env"
        name-attr="pipe-env"
        :add-label="t('editor.envTabAdd')"
        :remove-label="t('editor.envRemove')"
        :empty-label="t('editor.envTabEmpty')"
        :name-label="t('editor.envName')"
        :value-label="t('editor.envValue')"
        @add="addPipeEnv"
        @remove="removePipeEnv"
      />
    </section>
  </div>
</template>

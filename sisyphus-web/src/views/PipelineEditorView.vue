<script setup lang="ts">
// 混合式 pipeline 编辑器（票 B4-T8，ADR-0020 变体 C；票 #109 定稿设计语言统一）。
//
// - 加载/保存真实定义：GET `.../pipelines/{pipeline}` 原样读入 model JSON（revision
//   来自顶层字段，非定义内）；普通 PUT 兼容 upsert。显式 create=1 才建立
//   内存草稿，首存用 If-None-Match: * 原子创建，412 永不降级为覆盖。
// - 保存校验消费 B4-T7 对账校验（`validatePipeline`，单一事实源）：保存时本地校验
//   非空即整组展示 + 字段路径定位、不提交；服务端 422 的 `detail.errors` 同形 path，
//   一并按字段定位展示（与 server 结论一致）。
// - revision 展示 + 并发保存冲突可见：保存响应 revision 与「加载版本 +1」不符即弹
//   冲突弹窗（期间被他人保存），本次保存已覆盖，建议重新加载确认。
// - 三页签：任务（左轨道 + 右表单）/ 参数（四类型）/ 环境变量。
// 视觉（票 #109）：以定稿三主页面为推导源，复用共享组件类——breadcrumb 面包屑、
// badge 胶囊徽章、btn-outline 描边动作；事实态纪律——首载骨架屏、整页报错可重试。
// 混合交互逻辑（轨道派生导航 + 表单 + 页签）不变，只换视觉。
// #96: 迁移 Naive UI——页签改 NTabs、保存成功改 NMessage toast、错误面板改
// NAlert、并发冲突改 NModal、保存/重载改 NButton，交互不变。

import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { onBeforeRouteLeave, onBeforeRouteUpdate, useRoute, useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'
import { NAlert, NButton, NInput, NModal, NSkeleton, NTabs, NTabPane, useMessage } from 'naive-ui'

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
import { restoreScrollWhenReady } from '@/utils/returnSource'

const route = useRoute()
const router = useRouter()
const { t } = useI18n()
const message = useMessage()

const project = computed(() => String(route.params.name ?? ''))
const pipelineName = computed(() => String(route.params.pipeline ?? ''))

function safeReturnPath(): { path: string; valid: boolean } {
  const fallback = { path: router.resolve({ name: 'pipelines' }).fullPath, valid: false }
  const raw = typeof route.query.from === 'string' ? route.query.from : ''
  if (!raw.startsWith('/') || raw.startsWith('//')) return fallback
  const resolved = router.resolve(raw)
  const routeName = resolved.name
  if (
    resolved.matched.length === 0 ||
    routeName === 'not-found' ||
    routeName === 'login' ||
    routeName === 'setup' ||
    routeName === 'pipeline-edit'
  ) {
    return fallback
  }
  // 无论来源来自入口还是深链，返回都不能重开已完成的创建对话框。
  const query = { ...resolved.query }
  delete query.create
  return { path: router.resolve({ path: resolved.path, query, hash: resolved.hash }).fullPath, valid: true }
}

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
const saveError = ref('')
const conflictMessage = ref('')
const isNewDraft = ref(false)
const existingDuringCreate = ref(false)
const cleanSnapshot = ref('')
const showLeaveConfirm = ref(false)
const pendingLeavePath = ref<string | null>(null)
const pendingSourceReturn = ref(false)
const createCollision = ref(false)
const collisionName = ref('')
const displayName = computed(() => isNewDraft.value ? pipeline.value?.name ?? pipelineName.value : pipelineName.value)
let completingCreation = false
let loadSequence = 0

const isDirty = computed(() =>
  pipeline.value != null && JSON.stringify(toSavePayload(pipeline.value)) !== cleanSnapshot.value,
)

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
watch(() => route.query.create, (mode, previous) => {
  if (mode === '1' || (previous === '1' && isNewDraft.value)) void load()
})

async function load(): Promise<void> {
  if (project.value === '' || pipelineName.value === '') return
  const sequence = ++loadSequence
  const projectAtLoad = project.value
  const pipelineAtLoad = pipelineName.value
  status.value = 'loading'
  loadError.value = ''
  serverErrors.value = []
  showErrorPanel.value = false
  saveError.value = ''
  conflictMessage.value = ''
  existingDuringCreate.value = false
  const requestedCreate = route.query.create === '1'
  try {
    const resp = await pipelinesApi.getDefinition(projectAtLoad, pipelineAtLoad)
    if (sequence !== loadSequence) return
    if (requestedCreate) {
      // 新建深链若在加载时发现资源已存在，绝不进入可覆盖编辑器。
      pipeline.value = null
      loadedRevision.value = resp.revision
      existingDuringCreate.value = true
      isNewDraft.value = false
      cleanSnapshot.value = ''
      status.value = 'ready'
      return
    }
    const def = resp.definition as unknown as Pipeline
    // server 独占 revision：编辑态剥离（展示用顶层 loadedRevision，非定义内残留）。
    def.revision = undefined
    pipeline.value = def
    loadedRevision.value = resp.revision
    loadedOperator.value = resp.operator
    isNewDraft.value = false
    cleanSnapshot.value = JSON.stringify(toSavePayload(def))
    status.value = 'ready'
    selectFirstJob()
  } catch (err) {
    if (sequence !== loadSequence) return
    if (err instanceof ApiError && err.status === 404) {
      if (requestedCreate) {
        // 明确新建态：只在前端内存建立草稿，不发任何预创建写请求。
        pipeline.value = newPipeline(pipelineAtLoad)
        loadedRevision.value = null
        loadedOperator.value = ''
        isNewDraft.value = true
        cleanSnapshot.value = JSON.stringify(toSavePayload(pipeline.value))
        status.value = 'ready'
        selection.value = null
      } else {
        status.value = 'error'
        loadError.value = t('editor.pipelineNotFound')
      }
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
  if (!pipeline.value || saving.value) return
  saveError.value = ''
  conflictMessage.value = ''
  serverErrors.value = []
  const targetName = isNewDraft.value ? pipeline.value.name.trim() : pipelineName.value
  if (isNewDraft.value && (targetName === '' || targetName === '.' || targetName === '..' || /[\\/\u0000-\u001f\u007f]/u.test(targetName))) {
    saveError.value = t(targetName === '' ? 'plines.createNameRequired' : 'plines.createNameInvalid')
    return
  }
  // 本地校验先行：非空即整组展示 + 字段定位，不提交（与 server 422 同源结论）。
  const errs = validatePipeline(pipeline.value)
  if (errs.length > 0) {
    showErrorPanel.value = true
    return
  }
  showErrorPanel.value = false
  saving.value = true
  try {
    // 快照必须独立于响应式定义，防止请求期间的编辑悄悄推进已保存基线。
    const payload = JSON.parse(JSON.stringify(toSavePayload(pipeline.value))) as Pipeline
    const wasCreating = isNewDraft.value
    if (wasCreating) payload.name = targetName
    const resp = wasCreating
      ? await pipelinesApi.createDefinition(project.value, targetName, payload)
      : await pipelinesApi.saveDefinition(project.value, pipelineName.value, payload)
    // 并发冲突：响应 revision 应为「加载版本 + 1」；不符即期间被他人保存。
    const expected = (loadedRevision.value ?? 0) + 1
    if (resp.revision !== expected) {
      conflictMessage.value = t('editor.conflict', {
        loaded: loadedRevision.value ?? 0,
        prev: resp.revision - 1,
        revision: resp.revision,
      })
    } else {
      message.success(
        t('editor.saved', {
          revision: resp.revision,
          operator: resp.operator,
        }),
      )
    }
    loadedRevision.value = resp.revision
    loadedOperator.value = resp.operator
    cleanSnapshot.value = JSON.stringify(payload)
    if (wasCreating) {
      isNewDraft.value = false
      createCollision.value = false
      const query = { ...route.query }
      delete query.create
      completingCreation = true
      try {
        await router.replace({ name: 'pipeline-edit', params: { name: project.value, pipeline: targetName }, query })
      } finally {
        completingCreation = false
      }
    }
  } catch (err) {
    if (err instanceof ApiError) {
      if (err.status === 422 && err.code === 'VALIDATION_FAILED') {
        serverErrors.value = err.validationIssues
      } else if (err.status === 403) {
        saveError.value = t('editor.saveAdminOnly')
      } else if (err.status === 404) {
        saveError.value = t('editor.projectNotFound')
      } else if (err.status === 412) {
        createCollision.value = true
        collisionName.value = targetName
        saveError.value = t('editor.createConflict')
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

function beforeUnload(event: BeforeUnloadEvent): void {
  if (!isNewDraft.value || !isDirty.value) return
  event.preventDefault()
  event.returnValue = ''
}

onMounted(() => window.addEventListener('beforeunload', beforeUnload))
onBeforeUnmount(() => {
  ++loadSequence
  window.removeEventListener('beforeunload', beforeUnload)
})

onBeforeRouteLeave((to) => {
  if (saving.value && !completingCreation) return false
  if (!isNewDraft.value || !isDirty.value) return true
  pendingLeavePath.value = to.fullPath
  pendingSourceReturn.value = false
  showLeaveConfirm.value = true
  return false
})

onBeforeRouteUpdate((to) => {
  if (saving.value && !completingCreation) return false
  if (!isNewDraft.value || !isDirty.value) return true
  if (to.params.name === route.params.name && to.params.pipeline === route.params.pipeline && to.query.create === route.query.create) return true
  pendingLeavePath.value = to.fullPath
  pendingSourceReturn.value = false
  showLeaveConfirm.value = true
  return false
})

async function actualReturnToSource(): Promise<void> {
  const rawScroll = typeof route.query.fromScroll === 'string' ? route.query.fromScroll : ''
  const scroll = Number(rawScroll)
  const destination = safeReturnPath()
  await router.replace(destination.path)
  if (destination.valid) {
    restoreScrollWhenReady(
      rawScroll !== '' && Number.isFinite(scroll) && scroll >= 0 ? scroll : 0,
      router,
    )
  }
}

async function requestReturnToSource(): Promise<void> {
  if (isNewDraft.value && isDirty.value) {
    pendingLeavePath.value = null
    pendingSourceReturn.value = true
    showLeaveConfirm.value = true
    return
  }
  await actualReturnToSource()
}

function continueEditing(): void {
  showLeaveConfirm.value = false
  pendingLeavePath.value = null
  pendingSourceReturn.value = false
}

async function discardAndLeave(): Promise<void> {
  // 将当前快照标作 clean，让同一次编程式导航通过路由守卫；页面随即卸载。
  cleanSnapshot.value = pipeline.value == null ? '' : JSON.stringify(toSavePayload(pipeline.value))
  showLeaveConfirm.value = false
  if (pendingSourceReturn.value) {
    pendingSourceReturn.value = false
    await actualReturnToSource()
    return
  }
  const target = pendingLeavePath.value
  pendingLeavePath.value = null
  if (target) await router.push(target)
}

async function openExistingPipeline(): Promise<void> {
  if (isNewDraft.value && isDirty.value) {
    pendingLeavePath.value = router.resolve({ name: 'pipeline-edit', params: { name: project.value, pipeline: collisionName.value }, query: { from: route.query.from, fromScroll: route.query.fromScroll } }).fullPath
    pendingSourceReturn.value = false
    showLeaveConfirm.value = true
    return
  }
  const query = { ...route.query }
  delete query.create
  await router.replace({ path: route.path, query })
  await load()
}

async function changePipelineName(): Promise<void> {
  const destination = safeReturnPath()
  const resolved = router.resolve(destination.path)
  await router.replace({ path: resolved.path, query: { ...resolved.query, create: '1' } })
}

/** 冲突弹窗显隐：消息非空即弹；关闭/取消清消息（load 亦会清）。 */
const showConflict = computed({
  get: () => conflictMessage.value !== '',
  set: (show: boolean) => {
    if (!show) conflictMessage.value = ''
  },
})
</script>

<template>
  <!-- 首载骨架屏（事实态纪律，同项目详情 #108）。 -->
  <div v-if="status === 'loading'" class="editor-page" data-testid="editor-skeleton">
    <n-skeleton text :repeat="1" height="32px" class="editor-skeleton-row" />
    <n-skeleton text :repeat="2" height="56px" class="editor-skeleton-row" />
    <n-skeleton text :repeat="4" height="72px" class="editor-skeleton-row" />
  </div>

  <!-- 整页报错可重试（事实态纪律）。 -->
  <div v-else-if="status === 'error'" class="editor-page">
    <n-alert type="error" :title="loadError || t('editor.loadError')" role="alert">
      <button type="button" class="btn-outline blue" data-testid="editor-retry" @click="load">
        {{ t('plines.retry') }}
      </button>
    </n-alert>
  </div>

  <div v-else-if="existingDuringCreate" class="editor-page" data-testid="create-name-conflict">
    <nav class="breadcrumb" aria-label="Breadcrumb">
      <router-link :to="{ name: 'projects' }">{{ t('routes.projects') }}</router-link>
      <span class="breadcrumb-sep">/</span>
      <span>{{ project }}</span>
      <span class="breadcrumb-sep">/</span>
      <span class="breadcrumb-current">{{ displayName }}</span>
    </nav>
    <n-alert type="warning" :title="t('editor.createExistsTitle')" role="alert">
      {{ t('editor.createExists', { project, pipeline: pipelineName }) }}
      <div class="modal-actions">
        <n-button data-testid="create-change-name" @click="changePipelineName">
          {{ t('editor.changeName') }}
        </n-button>
        <n-button type="primary" data-testid="create-open-existing" @click="openExistingPipeline">
          {{ t('editor.openExisting') }}
        </n-button>
      </div>
    </n-alert>
  </div>

  <div v-else-if="pipeline && project && pipelineName" class="editor-page">
    <nav class="breadcrumb" aria-label="Breadcrumb">
      <router-link :to="{ name: 'projects' }">{{ t('routes.projects') }}</router-link>
      <span class="breadcrumb-sep">/</span>
      <router-link :to="{ name: 'project-detail', params: { name: project } }">
        {{ project }}
      </router-link>
      <span class="breadcrumb-sep">/</span>
      <span class="breadcrumb-current">{{ displayName }}</span>
    </nav>

    <header class="editor-header">
      <n-button data-testid="editor-back" :disabled="saving" @click="requestReturnToSource">
        {{ t('editor.back') }}
      </n-button>
      <h1 class="page-title">{{ displayName }}</h1>
      <div class="editor-revision">
        <span v-if="isNewDraft" class="badge warning" data-testid="editor-new-badge">
          {{ t('editor.newDraft') }}
        </span>
        <span class="badge neutral">
          {{ t('editor.revision') }} <span class="editor-rev-value">{{ loadedRevision ?? t('editor.revisionUnknown') }}</span>
        </span>
        <span v-if="loadedOperator" class="editor-rev-op">
          {{ t('editor.operator') }} {{ loadedOperator }}
        </span>
      </div>
      <div class="editor-header-actions">
        <n-button v-if="!isNewDraft" name="editor-reload" :disabled="saving" @click="load">
          {{ t('editor.reload') }}
        </n-button>
        <n-button
          type="primary"
          name="editor-save"
          :loading="saving"
          :disabled="saving"
          @click="save"
        >
          {{ saving ? t('editor.saving') : (isNewDraft ? t('editor.saveAndCreate') : t('editor.save')) }}
        </n-button>
      </div>
    </header>

    <p v-if="isNewDraft" class="form-hint" data-testid="editor-unsaved-hint">
      {{ t('editor.newDraftHint', { project, pipeline: displayName }) }}
    </p>
    <label v-if="isNewDraft" class="editor-draft-name">
      {{ t('projects.newPipelineName') }}
      <n-input v-model:value="pipeline.name" :disabled="saving" :input-props="{ name: 'editor-draft-name', autocomplete: 'off' }" />
    </label>
    <n-alert v-if="saveError" type="error" :title="saveError" role="alert" />
    <n-button v-if="isNewDraft && createCollision" data-testid="draft-open-existing" @click="openExistingPipeline">
      {{ t('editor.openExisting') }}
    </n-button>

    <!-- 整组校验错误面板（本地 + 服务端，含字段路径定位）。 -->
    <n-alert
      v-if="displayErrors.length > 0"
      type="error"
      class="editor-errors"
      role="alert"
      :title="serverErrors.length > 0
        ? t('editor.serverErrorsTitle', { count: displayErrors.length })
        : t('editor.errorsTitle', { count: displayErrors.length })"
    >
      <ul class="editor-errors-list">
        <li v-for="(e, i) in displayErrors" :key="i">
          <code class="err-path">{{ e.path }}</code> {{ e.message }}
        </li>
      </ul>
    </n-alert>

    <!-- 页签（display-directive=show：同旧 v-show，三页签常驻不卸载）。 -->
    <n-tabs
      class="editor-tabs"
      type="line"
      display-directive="show"
      :inert="saving || undefined"
      :aria-busy="saving"
      :value="activeTab"
      @update:value="activeTab = $event as 'jobs' | 'params' | 'env'"
    >
      <!-- 任务页签：左轨道 + 右表单 -->
      <n-tab-pane name="jobs" :tab="t('editor.tabJobs')">
        <div class="editor-jobs-pane">
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
      </n-tab-pane>

      <!-- 参数页签 -->
      <n-tab-pane name="params" :tab="t('editor.tabParams')">
        <ParametersTab :parameters="pipeline.parameters" :errors="displayErrors" />
      </n-tab-pane>

      <!-- 环境变量页签 -->
      <n-tab-pane name="env" :tab="t('editor.tabEnv')">
        <section class="editor-tab-pane">
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
      </n-tab-pane>
    </n-tabs>

    <!-- 并发保存冲突弹窗：本次保存已覆盖他人版本，建议重新加载确认。
         禁 mask/esc 误关（冲突信息仅此一处，误触即丢）。 -->
    <n-modal
      v-model:show="showConflict"
      preset="card"
      :title="t('editor.conflictTitle')"
      style="width: 480px"
      :bordered="false"
      :mask-closable="false"
      :close-on-esc="false"
      role="alert"
    >
      <p class="editor-conflict-text">{{ conflictMessage }}</p>
      <div class="modal-actions">
        <n-button @click="showConflict = false">{{ t('common.cancel') }}</n-button>
        <n-button type="primary" name="conflict-reload" :disabled="saving" @click="load">
          {{ t('editor.reload') }}
        </n-button>
      </div>
    </n-modal>

    <n-modal
      v-model:show="showLeaveConfirm"
      preset="card"
      :title="t('editor.unsavedTitle')"
      style="width: 480px"
      :bordered="false"
      :mask-closable="false"
      :close-on-esc="false"
    >
      <p>{{ t('editor.unsavedMessage') }}</p>
      <div class="modal-actions">
        <n-button data-testid="unsaved-continue" @click="continueEditing">
          {{ t('editor.continueEditing') }}
        </n-button>
        <n-button type="error" data-testid="unsaved-discard" @click="discardAndLeave">
          {{ t('editor.discardDraft') }}
        </n-button>
      </div>
    </n-modal>
  </div>
</template>

<style scoped>
.editor-page {
  display: flex;
  flex-direction: column;
  gap: 14px;
}

.editor-skeleton-row {
  display: block;
}

.editor-header {
  display: flex;
  align-items: center;
  gap: 16px;
  flex-wrap: wrap;
}

.editor-header h1 {
  margin: 0;
  font-size: 20px;
}

.editor-revision {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 13px;
  color: var(--sisy-color-text-secondary);
}

/* badge 内联 revision 数字保持原色深一级（胶囊底上是正文反差）。 */
.editor-rev-value {
  font-weight: 600;
  color: var(--sisy-color-text);
}

.editor-rev-op {
  color: var(--sisy-color-text-secondary);
}

.editor-header-actions {
  display: flex;
  gap: 8px;
  margin-left: auto;
}

.editor-errors-list {
  margin: 0;
  padding-left: 18px;
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 13px;
}

.err-path {
  font-family: ui-monospace, 'Cascadia Code', Consolas, monospace;
  font-size: 12px;
  color: var(--sisy-color-text-secondary);
  border: 1px solid var(--sisy-color-border);
  border-radius: 3px;
  padding: 0 4px;
  margin-right: 4px;
}

/* 任务页签：左轨道 + 右表单并排。 */
.editor-jobs-pane {
  display: flex;
  gap: 18px;
  align-items: flex-start;
  padding-top: 12px;
}

/* 平板档收窄：轨道/表单并排改纵排（轨道全宽在上），表单不被挤死
 * （响应式承诺不回退，spec #100 story 24）。 */
@media (max-width: 1024px) {
  .editor-jobs-pane {
    flex-direction: column;
  }

  .editor-jobs-pane :deep(.editor-track) {
    flex: none;
    width: 100%;
  }

  .editor-jobs-pane :deep(.job-form) {
    width: 100%;
  }
}

.editor-tab-pane {
  display: flex;
  flex-direction: column;
  gap: 8px;
  align-items: flex-start;
  padding-top: 12px;
}

.editor-tab-pane h2 {
  margin: 0;
}

.editor-conflict-text {
  margin: 0 0 4px;
  font-size: 13px;
  line-height: 1.6;
}

.modal-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 8px;
}
</style>

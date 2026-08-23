<script setup lang="ts">
// 任务属性表单（票 B4-T8，ADR-0020 变体 C 右侧）：选中任务的全属性显式字段——
// 名称 / 标签 / 执行环境（host|container+image）/ when / env / 机密引用 /
// allow_failure / retry_count / timeout_minutes / 产物上传 / 产物下载 / 缓存 / 步骤。
//
// 反应式约定：`job` 是父持有的响应式任务对象（`pipeline.stages[si].jobs[ji]`），
// 就地 mutate（v-model 改属性 + 数组 push/splice）——不替换 prop 绑定本身。
// 校验错误按 `jobPath` 前缀定位到字段（本地校验与服务端 422 同形 path），
// 经 NFormItem validation-status + feedback 插槽就地红显（输入框同步染红）。
// #96: 迁移 Naive UI——输入改 NInput、解释器改 NSelect、开关改 NSwitch、
// 数字改 NInputNumber、增删/重排按钮改 NButton，交互不变。

import { useI18n } from 'vue-i18n'
import { NButton, NForm, NFormItem, NInput, NInputNumber, NRadio, NRadioGroup, NSelect, NSwitch } from 'naive-ui'

import type { Job, Step, Shell, CacheSpec } from '@/model/pipeline'
import {
  errorsForField,
  linesToText,
  textToLines,
  swap,
  newShellStep,
  newCheckoutStep,
} from '@/model/editor'
import EnvListEditor from './EnvListEditor.vue'

const props = defineProps<{
  job: Job | null
  /** 该任务的字段路径前缀（`stages[si].jobs[ji]`），错误定位用。 */
  jobPath: string
  /** 展示用错误清单（本地校验或服务端 422，同形 {path,message}）。 */
  errors: { path: string; message: string }[]
}>()

const { t } = useI18n()

const SHELLS: Shell[] = ['sh', 'bash', 'pwsh', 'cmd']

const shellOptions = [
  { label: t('editor.stepShellDefault'), value: '' },
  ...SHELLS.map((sh) => ({ label: sh, value: sh })),
]

function errs(prefix: string): { path: string; message: string }[] {
  return errorsForField(props.errors, prefix)
}

/** NFormItem 校验态：有错 → 'error'（输入框红边 + feedback 区红字），否则不定。 */
function statusOf(prefix: string): 'error' | undefined {
  return errs(prefix).length > 0 ? 'error' : undefined
}

// 执行环境 -----------------------------------------------------------------
function isContainer(job: Job): boolean {
  return job.exec_env?.type === 'container'
}
function setExecType(job: Job, ty: 'host' | 'container'): void {
  if (ty === 'host') {
    job.exec_env = { type: 'host' }
  } else {
    const image = job.exec_env?.type === 'container' ? job.exec_env.config.image : ''
    job.exec_env = { type: 'container', config: { image } }
  }
}
// 容器 image（narrow exec_env 到 container 变体——host 变体无 config，模板内
// v-if 守卫运行期，但 TS 不跨元素窄化，故经 helper 窄化）。
function containerImage(job: Job): string {
  return job.exec_env?.type === 'container' ? job.exec_env.config.image : ''
}
function setContainerImage(job: Job, v: string): void {
  if (job.exec_env?.type === 'container') job.exec_env.config.image = v
}

// env ------------------------------------------------------------------
// 懒初始化在 add 期（非渲染期）：job.env 缺省 undefined，首次 add 时 `??=` 钉成
// 响应式数组再 push——保 ADR-0009「定义原样往返」（空 load→save 不给任务添
// `env: []` 噪声）。行内字段 v-model 就地改行对象属性（数组已是 job.env 真数组）。
function addJobEnv(job: Job): void {
  ;(job.env ??= []).push({ name: '', value: '' })
}
function removeJobEnv(job: Job, i: number): void {
  job.env?.splice(i, 1)
}

// when（空 = undefined：空串会被 when 校验判 when_syntax，故清空须归 undefined）
function whenText(job: Job): string {
  return job.when ?? ''
}
function setWhen(job: Job, v: string): void {
  job.when = v === '' ? undefined : v
}

// 标签 -----------------------------------------------------------------
function labelsText(job: Job): string {
  return linesToText(job.labels ?? [])
}
function setLabels(job: Job, text: string): void {
  const next = textToLines(text)
  job.labels = next.length > 0 ? next : undefined
}

// 机密引用 -----------------------------------------------------------------
function addSecret(job: Job): void {
  ;(job.secrets ??= []).push('')
}
function setSecret(job: Job, i: number, v: string): void {
  if (job.secrets) job.secrets[i] = v
}
function removeSecret(job: Job, i: number): void {
  job.secrets?.splice(i, 1)
}

// retry / timeout（NInputNumber null = 不设——model skip_if_zero；负数归不设）
function setRetry(job: Job, v: number | null): void {
  job.retry_count = v == null || v < 0 ? undefined : Math.floor(v)
}
function setTimeoutMin(job: Job, v: number | null): void {
  job.timeout_minutes = v == null || v < 0 ? undefined : Math.floor(v)
}

// 产物上传 -----------------------------------------------------------------
function addUpload(job: Job): void {
  ;(job.artifact_uploads ??= []).push({ name: '', path: '' })
}
function removeUpload(job: Job, i: number): void {
  job.artifact_uploads?.splice(i, 1)
}
function uploadErrors(i: number): { path: string; message: string }[] {
  return errs(`${props.jobPath}.artifact_uploads[${i}]`)
}

// 产物下载 -----------------------------------------------------------------
function addDownload(job: Job): void {
  ;(job.artifact_downloads ??= []).push({ job: '', name: '', path: '' })
}
function removeDownload(job: Job, i: number): void {
  job.artifact_downloads?.splice(i, 1)
}

// 缓存 -----------------------------------------------------------------
function addCache(job: Job): void {
  ;(job.caches ??= []).push({ key: '', paths: [], files: [] })
}
function removeCache(job: Job, i: number): void {
  job.caches?.splice(i, 1)
}
function pathsText(c: CacheSpec): string {
  return linesToText(c.paths)
}
function setPaths(c: CacheSpec, text: string): void {
  c.paths = textToLines(text)
}
function filesText(c: CacheSpec): string {
  return linesToText(c.files)
}
function setFiles(c: CacheSpec, text: string): void {
  c.files = textToLines(text)
}

// 步骤 -----------------------------------------------------------------
function addShell(job: Job): void {
  job.steps.push(newShellStep())
}
function addCheckout(job: Job): void {
  job.steps.push(newCheckoutStep())
}
function removeStep(job: Job, i: number): void {
  job.steps.splice(i, 1)
}
function moveStep(job: Job, i: number, dir: number): void {
  swap(job.steps, i, i + dir)
}
function stepWhen(step: Step): string {
  return step.config.when ?? ''
}
function setStepWhen(step: Step, v: string): void {
  step.config.when = v === '' ? undefined : v
}
// shell 配置（narrow by step.type === 'shell'）
function shellCommand(step: Step): string {
  return step.type === 'shell' ? step.config.command : ''
}
function setShellCommand(step: Step, v: string): void {
  if (step.type === 'shell') step.config.command = v
}
function shellShell(step: Step): Shell | null {
  return step.type === 'shell' ? step.config.shell : null
}
function setShellShell(step: Step, v: Shell | null): void {
  if (step.type === 'shell') step.config.shell = v
}
/** NSelect 值（'' = 默认解释器 → null）。 */
function onShellSelect(step: Step, v: string | number | null): void {
  setShellShell(step, v === '' ? null : (v as Shell))
}
// checkout 配置
function checkoutSubmodules(step: Step): boolean {
  return step.type === 'checkout' ? (step.config.submodules ?? false) : false
}
function setCheckoutSubmodules(step: Step, v: boolean): void {
  if (step.type === 'checkout') step.config.submodules = v
}
</script>

<template>
  <div v-if="!job" class="form-hint editor-select-prompt">{{ t('editor.selectPrompt') }}</div>

  <n-form v-else class="job-form" label-placement="top" @submit.prevent>
    <!-- 名称 + 标签 -->
    <n-form-item :label="t('editor.jobName')" :show-require-mark="true">
      <n-input v-model:value="job.name" :input-props="{ name: 'job-name' }" />
    </n-form-item>
    <n-form-item :label="t('editor.labels')">
      <n-input
        type="textarea"
        :rows="3"
        :value="labelsText(job)"
        :input-props="{ name: 'job-labels' }"
        :placeholder="t('editor.labelsPlaceholder')"
        @update:value="setLabels(job, $event)"
      />
    </n-form-item>
    <p class="form-hint">{{ t('editor.labelsHint') }}</p>

    <!-- 执行环境 -->
    <fieldset class="form-fieldset">
      <legend>{{ t('editor.execEnv') }}</legend>
      <n-radio-group
        name="job-exec-env"
        :value="isContainer(job) ? 'container' : 'host'"
        @update:value="setExecType(job, $event as 'host' | 'container')"
      >
        <n-radio value="host">{{ t('editor.execHost') }}</n-radio>
        <n-radio value="container">{{ t('editor.execContainer') }}</n-radio>
      </n-radio-group>
      <n-form-item
        v-if="isContainer(job)"
        :label="t('editor.containerImage')"
        :validation-status="statusOf(`${jobPath}.exec_env.image`)"
      >
        <n-input
          :value="containerImage(job)"
          :input-props="{ name: 'job-container-image' }"
          :placeholder="t('editor.containerImagePlaceholder')"
          @update:value="setContainerImage(job, $event)"
        />
        <template v-if="errs(`${jobPath}.exec_env.image`).length" #feedback>
          <ul class="field-errors" role="alert">
            <li v-for="(e, ei) in errs(`${jobPath}.exec_env.image`)" :key="ei">
              <code class="err-path">{{ e.path }}</code> {{ e.message }}
            </li>
          </ul>
        </template>
      </n-form-item>
    </fieldset>

    <!-- when -->
    <n-form-item
      :label="t('editor.when')"
      :validation-status="statusOf(`${jobPath}.when`)"
    >
      <n-input
        :value="whenText(job)"
        :input-props="{ name: 'job-when' }"
        @update:value="setWhen(job, $event)"
      />
      <template v-if="errs(`${jobPath}.when`).length" #feedback>
        <ul class="field-errors" role="alert">
          <li v-for="(e, ei) in errs(`${jobPath}.when`)" :key="ei">
            <code class="err-path">{{ e.path }}</code> {{ e.message }}
          </li>
        </ul>
      </template>
    </n-form-item>
    <p class="form-hint">{{ t('editor.whenHint') }}</p>

    <!-- env -->
    <fieldset class="form-fieldset">
      <legend>{{ t('editor.env') }}</legend>
      <EnvListEditor
        :env="job.env ?? []"
        name-attr="job-env"
        :add-label="t('editor.envAdd')"
        :remove-label="t('editor.envRemove')"
        :empty-label="t('editor.envEmpty')"
        :name-label="t('editor.envName')"
        :value-label="t('editor.envValue')"
        @add="addJobEnv(job)"
        @remove="removeJobEnv(job, $event)"
      />
      <!-- env 键 × 机密名冲突（R7）：path 按 env 键名定位（跨行），列于清单下。 -->
      <ul v-if="errs(`${jobPath}.env`).length" class="field-errors" role="alert">
        <li v-for="(e, ei) in errs(`${jobPath}.env`)" :key="ei">
          <code class="err-path">{{ e.path }}</code> {{ e.message }}
        </li>
      </ul>
    </fieldset>

    <!-- 机密引用 -->
    <fieldset class="form-fieldset">
      <legend>{{ t('editor.secrets') }}</legend>
      <p class="form-hint">{{ t('editor.secretsHint') }}</p>
      <p v-if="(job.secrets ?? []).length === 0" class="form-hint">{{ t('editor.secretsEmpty') }}</p>
      <div v-for="(s, i) in job.secrets ?? []" :key="i" class="secret-row">
        <n-input
          :value="s"
          :input-props="{ name: `job-secret-${i}` }"
          :placeholder="t('editor.secretName')"
          @update:value="setSecret(job, i, $event)"
        />
        <n-button size="small" :name="`job-secret-${i}-remove`" @click="removeSecret(job, i)">
          {{ t('editor.secretRemove') }}
        </n-button>
      </div>
      <n-button size="small" dashed name="job-secret-add" @click="addSecret(job)">
        {{ t('editor.secretAdd') }}
      </n-button>
    </fieldset>

    <!-- allow_failure / retry / timeout -->
    <fieldset class="form-fieldset">
      <div class="inline-field">
        <n-switch v-model:value="job.allow_failure" name="job-allow-failure" />
        <span>{{ t('editor.allowFailure') }}</span>
      </div>
      <n-form-item :label="t('editor.retryCount')">
        <n-input-number
          class="job-number-input"
          :value="job.retry_count ?? null"
          :input-props="{ name: 'job-retry-count' }"
          :min="0"
          :precision="0"
          @update:value="setRetry(job, $event)"
        />
      </n-form-item>
      <p class="form-hint">{{ t('editor.retryHint') }}</p>
      <n-form-item :label="t('editor.timeoutMinutes')">
        <n-input-number
          class="job-number-input"
          :value="job.timeout_minutes ?? null"
          :input-props="{ name: 'job-timeout-minutes' }"
          :min="0"
          :precision="0"
          @update:value="setTimeoutMin(job, $event)"
        />
      </n-form-item>
    </fieldset>

    <!-- 产物上传 -->
    <fieldset class="form-fieldset">
      <legend>{{ t('editor.artifactUploads') }}</legend>
      <p v-if="(job.artifact_uploads ?? []).length === 0" class="form-hint">
        {{ t('editor.artifactUploadsEmpty') }}
      </p>
      <div v-for="(u, i) in job.artifact_uploads ?? []" :key="i" class="kv-row">
        <n-input
          v-model:value="u.name"
          :input-props="{ name: `job-upload-${i}-name` }"
          :placeholder="t('editor.artifactUploadName')"
          :status="uploadErrors(i).length ? 'error' : undefined"
        />
        <n-input
          v-model:value="u.path"
          :input-props="{ name: `job-upload-${i}-path` }"
          :placeholder="t('editor.artifactUploadPath')"
          :status="uploadErrors(i).length ? 'error' : undefined"
        />
        <n-button size="small" :name="`job-upload-${i}-remove`" @click="removeUpload(job, i)">
          {{ t('editor.envRemove') }}
        </n-button>
        <ul v-if="uploadErrors(i).length" class="field-errors" role="alert">
          <li v-for="(e, ei) in uploadErrors(i)" :key="ei">
            <code class="err-path">{{ e.path }}</code> {{ e.message }}
          </li>
        </ul>
      </div>
      <n-button size="small" dashed name="job-upload-add" @click="addUpload(job)">
        {{ t('editor.artifactUploadAdd') }}
      </n-button>
    </fieldset>

    <!-- 产物下载 -->
    <fieldset class="form-fieldset">
      <legend>{{ t('editor.artifactDownloads') }}</legend>
      <p v-if="(job.artifact_downloads ?? []).length === 0" class="form-hint">
        {{ t('editor.artifactDownloadsEmpty') }}
      </p>
      <div v-for="(d, i) in job.artifact_downloads ?? []" :key="i" class="kv-row kv-row-3">
        <n-input
          v-model:value="d.job"
          :input-props="{ name: `job-download-${i}-job` }"
          :placeholder="t('editor.artifactDownloadJob')"
        />
        <n-input
          v-model:value="d.name"
          :input-props="{ name: `job-download-${i}-name` }"
          :placeholder="t('editor.artifactDownloadName')"
        />
        <n-input
          v-model:value="d.path"
          :input-props="{ name: `job-download-${i}-path` }"
          :placeholder="t('editor.artifactDownloadPath')"
        />
        <n-button size="small" :name="`job-download-${i}-remove`" @click="removeDownload(job, i)">
          {{ t('editor.envRemove') }}
        </n-button>
      </div>
      <n-button size="small" dashed name="job-download-add" @click="addDownload(job)">
        {{ t('editor.artifactDownloadAdd') }}
      </n-button>
    </fieldset>

    <!-- 缓存 -->
    <fieldset class="form-fieldset">
      <legend>{{ t('editor.caches') }}</legend>
      <p v-if="(job.caches ?? []).length === 0" class="form-hint">{{ t('editor.cachesEmpty') }}</p>
      <div v-for="(c, i) in job.caches ?? []" :key="i" class="cache-card">
        <n-form-item
          :label="t('editor.cacheKey')"
          :validation-status="statusOf(`${jobPath}.caches[${i}].key`)"
        >
          <n-input v-model:value="c.key" :input-props="{ name: `job-cache-${i}-key` }" />
          <template v-if="errs(`${jobPath}.caches[${i}].key`).length" #feedback>
            <ul class="field-errors" role="alert">
              <li v-for="(e, ei) in errs(`${jobPath}.caches[${i}].key`)" :key="ei">
                <code class="err-path">{{ e.path }}</code> {{ e.message }}
              </li>
            </ul>
          </template>
        </n-form-item>
        <n-form-item
          :label="t('editor.cachePaths')"
          :validation-status="statusOf(`${jobPath}.caches[${i}].paths`)"
        >
          <n-input
            type="textarea"
            :rows="2"
            :value="pathsText(c)"
            :input-props="{ name: `job-cache-${i}-paths` }"
            @update:value="setPaths(c, $event)"
          />
          <template v-if="errs(`${jobPath}.caches[${i}].paths`).length" #feedback>
            <ul class="field-errors" role="alert">
              <li v-for="(e, ei) in errs(`${jobPath}.caches[${i}].paths`)" :key="ei">
                <code class="err-path">{{ e.path }}</code> {{ e.message }}
              </li>
            </ul>
          </template>
        </n-form-item>
        <n-form-item
          :label="t('editor.cacheFiles')"
          :validation-status="statusOf(`${jobPath}.caches[${i}].files`)"
        >
          <n-input
            type="textarea"
            :rows="2"
            :value="filesText(c)"
            :input-props="{ name: `job-cache-${i}-files` }"
            @update:value="setFiles(c, $event)"
          />
          <template v-if="errs(`${jobPath}.caches[${i}].files`).length" #feedback>
            <ul class="field-errors" role="alert">
              <li v-for="(e, ei) in errs(`${jobPath}.caches[${i}].files`)" :key="ei">
                <code class="err-path">{{ e.path }}</code> {{ e.message }}
              </li>
            </ul>
          </template>
        </n-form-item>
        <p class="form-hint">{{ t('editor.cacheKeyHint') }}</p>
        <n-button size="small" :name="`job-cache-${i}-remove`" @click="removeCache(job, i)">
          {{ t('editor.envRemove') }}
        </n-button>
      </div>
      <n-button size="small" dashed name="job-cache-add" @click="addCache(job)">
        {{ t('editor.cacheAdd') }}
      </n-button>
    </fieldset>

    <!-- 步骤 -->
    <fieldset class="form-fieldset">
      <legend>{{ t('editor.steps') }}</legend>
      <p v-if="job.steps.length === 0" class="form-hint">{{ t('editor.stepsEmpty') }}</p>
      <div v-for="(step, i) in job.steps" :key="i" class="step-card">
        <div class="step-head">
          <span class="step-type-badge" :class="`step-type-${step.type}`">
            {{ step.type === 'shell' ? t('editor.stepShell') : t('editor.stepCheckout') }}
          </span>
          <div class="step-controls">
            <n-button size="tiny" :name="`job-step-${i}-up`" @click="moveStep(job, i, -1)">
              ↑
            </n-button>
            <n-button size="tiny" :name="`job-step-${i}-down`" @click="moveStep(job, i, 1)">
              ↓
            </n-button>
            <n-button size="tiny" :name="`job-step-${i}-remove`" @click="removeStep(job, i)">
              {{ t('editor.stepRemove') }}
            </n-button>
          </div>
        </div>

        <!-- shell 配置 -->
        <template v-if="step.type === 'shell'">
          <n-form-item
            :label="t('editor.stepCommand')"
            :validation-status="statusOf(`${jobPath}.steps[${i}].command`)"
          >
            <n-input
              type="textarea"
              :rows="2"
              :value="shellCommand(step)"
              :input-props="{ name: `job-step-${i}-command` }"
              @update:value="setShellCommand(step, $event)"
            />
            <template v-if="errs(`${jobPath}.steps[${i}].command`).length" #feedback>
              <ul class="field-errors" role="alert">
                <li v-for="(e, ei) in errs(`${jobPath}.steps[${i}].command`)" :key="ei">
                  <code class="err-path">{{ e.path }}</code> {{ e.message }}
                </li>
              </ul>
            </template>
          </n-form-item>
          <n-form-item :label="t('editor.stepShellLabel')">
            <n-select
              class="job-shell-select"
              :name="`job-step-${i}-shell`"
              :value="shellShell(step) ?? ''"
              :options="shellOptions"
              :virtual-scroll="false"
              @update:value="onShellSelect(step, $event)"
            />
          </n-form-item>
        </template>

        <!-- checkout 配置 -->
        <div v-if="step.type === 'checkout'" class="inline-field">
          <n-switch
            :value="checkoutSubmodules(step)"
            :name="`job-step-${i}-submodules`"
            @update:value="setCheckoutSubmodules(step, $event)"
          />
          <span>{{ t('editor.stepSubmodules') }}</span>
        </div>

        <!-- when（两型皆有） -->
        <n-form-item
          :label="t('editor.stepWhen')"
          :validation-status="statusOf(`${jobPath}.steps[${i}].when`)"
        >
          <n-input
            :value="stepWhen(step)"
            :input-props="{ name: `job-step-${i}-when` }"
            @update:value="setStepWhen(step, $event)"
          />
          <template v-if="errs(`${jobPath}.steps[${i}].when`).length" #feedback>
            <ul class="field-errors" role="alert">
              <li v-for="(e, ei) in errs(`${jobPath}.steps[${i}].when`)" :key="ei">
                <code class="err-path">{{ e.path }}</code> {{ e.message }}
              </li>
            </ul>
          </template>
        </n-form-item>
      </div>
      <div class="step-add-row">
        <n-button size="small" dashed name="job-step-add-shell" @click="addShell(job)">
          {{ t('editor.stepAddShell') }}
        </n-button>
        <n-button size="small" dashed name="job-step-add-checkout" @click="addCheckout(job)">
          {{ t('editor.stepAddCheckout') }}
        </n-button>
      </div>
    </fieldset>
  </n-form>
</template>

<style scoped>
.editor-select-prompt {
  flex: 1;
  padding: 24px;
  text-align: center;
}

.job-form {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 14px 16px;
  border: 1px solid var(--sisy-color-border);
  border-radius: var(--sisy-radius);
  background: var(--sisy-color-surface);
}

.form-fieldset {
  border: 1px solid var(--sisy-color-border);
  border-radius: var(--sisy-radius);
  padding: 10px 12px;
  margin: 8px 0 0;
  display: flex;
  flex-direction: column;
  gap: 8px;
  align-items: flex-start;
}

.form-fieldset legend {
  font-size: 13px;
  font-weight: 600;
  color: var(--sisy-color-text-secondary);
  padding: 0 4px;
}

/* fieldset 纵排 + flex-start 下，NFormItem 显式占满宽（输入框随行拉通）。 */
.form-fieldset :deep(.n-form-item) {
  width: 100%;
}

.inline-field {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 13px;
}

.job-number-input {
  width: 160px;
}

/* 键值行（env / 产物上传）：名 + 值 + 移除。 */
.kv-row {
  display: grid;
  grid-template-columns: 1fr 1fr auto;
  gap: 6px;
  align-items: center;
  width: 100%;
}

.kv-row-3 {
  grid-template-columns: 1fr 1fr 1fr auto;
}

.kv-row .field-errors {
  grid-column: 1 / -1;
}

.secret-row {
  display: grid;
  grid-template-columns: 1fr auto;
  gap: 6px;
  align-items: center;
  width: 100%;
}

.cache-card {
  border: 1px solid var(--sisy-color-border);
  border-radius: var(--sisy-radius);
  padding: 10px;
  display: flex;
  flex-direction: column;
  gap: 4px;
  align-items: stretch;
  width: 100%;
  background: #fbfcfd;
}

/* 步骤卡 */
.step-card {
  border: 1px solid var(--sisy-color-border);
  border-radius: var(--sisy-radius);
  padding: 10px;
  display: flex;
  flex-direction: column;
  gap: 4px;
  align-items: stretch;
  width: 100%;
  background: #fbfcfd;
}

.step-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}

.step-type-badge {
  font-size: 11px;
  font-weight: 600;
  padding: 1px 8px;
  border-radius: 999px;
  border: 1px solid var(--sisy-color-border);
  background: var(--sisy-color-surface);
  color: var(--sisy-color-text-secondary);
}

.step-type-badge.step-type-shell {
  border-color: #7ab3e8;
  background: #e8f3fd;
  color: #0b5cad;
}

.step-type-badge.step-type-checkout {
  border-color: #b7dfc2;
  background: #e6f4ea;
  color: #1b6b34;
}

.step-controls {
  display: flex;
  gap: 4px;
}

.step-add-row {
  display: flex;
  gap: 8px;
}

.job-shell-select {
  width: 160px;
}

/* 内联字段错误清单（表单内就近定位）。 */
.field-errors {
  list-style: none;
  margin: 4px 0 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
  font-size: 12px;
  color: var(--sisy-color-danger);
}

.field-errors li {
  line-height: 1.4;
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
</style>

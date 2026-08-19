<script setup lang="ts">
// 任务属性表单（票 B4-T8，ADR-0020 变体 C 右侧）：选中任务的全属性显式字段——
// 名称 / 标签 / 执行环境（host|container+image）/ when / env / 机密引用 /
// allow_failure / retry_count / timeout_minutes / 产物上传 / 产物下载 / 缓存 / 步骤。
//
// 反应式约定：`job` 是父持有的响应式任务对象（`pipeline.stages[si].jobs[ji]`），
// 就地 mutate（v-model 改属性 + 数组 push/splice）——不替换 prop 绑定本身。
// 校验错误按 `jobPath` 前缀定位到字段（本地校验与服务端 422 同形 path）。

import { useI18n } from 'vue-i18n'

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

function errs(prefix: string): { path: string; message: string }[] {
  return errorsForField(props.errors, prefix)
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

// retry / timeout（空 = undefined；0 视为不设——model skip_if_zero）----------
function retryText(job: Job): string {
  return job.retry_count != null ? String(job.retry_count) : ''
}
function setRetry(job: Job, v: string): void {
  const n = Number(v)
  job.retry_count = v === '' || !Number.isFinite(n) || n < 0 ? undefined : Math.floor(n)
}
function timeoutText(job: Job): string {
  return job.timeout_minutes != null ? String(job.timeout_minutes) : ''
}
function setTimeoutMin(job: Job, v: string): void {
  const n = Number(v)
  job.timeout_minutes = v === '' || !Number.isFinite(n) || n < 0 ? undefined : Math.floor(n)
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
function cacheErrors(cacheIndex: number): { path: string; message: string }[] {
  return errs(`${props.jobPath}.caches[${cacheIndex}]`)
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
/** select 值（'' = 默认解释器 → null）转 Shell|null。 */
function shellFromSelect(v: string): Shell | null {
  return v === '' ? null : (v as Shell)
}
// checkout 配置
function checkoutSubmodules(step: Step): boolean {
  return step.type === 'checkout' ? (step.config.submodules ?? false) : false
}
function setCheckoutSubmodules(step: Step, v: boolean): void {
  if (step.type === 'checkout') step.config.submodules = v
}
function stepErrors(i: number): { path: string; message: string }[] {
  return errs(`${props.jobPath}.steps[${i}]`)
}
</script>

<template>
  <div v-if="!job" class="form-hint editor-select-prompt">{{ t('editor.selectPrompt') }}</div>

  <form v-else class="job-form" @submit.prevent>
    <!-- 名称 + 标签 -->
    <label class="field">
      <span>{{ t('editor.jobName') }}</span>
      <input name="job-name" v-model="job.name" autocomplete="off" />
    </label>
    <label class="field">
      <span>{{ t('editor.labels') }}</span>
      <textarea
        name="job-labels"
        :value="labelsText(job)"
        @input="setLabels(job, ($event.target as HTMLTextAreaElement).value)"
        rows="3"
        :placeholder="t('editor.labelsPlaceholder')"
      ></textarea>
      <p class="form-hint">{{ t('editor.labelsHint') }}</p>
    </label>

    <!-- 执行环境 -->
    <fieldset class="form-fieldset">
      <legend>{{ t('editor.execEnv') }}</legend>
      <label class="inline-field">
        <input
          type="radio"
          name="job-exec-env"
          value="host"
          :checked="!isContainer(job)"
          @change="setExecType(job, 'host')"
        />
        {{ t('editor.execHost') }}
      </label>
      <label class="inline-field">
        <input
          type="radio"
          name="job-exec-env"
          value="container"
          :checked="isContainer(job)"
          @change="setExecType(job, 'container')"
        />
        {{ t('editor.execContainer') }}
      </label>
      <label v-if="isContainer(job)" class="field">
        <span>{{ t('editor.containerImage') }}</span>
        <input
          name="job-container-image"
          :value="containerImage(job)"
          @input="setContainerImage(job, ($event.target as HTMLInputElement).value)"
          :placeholder="t('editor.containerImagePlaceholder')"
          autocomplete="off"
        />
        <ul v-if="errs(`${jobPath}.exec_env.image`).length" class="field-errors" role="alert">
          <li v-for="(e, ei) in errs(`${jobPath}.exec_env.image`)" :key="ei">
            <code class="err-path">{{ e.path }}</code> {{ e.message }}
          </li>
        </ul>
      </label>
    </fieldset>

    <!-- when -->
    <label class="field">
      <span>{{ t('editor.when') }}</span>
      <input
        name="job-when"
        :value="whenText(job)"
        @input="setWhen(job, ($event.target as HTMLInputElement).value)"
        autocomplete="off"
      />
      <p class="form-hint">{{ t('editor.whenHint') }}</p>
      <ul v-if="errs(`${jobPath}.when`).length" class="field-errors" role="alert">
        <li v-for="(e, ei) in errs(`${jobPath}.when`)" :key="ei">
          <code class="err-path">{{ e.path }}</code> {{ e.message }}
        </li>
      </ul>
    </label>

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
    </fieldset>

    <!-- 机密引用 -->
    <fieldset class="form-fieldset">
      <legend>{{ t('editor.secrets') }}</legend>
      <p class="form-hint">{{ t('editor.secretsHint') }}</p>
      <p v-if="(job.secrets ?? []).length === 0" class="form-hint">{{ t('editor.secretsEmpty') }}</p>
      <div v-for="(s, i) in job.secrets ?? []" :key="i" class="secret-row">
        <input
          :name="`job-secret-${i}`"
          :value="s"
          @input="setSecret(job, i, ($event.target as HTMLInputElement).value)"
          :placeholder="t('editor.secretName')"
          autocomplete="off"
        />
        <button type="button" class="btn" :name="`job-secret-${i}-remove`" @click="removeSecret(job, i)">
          {{ t('editor.secretRemove') }}
        </button>
      </div>
      <button type="button" class="btn" name="job-secret-add" @click="addSecret(job)">
        {{ t('editor.secretAdd') }}
      </button>
    </fieldset>

    <!-- allow_failure / retry / timeout -->
    <fieldset class="form-fieldset">
      <label class="inline-field">
        <input type="checkbox" name="job-allow-failure" v-model="job.allow_failure" />
        {{ t('editor.allowFailure') }}
      </label>
      <label class="field">
        <span>{{ t('editor.retryCount') }}</span>
        <input
          type="number"
          name="job-retry-count"
          :value="retryText(job)"
          @input="setRetry(job, ($event.target as HTMLInputElement).value)"
        />
        <p class="form-hint">{{ t('editor.retryHint') }}</p>
      </label>
      <label class="field">
        <span>{{ t('editor.timeoutMinutes') }}</span>
        <input
          type="number"
          name="job-timeout-minutes"
          :value="timeoutText(job)"
          @input="setTimeoutMin(job, ($event.target as HTMLInputElement).value)"
        />
      </label>
    </fieldset>

    <!-- 产物上传 -->
    <fieldset class="form-fieldset">
      <legend>{{ t('editor.artifactUploads') }}</legend>
      <p v-if="(job.artifact_uploads ?? []).length === 0" class="form-hint">
        {{ t('editor.artifactUploadsEmpty') }}
      </p>
      <div v-for="(u, i) in job.artifact_uploads ?? []" :key="i" class="kv-row">
        <input
          :name="`job-upload-${i}-name`"
          v-model="u.name"
          :placeholder="t('editor.artifactUploadName')"
          autocomplete="off"
        />
        <input
          :name="`job-upload-${i}-path`"
          v-model="u.path"
          :placeholder="t('editor.artifactUploadPath')"
          autocomplete="off"
        />
        <button type="button" class="btn" :name="`job-upload-${i}-remove`" @click="removeUpload(job, i)">
          {{ t('editor.envRemove') }}
        </button>
        <ul v-if="uploadErrors(i).length" class="field-errors" role="alert">
          <li v-for="(e, ei) in uploadErrors(i)" :key="ei">
            <code class="err-path">{{ e.path }}</code> {{ e.message }}
          </li>
        </ul>
      </div>
      <button type="button" class="btn" name="job-upload-add" @click="addUpload(job)">
        {{ t('editor.artifactUploadAdd') }}
      </button>
    </fieldset>

    <!-- 产物下载 -->
    <fieldset class="form-fieldset">
      <legend>{{ t('editor.artifactDownloads') }}</legend>
      <p v-if="(job.artifact_downloads ?? []).length === 0" class="form-hint">
        {{ t('editor.artifactDownloadsEmpty') }}
      </p>
      <div v-for="(d, i) in job.artifact_downloads ?? []" :key="i" class="kv-row kv-row-3">
        <input
          :name="`job-download-${i}-job`"
          v-model="d.job"
          :placeholder="t('editor.artifactDownloadJob')"
          autocomplete="off"
        />
        <input
          :name="`job-download-${i}-name`"
          v-model="d.name"
          :placeholder="t('editor.artifactDownloadName')"
          autocomplete="off"
        />
        <input
          :name="`job-download-${i}-path`"
          v-model="d.path"
          :placeholder="t('editor.artifactDownloadPath')"
          autocomplete="off"
        />
        <button type="button" class="btn" :name="`job-download-${i}-remove`" @click="removeDownload(job, i)">
          {{ t('editor.envRemove') }}
        </button>
      </div>
      <button type="button" class="btn" name="job-download-add" @click="addDownload(job)">
        {{ t('editor.artifactDownloadAdd') }}
      </button>
    </fieldset>

    <!-- 缓存 -->
    <fieldset class="form-fieldset">
      <legend>{{ t('editor.caches') }}</legend>
      <p v-if="(job.caches ?? []).length === 0" class="form-hint">{{ t('editor.cachesEmpty') }}</p>
      <div v-for="(c, i) in job.caches ?? []" :key="i" class="cache-card">
        <label class="field">
          <span>{{ t('editor.cacheKey') }}</span>
          <input :name="`job-cache-${i}-key`" v-model="c.key" autocomplete="off" />
          <p class="form-hint">{{ t('editor.cacheKeyHint') }}</p>
          <ul v-if="cacheErrors(i).length" class="field-errors" role="alert">
            <li v-for="(e, ei) in cacheErrors(i)" :key="ei">
              <code class="err-path">{{ e.path }}</code> {{ e.message }}
            </li>
          </ul>
        </label>
        <label class="field">
          <span>{{ t('editor.cachePaths') }}</span>
          <textarea
            :name="`job-cache-${i}-paths`"
            :value="pathsText(c)"
            @input="setPaths(c, ($event.target as HTMLTextAreaElement).value)"
            rows="2"
          ></textarea>
        </label>
        <label class="field">
          <span>{{ t('editor.cacheFiles') }}</span>
          <textarea
            :name="`job-cache-${i}-files`"
            :value="filesText(c)"
            @input="setFiles(c, ($event.target as HTMLTextAreaElement).value)"
            rows="2"
          ></textarea>
        </label>
        <button type="button" class="btn" :name="`job-cache-${i}-remove`" @click="removeCache(job, i)">
          {{ t('editor.envRemove') }}
        </button>
      </div>
      <button type="button" class="btn" name="job-cache-add" @click="addCache(job)">
        {{ t('editor.cacheAdd') }}
      </button>
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
            <button type="button" class="btn" :name="`job-step-${i}-up`" @click="moveStep(job, i, -1)">
              ↑
            </button>
            <button type="button" class="btn" :name="`job-step-${i}-down`" @click="moveStep(job, i, 1)">
              ↓
            </button>
            <button type="button" class="btn" :name="`job-step-${i}-remove`" @click="removeStep(job, i)">
              {{ t('editor.stepRemove') }}
            </button>
          </div>
        </div>

        <!-- shell 配置 -->
        <template v-if="step.type === 'shell'">
          <label class="field">
            <span>{{ t('editor.stepCommand') }}</span>
            <textarea
              :name="`job-step-${i}-command`"
              :value="shellCommand(step)"
              @input="setShellCommand(step, ($event.target as HTMLTextAreaElement).value)"
              rows="2"
            ></textarea>
          </label>
          <label class="field">
            <span>{{ t('editor.stepShellLabel') }}</span>
            <select
              :name="`job-step-${i}-shell`"
              :value="shellShell(step) ?? ''"
              @change="setShellShell(step, shellFromSelect(($event.target as HTMLSelectElement).value))"
            >
              <option value="">{{ t('editor.stepShellDefault') }}</option>
              <option v-for="sh in SHELLS" :key="sh" :value="sh">{{ sh }}</option>
            </select>
          </label>
        </template>

        <!-- checkout 配置 -->
        <label v-if="step.type === 'checkout'" class="inline-field">
          <input
            type="checkbox"
            :name="`job-step-${i}-submodules`"
            :checked="checkoutSubmodules(step)"
            @change="setCheckoutSubmodules(step, ($event.target as HTMLInputElement).checked)"
          />
          {{ t('editor.stepSubmodules') }}
        </label>

        <!-- when（两型皆有） -->
        <label class="field">
          <span>{{ t('editor.stepWhen') }}</span>
          <input
            :name="`job-step-${i}-when`"
            :value="stepWhen(step)"
            @input="setStepWhen(step, ($event.target as HTMLInputElement).value)"
            autocomplete="off"
          />
        </label>

        <ul v-if="stepErrors(i).length" class="field-errors" role="alert">
          <li v-for="(e, ei) in stepErrors(i)" :key="ei">
            <code class="err-path">{{ e.path }}</code> {{ e.message }}
          </li>
        </ul>
      </div>
      <div class="step-add-row">
        <button type="button" class="btn" name="job-step-add-shell" @click="addShell(job)">
          {{ t('editor.stepAddShell') }}
        </button>
        <button type="button" class="btn" name="job-step-add-checkout" @click="addCheckout(job)">
          {{ t('editor.stepAddCheckout') }}
        </button>
      </div>
    </fieldset>
  </form>
</template>

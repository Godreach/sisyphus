<script setup lang="ts">
// 拓扑轨道（票 B4-T8，ADR-0020 变体 C 左侧）：阶段=列、任务=chip，布局由数据派生、
// 不可手拖、无画布坐标。chip 选中导航右侧表单；状态色边（选中/有错）；重试与
// allow-failure 角标。增删/重排阶段与任务（上移/下移按钮，非拖拽）。
//
// 纯展示+事件：结构变更（增删/重排/选中）一律 emit 给父（PipelineEditorView）
// 处理，本组件只渲染阶段名/when 的就地编辑（v-model 改属性，同 JobFormPanel 纪律）。

import { useI18n } from 'vue-i18n'

import type { Pipeline, Stage } from '@/model/pipeline'
import { errorsForStage, errorsForJob } from '@/model/editor'

const props = defineProps<{
  pipeline: Pipeline
  selection: { stageIndex: number; jobIndex: number } | null
  errors: { path: string; message: string }[]
}>()

const emit = defineEmits<{
  (e: 'select', stageIndex: number, jobIndex: number): void
  (e: 'add-stage'): void
  (e: 'delete-stage', stageIndex: number): void
  (e: 'move-stage', stageIndex: number, dir: number): void
  (e: 'add-job', stageIndex: number): void
  (e: 'delete-job', stageIndex: number, jobIndex: number): void
  (e: 'move-job', stageIndex: number, jobIndex: number, dir: number): void
}>()

const { t } = useI18n()

function isSelected(si: number, ji: number): boolean {
  return props.selection?.stageIndex === si && props.selection?.jobIndex === ji
}

// 阶段 when：空 = undefined（空串会被 when 校验判 when_syntax）。
function stageWhenText(stage: Stage): string {
  return stage.when ?? ''
}
function setStageWhen(stage: Stage, v: string): void {
  stage.when = v === '' ? undefined : v
}
</script>

<template>
  <aside class="editor-track">
    <div class="track-toolbar">
      <span class="track-title">{{ t('editor.trackTitle') }}</span>
      <button type="button" class="btn btn-primary" name="track-add-stage" @click="emit('add-stage')">
        {{ t('editor.addStage') }}
      </button>
    </div>
    <p class="form-hint">{{ t('editor.trackHint') }}</p>

    <p v-if="pipeline.stages.length === 0" class="form-hint">{{ t('editor.trackEmpty') }}</p>

    <div class="track-columns">
      <div
        v-for="(stage, si) in pipeline.stages"
        :key="si"
        class="stage-column"
        :class="{ 'stage-has-error': errorsForStage(errors, si).length > 0 }"
      >
        <div class="stage-column-head">
          <input
            class="stage-name-input"
            :name="`stage-${si}-name`"
            v-model="stage.name"
            :placeholder="t('editor.stage')"
            autocomplete="off"
          />
          <div class="stage-controls">
            <button
              type="button"
              class="btn chip-mini"
              :name="`stage-${si}-up`"
              :disabled="si === 0"
              :title="t('editor.moveUp')"
              @click="emit('move-stage', si, -1)"
            >↑</button>
            <button
              type="button"
              class="btn chip-mini"
              :name="`stage-${si}-down`"
              :disabled="si === pipeline.stages.length - 1"
              :title="t('editor.moveDown')"
              @click="emit('move-stage', si, 1)"
            >↓</button>
            <button
              type="button"
              class="btn chip-mini"
              :name="`stage-${si}-delete`"
              @click="emit('delete-stage', si)"
            >{{ t('editor.deleteStage') }}</button>
          </div>
          <label class="field stage-when-field">
            <span>{{ t('editor.stageWhen') }}</span>
            <input
              :name="`stage-${si}-when`"
              :value="stageWhenText(stage)"
              @input="setStageWhen(stage, ($event.target as HTMLInputElement).value)"
              autocomplete="off"
            />
          </label>
          <ul v-if="errorsForStage(errors, si).length > 0" class="field-errors" role="alert">
            <li v-for="(e, ei) in errorsForStage(errors, si)" :key="ei">
              <code class="err-path">{{ e.path }}</code> {{ e.message }}
            </li>
          </ul>
        </div>

        <ul class="track-jobs">
          <li v-for="(job, ji) in stage.jobs" :key="ji" class="track-job-row">
            <button
              type="button"
              class="job-chip"
              :class="{
                'chip-selected': isSelected(si, ji),
                'chip-error': errorsForJob(errors, si, ji).length > 0,
              }"
              :name="`chip-${si}-${ji}`"
              @click="emit('select', si, ji)"
            >
              <span class="chip-name">{{ job.name || t('editor.unnamedJob') }}</span>
              <span v-if="(job.retry_count ?? 0) > 0" class="chip-badge chip-retry">
                {{ t('editor.retryBadge', { n: job.retry_count }) }}
              </span>
              <span v-if="job.allow_failure" class="chip-badge chip-allow-failure">
                {{ t('editor.allowFailureBadge') }}
              </span>
              <span
                v-if="job.exec_env?.type === 'container'"
                class="chip-badge chip-container"
              >{{ t('editor.containerBadge') }}</span>
            </button>
            <div class="chip-controls">
              <button
                type="button"
                class="btn chip-mini"
                :name="`job-${si}-${ji}-up`"
                :disabled="ji === 0"
                @click="emit('move-job', si, ji, -1)"
              >↑</button>
              <button
                type="button"
                class="btn chip-mini"
                :name="`job-${si}-${ji}-down`"
                :disabled="ji === stage.jobs.length - 1"
                @click="emit('move-job', si, ji, 1)"
              >↓</button>
              <button
                type="button"
                class="btn chip-mini"
                :name="`job-${si}-${ji}-delete`"
                :title="t('editor.deleteJob')"
                :aria-label="t('editor.deleteJob')"
                @click="emit('delete-job', si, ji)"
              >✕</button>
            </div>
          </li>
        </ul>

        <button
          type="button"
          class="btn"
          :name="`stage-${si}-add-job`"
          @click="emit('add-job', si)"
        >{{ t('editor.addJob') }}</button>
      </div>
    </div>
  </aside>
</template>

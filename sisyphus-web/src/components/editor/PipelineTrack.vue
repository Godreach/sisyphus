<script setup lang="ts">
// 拓扑轨道（票 B4-T8，ADR-0020 变体 C 左侧）：阶段=列、任务=chip，布局由数据派生、
// 不可手拖、无画布坐标。chip 选中导航右侧表单；状态色边（选中/有错）；重试与
// allow-failure 角标。增删/重排阶段与任务（上移/下移按钮，非拖拽）。
//
// 纯展示+事件：结构变更（增删/重排/选中）一律 emit 给父（PipelineEditorView）
// 处理，本组件只渲染阶段名/when 的就地编辑（v-model 改属性，同 JobFormPanel 纪律）。
// #96: 迁移 Naive UI——任务 chip 改 NTag（checkable+checked 表达选中态高亮，
// 有错附 chip-error 类红边）、阶段名/when 改 NInput、增删/重排按钮改 NButton，
// 交互不变。
// 票 #109: 定稿设计语言——轨道面板/阶段列走 token 驱动卡片语义（白卡 + 凹陷填充），
// 角标换定稿胶囊柔和色族（深色系 token 深色跟随），交互不变。

import { useI18n } from 'vue-i18n'
import { NButton, NInput, NTag } from 'naive-ui'

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
  return props.selection?.stageIndex === si && props.selection.jobIndex === ji
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
      <n-button size="small" type="primary" name="track-add-stage" @click="emit('add-stage')">
        {{ t('editor.addStage') }}
      </n-button>
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
          <n-input
            v-model:value="stage.name"
            class="stage-name-input"
            :input-props="{ name: `stage-${si}-name` }"
            :placeholder="t('editor.stage')"
            size="small"
          />
          <div class="stage-controls">
            <n-button
              size="tiny"
              :name="`stage-${si}-up`"
              :disabled="si === 0"
              :title="t('editor.moveUp')"
              @click="emit('move-stage', si, -1)"
            >↑</n-button>
            <n-button
              size="tiny"
              :name="`stage-${si}-down`"
              :disabled="si === pipeline.stages.length - 1"
              :title="t('editor.moveDown')"
              @click="emit('move-stage', si, 1)"
            >↓</n-button>
            <n-button size="tiny" :name="`stage-${si}-delete`" @click="emit('delete-stage', si)">
              {{ t('editor.deleteStage') }}
            </n-button>
          </div>
          <div class="stage-when-field">
            <span class="stage-when-label">{{ t('editor.stageWhen') }}</span>
            <n-input
              :value="stageWhenText(stage)"
              :input-props="{ name: `stage-${si}-when` }"
              size="small"
              @update:value="setStageWhen(stage, $event)"
            />
          </div>
          <ul v-if="errorsForStage(errors, si).length > 0" class="field-errors" role="alert">
            <li v-for="(e, ei) in errorsForStage(errors, si)" :key="ei">
              <code class="err-path">{{ e.path }}</code> {{ e.message }}
            </li>
          </ul>
        </div>

        <ul class="track-jobs">
          <li v-for="(job, ji) in stage.jobs" :key="ji" class="track-job-row">
            <!-- checkable NTag：点击（update:checked）即选中导航；checked = 选中态
                 高亮（主题 primary）；有错附 chip-error 红边。 -->
            <n-tag
              checkable
              :checked="isSelected(si, ji)"
              class="job-chip"
              :class="{
                'chip-selected': isSelected(si, ji),
                'chip-error': errorsForJob(errors, si, ji).length > 0,
              }"
              :name="`chip-${si}-${ji}`"
              @update:checked="emit('select', si, ji)"
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
            </n-tag>
            <div class="chip-controls">
              <n-button
                size="tiny"
                :name="`job-${si}-${ji}-up`"
                :disabled="ji === 0"
                @click="emit('move-job', si, ji, -1)"
              >↑</n-button>
              <n-button
                size="tiny"
                :name="`job-${si}-${ji}-down`"
                :disabled="ji === stage.jobs.length - 1"
                @click="emit('move-job', si, ji, 1)"
              >↓</n-button>
              <n-button
                size="tiny"
                :name="`job-${si}-${ji}-delete`"
                :title="t('editor.deleteJob')"
                :aria-label="t('editor.deleteJob')"
                @click="emit('delete-job', si, ji)"
              >✕</n-button>
            </div>
          </li>
        </ul>

        <n-button size="small" dashed :name="`stage-${si}-add-job`" @click="emit('add-job', si)">
          {{ t('editor.addJob') }}
        </n-button>
      </div>
    </div>
  </aside>
</template>

<style scoped>
/* 轨道面板：定稿卡片语义（白底 12px 圆角无描边，同 sisy-card）。 */
.editor-track {
  flex: 0 0 340px;
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 16px;
  border-radius: var(--sisy-radius-card);
  background: var(--sisy-color-surface);
}

.track-toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}

.track-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--sisy-color-text-secondary);
}

.track-columns {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

/* 阶段列：卡内嵌套容器 = 页面底色的凹陷填充（token 驱动，深色跟随）。 */
.stage-column {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 12px;
  border: 1px solid var(--sisy-color-border);
  border-radius: var(--sisy-radius);
  background: var(--sisy-color-bg);
}

.stage-column.stage-has-error {
  border-color: var(--sisy-color-danger);
}

.stage-column-head {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.stage-name-input {
  font-weight: 600;
}

.stage-controls {
  display: flex;
  gap: 4px;
  flex-wrap: wrap;
}

.stage-when-field {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.stage-when-label {
  font-size: 12px;
  color: var(--sisy-color-text-secondary);
}

/* 任务 chip（NTag）：占满行宽、可点。 */
.track-jobs {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.track-job-row {
  display: flex;
  align-items: center;
  gap: 4px;
}

.job-chip {
  flex: 1;
  min-width: 0;
  justify-content: flex-start;
  cursor: pointer;
  height: auto;
  min-height: 28px;
  padding: 2px 10px;
}

.job-chip.chip-error:not(.chip-selected) {
  border-color: var(--sisy-color-danger);
  color: var(--sisy-color-danger);
}

.chip-name {
  font-weight: 600;
}

/* chip 角标：定稿胶囊语义（柔和底色 + 同族正文色，token 驱动，深色跟随）。 */
.chip-badge {
  font-size: 11px;
  padding: 1px 6px;
  border-radius: var(--sisy-radius-pill);
}

.chip-badge.chip-retry {
  background: var(--sisy-color-success-soft);
  color: var(--sisy-color-success);
}

.chip-badge.chip-allow-failure {
  background: var(--sisy-color-warning-soft);
  color: var(--sisy-color-warning-text);
}

.chip-badge.chip-container {
  background: var(--sisy-color-primary-soft);
  color: var(--sisy-color-primary);
}

.chip-controls {
  display: flex;
  gap: 2px;
}

/* 内联字段错误清单（阶段 when 定位）。 */
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

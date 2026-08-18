<script setup lang="ts">
// 构建日志视图（票 B4-T4，ADR-0013）：SSE 日志流按步骤折叠渲染。
//
// - `openLogStream` 消费原生 EventSource（同源 cookie；断线自动重连带
//   Last-Event-ID 原地续传——原生语义，本组件不重复实现）。
// - 输出块合流带 stream 标记（stdout/stderr 合流保序，stderr 醒目）；
//   按步骤折叠渲染（步骤头点击切换折叠态）。
// - ANSI 剥离（纯文本渲染）；截断事件显著标注（截断不判败）。
// - 首连失败 = 端点未交付（degraded）→ 显式标注退化态（Spec B4 缺端点纪律）。
// - 任务终态事件送达即关流（ADR-0013）。

import { computed, onBeforeUnmount, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'

import {
  openLogStream,
  buildLogStreamUrl,
  type LogStreamConnection,
  type LogStreamConnectionStatus,
} from '@/api/sse'
import {
  appendEvent,
  createLogModel,
  toggleStep,
  type BuildLogModel,
} from '@/model/buildLog'
import { formatBytes, formatDuration } from '@/utils/format'

const props = defineProps<{
  project: string
  pipeline: string
  buildNumber: number
  job: string
  attempt: number
}>()

const { t } = useI18n()

const model = ref<BuildLogModel>(createLogModel())
const connectionStatus = ref<LogStreamConnectionStatus>('connecting')

let connection: LogStreamConnection | null = null

const logUrl = computed(() =>
  buildLogStreamUrl(
    props.project,
    props.pipeline,
    props.buildNumber,
    props.job,
    props.attempt,
  ),
)

/** 折叠态展示（无折叠步骤时按钮禁用）。 */
const anyCollapsible = computed(() => model.value.steps.length > 0)
const allExpanded = computed(() => model.value.steps.every((s) => !s.collapsed))
const hasLog = computed(() => model.value.steps.length > 0 || model.value.preamble.length > 0)

function openStream(): void {
  connection?.close()
  model.value = createLogModel()
  connectionStatus.value = 'connecting'
  connection = openLogStream(
    logUrl.value,
    (event) => appendEvent(model.value, event),
    (status) => {
      connectionStatus.value = status
    },
  )
}

function onToggleStep(index: number): void {
  toggleStep(model.value, index)
}

function expandAll(): void {
  model.value.steps.forEach((s) => {
    s.collapsed = false
  })
}

function collapseAll(): void {
  model.value.steps.forEach((s) => {
    s.collapsed = true
  })
}

watch(
  [() => props.job, () => props.attempt],
  () => {
    openStream()
  },
  { immediate: true },
)

onBeforeUnmount(() => {
  connection?.close()
  connection = null
})
</script>

<template>
  <section class="build-log">
    <header class="build-log-header">
      <h3>{{ t('buildLog.title') }}</h3>
      <div v-if="hasLog" class="build-log-actions">
        <button
          type="button"
          class="build-log-toggle"
          :disabled="!anyCollapsible"
          @click="allExpanded ? collapseAll() : expandAll()"
        >
          {{ allExpanded ? t('buildLog.collapseAll') : t('buildLog.expandAll') }}
        </button>
      </div>
    </header>

    <!-- 退化态：SSE 日志端点尚未交付（Spec B4 缺端点纪律：显式标注）。 -->
    <div v-if="connectionStatus === 'degraded'" class="build-log-degraded" role="status">
      {{ t('buildLog.degraded') }}
    </div>

    <!-- 连接中/重连中：轻提示（重连为原生 EventSource 自动行为，携
         Last-Event-ID 续传，ADR-0013）。 -->
    <div
      v-else-if="connectionStatus === 'connecting' || connectionStatus === 'reconnecting'"
      class="build-log-status"
    >
      {{
        connectionStatus === 'connecting'
          ? t('buildLog.connecting')
          : t('buildLog.reconnecting')
      }}
    </div>

    <div
      v-else-if="!hasLog && connectionStatus !== 'open' && connectionStatus !== 'closed'"
      class="build-log-status"
    >
      {{ t('buildLog.noLog') }}
    </div>

    <div v-if="hasLog" class="build-log-body">
      <!-- 截断显著标注（截断不判败，ADR-0013）。 -->
      <div v-if="model.truncatedAt != null" class="build-log-truncated" role="alert">
        {{ t('buildLog.truncated', { limit: formatBytes(model.truncatedAt) }) }}
      </div>

      <!-- 步骤前的输出（工作区准备等）。 -->
      <div v-if="model.preamble.length > 0" class="build-log-preamble">
        <p
          v-for="(line, i) in model.preamble"
          :key="`p-${i}`"
          class="log-line"
          :class="{ 'log-stderr': line.stream === 'stderr' }"
        >
          {{ line.text }}
        </p>
      </div>

      <!-- 步骤卡（折叠渲染）。 -->
      <article v-for="step in model.steps" :key="step.index" class="build-log-step">
        <button
          type="button"
          class="build-log-step-head"
          :aria-expanded="!step.collapsed"
          @click="onToggleStep(step.index)"
        >
          <span class="build-log-step-caret">{{ step.collapsed ? '▸' : '▾' }}</span>
          <span class="build-log-step-name">
            {{ step.name || `${t('buildLog.step')} ${step.index + 1}` }}
          </span>
          <span v-if="step.command" class="build-log-step-cmd mono">{{ step.command }}</span>
          <span v-if="step.exitCode != null" class="build-log-step-exit">
            {{ t('buildLog.exitCode', { code: step.exitCode }) }}
          </span>
          <span v-if="step.durationMs != null" class="build-log-step-duration">
            {{ formatDuration(step.durationMs) }}
          </span>
        </button>
        <div v-if="!step.collapsed" class="build-log-step-body">
          <p
            v-for="(line, i) in step.lines"
            :key="`${step.index}-${i}`"
            class="log-line"
            :class="{ 'log-stderr': line.stream === 'stderr' }"
          >
            {{ line.text }}
          </p>
        </div>
      </article>

      <!-- 终态：任务终态事件送达即关流（ADR-0013）。 -->
      <div v-if="model.ended" class="build-log-ended">
        {{ t('buildLog.ended') }}
      </div>
    </div>
  </section>
</template>

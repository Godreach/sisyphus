<script setup lang="ts">
// 构建日志视图（票 B4-T4；视觉随票 #107 收编定稿设计语言，ADR-0013）：
// SSE 日志流按步骤折叠渲染。
//
// - `openLogStream` 消费原生 EventSource（同源 cookie；断线自动重连带
//   Last-Event-ID 原地续传——原生语义，本组件不重复实现）。
// - 输出块合流带 stream 标记（stdout/stderr 合流保序，stderr 醒目）；
//   按步骤折叠渲染（步骤头点击切换折叠态）。
// - ANSI 剥离（纯文本渲染，ADR-0013——日志字节原样存储、渲染时剥离）；
//   截断事件显著标注（截断不判败）。
// - 首连失败 = 端点未交付（degraded）→ 显式标注退化态（Spec B4 缺端点纪律）。
// - 任务终态事件送达即关流（ADR-0013）。
// - 自动跟随：日志体为限高滚动容器，输出块到达即滚动到底（运行中实时
//   跟随至终态）；用户上滚阅读历史时暂停跟随，滚回底部自动恢复。
// - 视觉：`--sisy-*` token + 共享组件类（btn-outline/state-note 同族提示条），
//   日志体保持固定深底等宽形态（浅/深色主题下均为惯例终端观感）。

import { computed, nextTick, onBeforeUnmount, ref, watch } from 'vue'
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
  stick = true
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

// 自动跟随：stick=true 时每次事件推进都滚到底；用户上滚（离底 >32px）
// 即暂停跟随，重新滚到底部附近自动恢复。
const bodyEl = ref<HTMLElement | null>(null)
let stick = true

function onBodyScroll(): void {
  const el = bodyEl.value
  if (el == null) return
  stick = el.scrollTop + el.clientHeight >= el.scrollHeight - 32
}

function followBottom(): void {
  void nextTick(() => {
    const el = bodyEl.value
    if (el == null || !stick) return
    el.scrollTop = el.scrollHeight
  })
}

/** 事件量信号：任一变化（新步骤/新输出块/终态）触发跟随判定。 */
const eventTick = computed(() => {
  const m = model.value
  return (
    m.preamble.length +
    m.steps.length +
    (m.steps[m.steps.length - 1]?.lines.length ?? 0) +
    (m.ended ? 1 : 0)
  )
})

watch(eventTick, followBottom)

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

/** 步骤退出码色（0=成功绿，非 0=失败红）。 */
function stepExitClass(exitCode: number | null): string {
  if (exitCode == null) return ''
  return exitCode === 0 ? 'exit-ok' : 'exit-fail'
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
      <button
        v-if="hasLog"
        type="button"
        class="btn-outline build-log-collapse"
        :disabled="!anyCollapsible"
        @click="allExpanded ? collapseAll() : expandAll()"
      >
        {{ allExpanded ? t('buildLog.collapseAll') : t('buildLog.expandAll') }}
      </button>
    </header>

    <!-- 退化态：SSE 日志端点尚未交付（Spec B4 缺端点纪律：显式标注）。 -->
    <div v-if="connectionStatus === 'degraded'" class="state-note" role="status">
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

    <!-- 空态：流已开但无任何输出（历史为空且未推块——如无日志的终态构建）。 -->
    <div v-else-if="!hasLog" class="build-log-status">
      {{ t('buildLog.noLog') }}
    </div>

    <div v-if="hasLog" ref="bodyEl" class="build-log-body" @scroll="onBodyScroll">
      <!-- 截断显著标注（截断不判败，ADR-0013）。 -->
      <div v-if="model.truncatedAt != null" class="state-note build-log-truncated" role="alert">
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

      <!-- 步骤卡（折叠渲染；步骤头点击切换）。 -->
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
          <span v-if="step.exitCode != null" class="build-log-step-exit" :class="stepExitClass(step.exitCode)">
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

<style scoped>
.build-log {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.build-log-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}

.build-log-header h3 {
  margin: 0;
  font-size: 13px;
  font-weight: 600;
  color: var(--sisy-color-text);
}

.build-log-collapse {
  height: 24px;
  padding: 0 10px;
  font-size: 11px;
}

.build-log-status {
  font-size: 12px;
  color: var(--sisy-color-text-tertiary);
}

/* 日志体：固定深底终端形态（浅/深色主题下一致），与页面柔和对比。 */
.build-log-body {
  max-height: 420px;
  overflow-y: auto;
  background: #16181d;
  border-radius: var(--sisy-radius);
  padding: 10px 12px;
  font-size: 12.5px;
  line-height: 1.55;
}

.build-log-preamble {
  color: #d7e0ee;
}

.log-line {
  margin: 0;
  white-space: pre-wrap;
  word-break: break-word;
  font-family: ui-monospace, 'Cascadia Code', Consolas, 'SF Mono', Menlo, monospace;
  color: #d7e0ee;
}

.log-line.log-stderr {
  color: #ffb4a9;
}

.build-log-step {
  margin-top: 8px;
  border-top: 1px solid #262b36;
  padding-top: 6px;
}

.build-log-step-head {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  background: none;
  border: none;
  color: #d7e0ee;
  cursor: pointer;
  padding: 4px 0;
  text-align: left;
  font-family: inherit;
  font-size: 12.5px;
}

.build-log-step-caret {
  color: #8aa0c0;
}

.build-log-step-name {
  font-weight: 600;
}

.build-log-step-cmd {
  color: #8aa0c0;
  font-size: 12px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.build-log-step-exit {
  margin-left: auto;
  font-size: 12px;
  white-space: nowrap;
}

.build-log-step-exit.exit-ok {
  color: #6fd78a;
}

.build-log-step-exit.exit-fail {
  color: #ff8a80;
}

.build-log-step-duration {
  color: #8aa0c0;
  font-size: 12px;
  white-space: nowrap;
}

.build-log-step-body {
  padding: 4px 0 0 14px;
}

.build-log-truncated {
  margin-bottom: 8px;
}

.build-log-ended {
  margin-top: 8px;
  color: #8aa0c0;
  border-top: 1px solid #262b36;
  padding-top: 6px;
}

.mono {
  font-family: ui-monospace, 'Cascadia Code', Consolas, 'SF Mono', Menlo, monospace;
}
</style>

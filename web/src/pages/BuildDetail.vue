<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15)
import { computed, ref } from 'vue'
import { useRoute } from 'vue-router'
import { useI18n } from 'vue-i18n'
import { builds, logEvents } from '../data/mock'

const { t } = useI18n()
const route = useRoute()
const build = computed(() => builds.find((b) => b.id === route.params.id) ?? builds[0])
const activeJob = ref(build.value.stages.flatMap((s) => s.jobs)[0]?.id)

function badgeClass(s: string) {
  return { success: 'b-ok', failure: 'b-err', running: 'b-run', queued: 'b-dim', cancelled: 'b-warn', unknown: 'b-unknown', skipped: 'b-dim' }[s] ?? 'b-dim'
}
</script>

<template>
  <div class="row" style="justify-content: space-between">
    <h1 class="mono">
      #{{ build.number }}<span v-if="build.attempt > 1" class="attempt"> · {{ t('builds.attempt', { n: build.attempt }) }}</span>
    </h1>
    <div class="row">
      <button class="btn">{{ t('common.rerunAll') }}</button>
      <button v-if="build.status === 'failure'" class="btn">{{ t('common.rerunFailed') }}</button>
      <button class="btn danger">{{ t('common.cancelBuild') }}</button>
    </div>
  </div>

  <div class="card meta row">
    <span><span class="badge" :class="badgeClass(build.status)">{{ t(`builds.${build.status}`) }}</span></span>
    <span>{{ t('builds.triggeredBy') }}: <b>{{ build.triggeredBy }}</b>（{{ build.triggerKind }}）</span>
    <span>{{ t('builds.commit') }}: <code>{{ build.commit }}</code></span>
    <span>{{ build.startedAt }}</span>
    <a href="#" @click.prevent>{{ t('builds.snapshot') }} (rev 14)</a>
  </div>

  <div class="layout">
    <div class="stages">
      <div v-for="st in build.stages" :key="st.id" class="stage card">
        <div class="stage-head">
          <b>{{ st.name }}</b>
          <code v-if="st.when" class="when">when: {{ st.when }}</code>
        </div>
        <div class="jobs">
          <div
            v-for="j in st.jobs"
            :key="j.id"
            class="job"
            :class="{ active: j.id === activeJob }"
            @click="activeJob = j.id"
          >
            <div class="row">
              <span class="badge" :class="badgeClass(j.status)">{{ t(`builds.${j.status}`) }}</span>
              <b>{{ j.name }}</b>
            </div>
            <div class="job-meta mono">
              <span v-if="j.agentName">@{{ j.agentName }}</span>
              <span v-if="j.durationSec">{{ Math.floor(j.durationSec / 60) }}m{{ j.durationSec % 60 }}s</span>
              <span v-if="j.containerImage">🐳 {{ j.containerImage }}</span>
              <span v-if="j.retry">↻{{ j.retry }}</span>
            </div>
            <div v-if="j.missingLabels" class="missing">
              ⏳ {{ t('builds.missingLabels', { labels: j.missingLabels.join(', ') }) }}
            </div>
            <div class="steps mono">
              <div v-for="s in j.steps" :key="s.id" class="step">· {{ s.name }}</div>
            </div>
          </div>
        </div>
      </div>
    </div>

    <div class="logs card">
      <div class="row" style="justify-content: space-between; margin-bottom: 8px">
        <h2 style="margin: 0">{{ t('builds.logs') }} <span class="mono dim">{{ activeJob }}</span></h2>
        <div class="row">
          <span class="live">● SSE</span>
          <button class="btn">{{ t('common.download') }}</button>
        </div>
      </div>
      <div class="log mono">
        <div v-for="e in logEvents" :key="e.seq" class="ev" :class="e.kind + (e.stream === 'stderr' ? ' err' : '')">
          <template v-if="e.kind === 'step-start'"><span class="tag s">STEP</span> {{ t('builds.stepStart') }} · {{ e.step }}<br />{{ e.text }}</template>
          <template v-else-if="e.kind === 'step-end'"><span class="tag e">STEP</span> {{ t('builds.stepEnd', { code: e.exitCode }) }}</template>
          <template v-else>{{ e.text }}</template>
        </div>
      </div>
      <h2>{{ t('builds.artifacts') }}</h2>
      <div class="art mono">
        <div>📦 target/debug/sisyphus <button class="btn">{{ t('common.download') }}</button></div>
        <div>📦 sisyphus.tar.gz <button class="btn">{{ t('common.download') }}</button></div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.meta { flex-wrap: wrap; gap: 18px; font-size: 13px; margin-bottom: 14px; }
.attempt { font-size: 13px; color: var(--ink-dim); font-weight: 400; }
.layout { display: grid; grid-template-columns: minmax(380px, 1fr) minmax(420px, 1.2fr); gap: 14px; align-items: start; }
.stages { display: flex; flex-direction: column; gap: 10px; }
.stage-head { display: flex; justify-content: space-between; margin-bottom: 8px; }
.when { font-size: 11px; color: var(--ink-dim); }
.jobs { display: flex; gap: 8px; flex-wrap: wrap; }
.job { border: 1px solid var(--line); border-radius: 6px; padding: 8px 10px; flex: 1; min-width: 150px; cursor: pointer; }
.job.active { border-color: var(--accent); box-shadow: 0 0 0 2px #dbeafe; }
.job-meta { display: flex; gap: 10px; font-size: 11px; color: var(--ink-dim); margin-top: 4px; flex-wrap: wrap; }
.missing { margin-top: 6px; font-size: 12px; color: var(--warn); background: #fffbeb; border-radius: 4px; padding: 3px 6px; }
.steps { margin-top: 6px; font-size: 11.5px; color: var(--ink-dim); }
.step { padding: 1px 0; }
.logs { position: sticky; top: 16px; }
.log { background: #0f172a; color: #d7e0ee; border-radius: 6px; padding: 10px 12px; max-height: 340px; overflow: auto; font-size: 12px; line-height: 1.55; }
.ev.err { color: #fca5a5; }
.tag { font-size: 10px; border-radius: 3px; padding: 0 4px; margin-right: 4px; font-weight: 700; }
.tag.s { background: #1e3a5f; color: #7dd3fc; }
.tag.e { background: #14532d; color: #86efac; }
.live { color: var(--err); font-size: 11px; font-weight: 700; }
.dim { color: var(--ink-dim); font-weight: 400; font-size: 12px; }
.art { display: flex; flex-direction: column; gap: 6px; font-size: 12.5px; }
</style>

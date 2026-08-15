<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15). Variant A: Vue Flow canvas editor.
// Spatial topology: stages as columns, jobs as draggable nodes, edges fan
// out stage-to-stage (stage serial, jobs parallel). Property drawer on select.
import { computed, reactive, ref } from 'vue'
import { VueFlow, type Node, type Edge } from '@vue-flow/core'
import { Background } from '@vue-flow/background'
import { Controls } from '@vue-flow/controls'
import { useI18n } from 'vue-i18n'
import { projects, type Job } from '../../data/mock'

import '@vue-flow/core/dist/style.css'
import '@vue-flow/core/dist/theme-default.css'
import '@vue-flow/controls/dist/style.css'

const { t } = useI18n()
const pipeline = reactive(structuredClone(projects[0].pipelines[0]))
const selected = ref<Job | null>(null)
const showJson = ref(false)

const nodes = computed<Node[]>(() => {
  const out: Node[] = []
  pipeline.stages.forEach((st, ci) => {
    out.push({
      id: `stage-${st.id}`,
      position: { x: ci * 250, y: 0 },
      data: { label: `${t('pipeline.stages')}: ${st.name}${st.when ? ` (when: ${st.when})` : ''}` },
      class: 'stage-node',
      draggable: false,
      selectable: false,
    })
    st.jobs.forEach((j, ri) => {
      out.push({
        id: j.id,
        position: { x: ci * 250, y: 90 + ri * 110 },
        data: { label: `${j.name}\n${j.containerImage ? '🐳 ' + j.containerImage : '🖥 host'} · ${j.labels.join('+')}` },
        class: `job-node st-${j.status}`,
        draggable: true,
      })
    })
  })
  return out
})

const edges = computed<Edge[]>(() => {
  const out: Edge[] = []
  for (let i = 0; i < pipeline.stages.length - 1; i++) {
    for (const from of pipeline.stages[i].jobs) {
      for (const to of pipeline.stages[i + 1].jobs) {
        out.push({ id: `${from.id}-${to.id}`, source: from.id, target: to.id, animated: true })
      }
    }
  }
  return out
})

function onNodeClick({ node }: { node: Node }) {
  const all = pipeline.stages.flatMap((s) => s.jobs)
  selected.value = all.find((j) => j.id === node.id) ?? null
}

function addJob(stageIdx: number) {
  pipeline.stages[stageIdx].jobs.push({
    id: `new-${Math.random().toString(36).slice(2, 6)}`,
    name: 'new-job',
    labels: ['linux'],
    env: {},
    secrets: [],
    steps: [{ id: 's', type: 'shell', name: 'echo', command: 'echo hello' }],
    status: 'skipped',
  } as Job)
}
</script>

<template>
  <div class="variant-a">
    <div class="toolbar row">
      <button v-for="(st, i) in pipeline.stages" :key="st.id" class="btn" @click="addJob(i)">+ {{ t('pipeline.newJob') }} @ {{ st.name }}</button>
      <button class="btn" @click="pipeline.stages.push({ id: `s${pipeline.stages.length + 1}`, name: `stage-${pipeline.stages.length + 1}`, jobs: [] })">+ {{ t('pipeline.newStage') }}</button>
      <span class="spacer" />
      <button class="btn" @click="showJson = !showJson">{{ showJson ? 'JSON −' : 'JSON +' }}</button>
      <button class="btn primary">{{ t('common.save') }} · rev {{ pipeline.rev }} -> {{ pipeline.rev + 1 }}</button>
    </div>

    <div class="canvas-wrap">
      <VueFlow :nodes="nodes" :edges="edges" :default-zoom="0.8" fit-view-on-init @node-click="onNodeClick">
        <Background :gap="20" />
        <Controls />
      </VueFlow>

      <aside v-if="selected" class="drawer card">
        <div class="row" style="justify-content: space-between">
          <b>{{ t('pipeline.jobs') }}</b>
          <button class="btn" @click="selected = null">✕</button>
        </div>
        <div class="f"><label>{{ t('common.name') }}</label><input v-model="selected.name" type="text" /></div>
        <div class="f"><label>{{ t('pipeline.labels') }}</label><input :value="selected.labels.join(', ')" type="text" @change="selected.labels = ($event.target as HTMLInputElement).value.split(',').map((s) => s.trim())" /></div>
        <div class="f"><label>{{ t('pipeline.containerImage') }}</label>
          <select v-model="selected.containerImage">
            <option :value="undefined">🖥 {{ t('pipeline.hostRun') }}</option>
            <option value="rust:1.83">rust:1.83</option>
            <option value="node:22">node:22</option>
          </select>
        </div>
        <div class="f"><label>{{ t('pipeline.when') }}</label><input v-model="selected.when" type="text" placeholder="e.g. always()" /></div>
        <div class="f"><label>{{ t('pipeline.retry') }}</label><input v-model.number="selected.retry" type="text" /></div>
        <div class="f"><label>{{ t('pipeline.timeout') }} ({{ t('common.min') }})</label><input v-model.number="selected.timeoutMin" type="text" /></div>
        <div class="f"><label>{{ t('pipeline.secrets') }}</label><input :value="selected.secrets.join(', ')" type="text" /></div>
        <div class="f"><label>{{ t('pipeline.artifactUpload') }}</label><input :value="(selected.artifacts ?? []).join(', ')" type="text" /></div>
        <h4>{{ t('pipeline.steps') }}</h4>
        <div v-for="s in selected.steps" :key="s.id" class="step mono">
          <span class="badge b-dim">{{ s.type }}</span> {{ s.type === 'shell' ? s.command : (s.submodules ? 'git +submodules' : 'svn/git checkout') }}
        </div>
        <button class="btn">+ {{ t('pipeline.newStep') }}</button>
      </aside>
    </div>

    <pre v-if="showJson" class="json mono">{{ JSON.stringify(pipeline, null, 2) }}</pre>
  </div>
</template>

<style scoped>
.variant-a { display: flex; flex-direction: column; gap: 10px; height: calc(100vh - 210px); }
.toolbar { flex-wrap: wrap; }
.spacer { flex: 1; }
.canvas-wrap { flex: 1; position: relative; border: 1px solid var(--line); border-radius: 8px; overflow: hidden; background: #fff; }
.canvas-wrap :deep(.vue-flow) { height: 100%; }
.canvas-wrap :deep(.stage-node) { border: none; background: transparent; font-weight: 700; font-size: 13px; color: var(--ink-dim); text-transform: uppercase; letter-spacing: .04em; }
.canvas-wrap :deep(.job-node) { border-radius: 8px; border: 2px solid #cbd5e1; background: #fff; padding: 10px 12px; font-size: 12.5px; width: 190px; white-space: pre-line; cursor: grab; box-shadow: 0 1px 3px rgba(0,0,0,.08); }
.canvas-wrap :deep(.job-node.st-success) { border-color: var(--ok); }
.canvas-wrap :deep(.job-node.st-running) { border-color: var(--accent); }
.canvas-wrap :deep(.job-node.st-failure) { border-color: var(--err); }
.drawer { position: absolute; top: 12px; right: 12px; width: 300px; max-height: calc(100% - 24px); overflow: auto; z-index: 5; }
.f { display: flex; flex-direction: column; gap: 3px; margin-top: 8px; }
.f label { font-size: 11.5px; color: var(--ink-dim); }
.step { font-size: 12px; padding: 3px 0; }
h4 { margin: 12px 0 4px; }
.json { background: #0f172a; color: #d7e0ee; padding: 12px; border-radius: 8px; max-height: 220px; overflow: auto; font-size: 11.5px; }
</style>

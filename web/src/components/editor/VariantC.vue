<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15). Variant C: hybrid. Form-first like B,
// but with a persistent read-only topological "rail" down the left that
// always shows the whole pipeline at a glance; clicking the rail navigates
// the form. No free drag - layout is derived, never hand-arranged.
import { computed, reactive, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { projects, type Stage, type Job } from '../../data/mock'

const { t } = useI18n()
const pipeline = reactive(structuredClone(projects[0].pipelines[0]))
const sel = ref<{ stage: Stage; job: Job } | null>({ stage: pipeline.stages[0], job: pipeline.stages[0].jobs[0] })
const showJson = ref(false)

const cols = computed(() => pipeline.stages.map((st) => ({ stage: st, jobs: st.jobs })))
</script>

<template>
  <div class="variant-c">
    <div class="toolbar row">
      <span class="dim mono">{{ pipeline.name }} · rev {{ pipeline.rev }} -> {{ pipeline.rev + 1 }}</span>
      <span class="spacer" />
      <button class="btn" @click="showJson = !showJson">JSON +</button>
      <button class="btn primary">{{ t('common.save') }}</button>
    </div>

    <div class="cols">
      <!-- derived rail: stage columns with job chips -->
      <div v-for="(c, ci) in cols" :key="c.stage.id" class="col">
        <div class="col-head">
          <span class="idx">{{ ci + 1 }}</span> {{ c.stage.name }}
          <code v-if="c.stage.when" class="dim mono">?{{ c.stage.when }}</code>
        </div>
        <div class="chips">
          <div
            v-for="j in c.jobs"
            :key="j.id"
            class="chip"
            :class="{ on: sel?.job.id === j.id, [j.status]: true }"
            @click="sel = { stage: c.stage, job: j }"
          >
            <div class="chip-name">{{ j.name }}</div>
            <div class="chip-meta">{{ j.containerImage ? '🐳' : '🖥' }} {{ j.steps.length }}{{ j.retry ? ` ↻${j.retry}` : '' }}{{ j.allowFailure ? ' ~' : '' }}</div>
          </div>
          <button class="chip add" @click="c.stage.jobs.push({ id: `n${Math.random().toString(36).slice(2, 5)}`, name: 'new-job', labels: ['linux'], env: {}, secrets: [], steps: [], status: 'skipped' } as Job)">+</button>
        </div>
        <div v-if="ci === cols.length - 1" class="col-foot">
          <button class="btn slim">+ {{ t('pipeline.newStage') }}</button>
        </div>
      </div>
    </div>

    <div v-if="sel" class="editor card">
      <div class="row" style="justify-content: space-between">
        <h2 style="margin: 0">{{ sel.stage.name }} → {{ sel.job.name }}</h2>
        <button class="btn danger">{{ t('common.delete') }}</button>
      </div>
      <div class="sections">
        <section>
          <h3>{{ t('pipeline.jobs') }} · 基本</h3>
          <div class="grid2">
            <div class="f"><label>{{ t('common.name') }}</label><input v-model="sel.job.name" type="text" /></div>
            <div class="f"><label>{{ t('pipeline.labels') }}</label><input :value="sel.job.labels.join(', ')" type="text" /></div>
            <div class="f"><label>{{ t('common.settings') }} · 执行环境</label>
              <select v-model="sel.job.containerImage">
                <option :value="undefined">🖥 {{ t('pipeline.hostRun') }}</option>
                <option value="rust:1.83">rust:1.83</option>
                <option value="node:22">node:22</option>
              </select>
            </div>
            <div class="f"><label>{{ t('pipeline.when') }}</label><input v-model="sel.job.when" type="text" /></div>
          </div>
        </section>
        <section>
          <h3>{{ t('pipeline.steps') }}</h3>
          <div v-for="s in sel.job.steps" :key="s.id" class="step mono">
            <span class="badge b-dim">{{ s.type }}</span>
            <span class="grow">{{ s.type === 'shell' ? s.command : `checkout${s.submodules ? ' +submodules' : ''}` }}</span>
          </div>
          <div class="row">
            <button class="btn slim">+ {{ t('pipeline.shell') }}</button>
            <button class="btn slim">+ {{ t('pipeline.checkout') }}</button>
          </div>
        </section>
        <section>
          <h3>{{ t('pipeline.env') }} / {{ t('pipeline.secrets') }} / {{ t('pipeline.artifactUpload') }} / {{ t('pipeline.cache') }}</h3>
          <div class="mini-grid">
            <div class="f"><label>env</label><input value="CARGO_TERM_COLOR=always" type="text" class="mono" /></div>
            <div class="f"><label>{{ t('pipeline.secrets') }}</label><input :value="sel.job.secrets.join(', ')" type="text" class="mono" /></div>
            <div class="f"><label>{{ t('pipeline.artifactUpload') }}</label><input :value="(sel.job.artifacts ?? []).join(', ')" type="text" class="mono" /></div>
            <div class="f"><label>{{ t('pipeline.cache') }}</label><input :value="sel.job.caches?.[0]?.key ?? ''" type="text" class="mono" /></div>
            <div class="f"><label>{{ t('pipeline.retry') }} / {{ t('pipeline.timeout') }}</label><input :value="`${sel.job.retry ?? 0} / ${sel.job.timeoutMin ?? '∞'}`" type="text" class="mono" /></div>
            <div class="f"><label>{{ t('pipeline.allowFailure') }}</label><input v-model="sel.job.allowFailure" type="checkbox" style="width: auto" /></div>
          </div>
        </section>
      </div>
    </div>

    <pre v-if="showJson" class="json mono">{{ JSON.stringify(pipeline, null, 2) }}</pre>
  </div>
</template>

<style scoped>
.variant-c { display: flex; flex-direction: column; gap: 10px; }
.spacer { flex: 1; }
.dim { color: var(--ink-dim); }
.cols { display: flex; gap: 18px; overflow-x: auto; padding: 12px; background: var(--panel); border: 1px solid var(--line); border-radius: 8px; }
.col { min-width: 170px; }
.col-head { font-weight: 700; font-size: 12.5px; display: flex; gap: 6px; align-items: center; margin-bottom: 8px; }
.idx { background: var(--ink); color: #fff; width: 18px; height: 18px; border-radius: 50%; display: inline-flex; align-items: center; justify-content: center; font-size: 11px; }
.chips { display: flex; flex-direction: column; gap: 6px; }
.chip { border: 2px solid var(--line); border-radius: 8px; padding: 7px 9px; cursor: pointer; background: #fff; }
.chip:hover { border-color: #94a3b8; }
.chip.on { border-color: var(--accent); box-shadow: 0 0 0 3px #dbeafe; }
.chip.st-success { border-left: 4px solid var(--ok); }
.chip.st-failure { border-left: 4px solid var(--err); }
.chip.st-running { border-left: 4px solid var(--accent); }
.chip-name { font-weight: 600; font-size: 12.5px; }
.chip-meta { font-size: 11px; color: var(--ink-dim); margin-top: 2px; }
.chip.add { border-style: dashed; text-align: center; color: var(--ink-dim); }
.col-foot { margin-top: 8px; }
.btn.slim { padding: 3px 8px; font-size: 12px; }
.editor { margin-top: 2px; }
.sections section { margin-top: 14px; }
h3 { font-size: 12px; color: var(--ink-dim); text-transform: uppercase; letter-spacing: .05em; margin: 0 0 8px; }
.grid2 { display: grid; grid-template-columns: 1fr 1fr; gap: 8px 14px; }
.mini-grid { display: grid; grid-template-columns: repeat(3, 1fr); gap: 8px 14px; }
.f { display: flex; flex-direction: column; gap: 3px; }
.f label { font-size: 11px; color: var(--ink-dim); }
.step { display: flex; gap: 8px; align-items: center; padding: 4px 0; font-size: 12.5px; }
.grow { flex: 1; }
.json { background: #0f172a; color: #d7e0ee; padding: 12px; border-radius: 8px; max-height: 220px; overflow: auto; font-size: 11.5px; }
</style>

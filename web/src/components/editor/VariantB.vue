<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15). Variant B: structured form editor.
// Tree master-detail, zero canvas: stage list -> job accordion -> step rows.
// Every attribute is an explicit labelled field, like a settings screen.
import { computed, reactive, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { projects, type Job } from '../../data/mock'

const { t } = useI18n()
const pipeline = reactive(structuredClone(projects[0].pipelines[0]))
const openJob = ref<string | null>(pipeline.stages[0]?.jobs[0]?.id ?? null)
const showJson = ref(false)
const tab = ref<'jobs' | 'params' | 'env'>('jobs')

const flat = computed(() => pipeline.stages.flatMap((s) => s.jobs))
</script>

<template>
  <div class="variant-b">
    <div class="toolbar row">
      <span class="dim mono">rev {{ pipeline.rev }} -> {{ pipeline.rev + 1 }}</span>
      <span class="spacer" />
      <button class="btn" @click="showJson = !showJson">JSON +</button>
      <button class="btn primary">{{ t('common.save') }}</button>
    </div>

    <div class="tabs">
      <button :class="{ on: tab === 'jobs' }" @click="tab = 'jobs'">{{ t('pipeline.jobs') }} ({{ flat.length }})</button>
      <button :class="{ on: tab === 'params' }" @click="tab = 'params'">{{ t('pipeline.params') }} ({{ pipeline.params.length }})</button>
      <button :class="{ on: tab === 'env' }" @click="tab = 'env'">{{ t('pipeline.env') }}</button>
    </div>

    <!-- JOBS tab: outline + detail -->
    <div v-if="tab === 'jobs'" class="split">
      <div class="outline card">
        <template v-for="st in pipeline.stages" :key="st.id">
          <div class="stage-head">
            <b>{{ st.name }}</b>
            <code v-if="st.when" class="mono dim">when: {{ st.when }}</code>
          </div>
          <div
            v-for="j in st.jobs"
            :key="j.id"
            class="outline-job"
            :class="{ on: j.id === openJob }"
            @click="openJob = j.id"
          >
            <span class="badge b-dim">{{ j.containerImage ? '🐳' : '🖥' }}</span>
            <span>{{ j.name }}</span>
            <span class="mono dim">{{ j.steps.length }} {{ t('pipeline.steps') }}</span>
          </div>
          <button class="btn slim">+ {{ t('pipeline.newJob') }}</button>
        </template>
        <button class="btn slim">+ {{ t('pipeline.newStage') }}</button>
      </div>

      <div class="detail card">
        <template v-for="st in pipeline.stages" :key="st.id">
          <template v-for="j in st.jobs" :key="j.id">
            <div v-if="j.id === openJob" class="jobform">
              <h2 style="margin-top: 0">{{ st.name }} / {{ j.name }}</h2>
              <div class="grid2">
                <div class="f"><label>{{ t('common.name') }}</label><input v-model="j.name" type="text" /></div>
                <div class="f"><label>{{ t('pipeline.labels') }}（AND 匹配）</label><input :value="j.labels.join(', ')" type="text" /></div>
                <div class="f"><label>{{ t('pipeline.containerImage') }}</label>
                  <select v-model="j.containerImage">
                    <option :value="undefined">🖥 {{ t('pipeline.hostRun') }}（默认）</option>
                    <option value="rust:1.83">rust:1.83</option>
                    <option value="node:22">node:22</option>
                  </select>
                </div>
                <div class="f"><label>{{ t('pipeline.when') }}</label><input v-model="j.when" type="text" placeholder="e.g. ${profile} == 'release'" /></div>
                <div class="f"><label>{{ t('pipeline.retry') }}（次数）</label><input v-model.number="j.retry" type="text" /></div>
                <div class="f"><label>{{ t('pipeline.timeout') }}（{{ t('common.min') }}，默认不限）</label><input v-model.number="j.timeoutMin" type="text" /></div>
                <div class="f"><label>{{ t('pipeline.secrets') }}（按名引用）</label><input :value="j.secrets.join(', ')" type="text" /></div>
                <div class="f"><label>{{ t('pipeline.artifactUpload') }}（路径）</label><input :value="(j.artifacts ?? []).join(', ')" type="text" /></div>
                <div class="f"><label>{{ t('pipeline.cache') }} key</label><input :value="j.caches?.[0]?.key ?? ''" type="text" /></div>
                <div class="f"><label>{{ t('pipeline.allowFailure') }}</label><input v-model="j.allowFailure" type="checkbox" style="width: auto" /></div>
              </div>

              <h3>{{ t('pipeline.env') }}</h3>
              <div class="kv mono"><code>CARGO_TERM_COLOR</code><input value="always" type="text" /></div>

              <h3>{{ t('pipeline.steps') }}</h3>
              <div v-for="s in j.steps" :key="s.id" class="steprow">
                <span class="badge b-dim">{{ s.type }}</span>
                <input v-if="s.type === 'shell'" v-model="s.command" type="text" class="mono grow" />
                <span v-else class="mono dim">checkout · {{ j.containerImage ? '容器内（镜像前置 git）' : '宿主机 git/svn' }} · submodules: {{ s.submodules ? 'on' : 'off' }}</span>
                <button class="btn danger">✕</button>
              </div>
              <div class="row">
                <button class="btn">+ {{ t('pipeline.shell') }}</button>
                <button class="btn">+ {{ t('pipeline.checkout') }}</button>
              </div>
            </div>
          </template>
        </template>
      </div>
    </div>

    <!-- PARAMS tab -->
    <div v-else-if="tab === 'params'" class="card">
      <table class="tbl" style="border: none">
        <thead><tr><th>{{ t('common.name') }}</th><th>type</th><th>default</th><th>required</th><th></th></tr></thead>
        <tbody>
          <tr v-for="p in pipeline.params" :key="p.name">
            <td><input v-model="p.name" type="text" /></td>
            <td><select v-model="p.type"><option>string</option><option>number</option><option>bool</option><option>enum</option></select></td>
            <td><input v-model="p.default" type="text" class="mono" /></td>
            <td><input v-model="p.required" type="checkbox" /></td>
            <td><button class="btn danger">✕</button></td>
          </tr>
        </tbody>
      </table>
      <button class="btn">+ {{ t('pipeline.params') }}</button>
      <div class="hint">必填参数必须带默认值；任何触发方式一律「默认值，手动触发可覆盖」</div>
    </div>

    <!-- ENV tab -->
    <div v-else class="card">
      <div class="kv mono"><input value="RUSTFLAGS" type="text" /><input value="--deny warnings" type="text" /></div>
      <div class="hint">Pipeline 级 env，任务级可覆盖同名项；与 ${} 变量替换是两回事</div>
    </div>

    <pre v-if="showJson" class="json mono">{{ JSON.stringify(pipeline, null, 2) }}</pre>
  </div>
</template>

<style scoped>
.variant-b { display: flex; flex-direction: column; gap: 10px; }
.spacer { flex: 1; }
.dim { color: var(--ink-dim); }
.tabs { display: flex; gap: 4px; border-bottom: 2px solid var(--line); }
.tabs button { border: none; background: none; padding: 8px 14px; font-weight: 600; color: var(--ink-dim); border-bottom: 2px solid transparent; margin-bottom: -2px; }
.tabs button.on { color: var(--accent); border-color: var(--accent); }
.split { display: grid; grid-template-columns: 260px 1fr; gap: 12px; align-items: start; }
.outline { display: flex; flex-direction: column; gap: 4px; }
.stage-head { display: flex; justify-content: space-between; margin-top: 8px; padding: 2px 4px; }
.outline-job { display: flex; gap: 8px; align-items: center; padding: 7px 8px; border-radius: 6px; cursor: pointer; border: 1px solid transparent; }
.outline-job:hover { background: #f1f5f9; }
.outline-job.on { border-color: var(--accent); background: #eff6ff; }
.outline-job .dim { margin-left: auto; font-size: 11px; }
.btn.slim { padding: 3px 8px; font-size: 12px; align-self: flex-start; }
.grid2 { display: grid; grid-template-columns: 1fr 1fr; gap: 8px 14px; }
.f { display: flex; flex-direction: column; gap: 3px; }
.f label { font-size: 11.5px; color: var(--ink-dim); }
.kv { display: grid; grid-template-columns: 200px 1fr; gap: 8px; margin-bottom: 8px; align-items: center; }
.steprow { display: flex; gap: 8px; align-items: center; margin-bottom: 8px; }
.grow { flex: 1; }
h3 { font-size: 13px; margin: 16px 0 8px; color: var(--ink-dim); }
.hint { font-size: 12px; color: var(--ink-dim); margin-top: 8px; }
.json { background: #0f172a; color: #d7e0ee; padding: 12px; border-radius: 8px; max-height: 220px; overflow: auto; font-size: 11.5px; }
</style>

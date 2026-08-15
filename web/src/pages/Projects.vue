<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15)
import { ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { projects } from '../data/mock'

const { t } = useI18n()
const showNew = ref(false)
const connState = ref<'idle' | 'ok' | 'fail'>('idle')
const branchList = ref<string[]>([])

function testConn() {
  connState.value = 'ok'
  branchList.value = ['main', 'develop', 'release/1.0']
}
</script>

<template>
  <div class="row" style="justify-content: space-between">
    <h1>{{ t('projects.title') }}</h1>
    <button class="btn primary" @click="showNew = !showNew">{{ t('projects.newProject') }}</button>
  </div>

  <div v-if="showNew" class="card newproj">
    <div class="form-row">
      <label>{{ t('common.name') }}</label>
      <input type="text" placeholder="my-project" />
    </div>
    <div class="form-row">
      <label>{{ t('projects.scmType') }}</label>
      <select><option>git</option><option>svn</option></select>
    </div>
    <div class="form-row">
      <label>{{ t('projects.repoUrl') }}</label>
      <input type="text" style="flex: 1" placeholder="https://github.com/org/repo.git" />
      <button class="btn" @click="testConn">{{ t('projects.testConn') }}</button>
      <span v-if="connState === 'ok'" style="color: var(--ok)">✓ {{ t('projects.testConnOk') }} · main / develop / release/1.0</span>
    </div>
    <div class="form-row" v-if="connState === 'ok'">
      <label>{{ t('projects.defaultBranch') }}</label>
      <select><option>main</option><option>develop</option><option>release/1.0</option></select>
    </div>
    <div class="form-row">
      <label>{{ t('projects.credentials') }}</label>
      <select><option>—</option><option>user/pass</option><option>token</option></select>
      <span class="hint">凭据经 GIT_ASKPASS 递送，不上命令行</span>
    </div>
  </div>

  <table class="tbl">
    <thead>
      <tr>
        <th>{{ t('common.name') }}</th>
        <th>{{ t('projects.scmType') }}</th>
        <th>{{ t('projects.repoUrl') }}</th>
        <th>{{ t('projects.defaultBranch') }}</th>
        <th>Pipeline</th>
      </tr>
    </thead>
    <tbody>
      <tr v-for="p in projects" :key="p.id">
        <td><RouterLink :to="`/projects/${p.id}`" style="font-weight: 600">{{ p.name }}</RouterLink></td>
        <td><span class="badge b-dim">{{ p.scm }}</span></td>
        <td class="mono">{{ p.repoUrl }}</td>
        <td>{{ p.defaultBranch ?? '—' }}</td>
        <td>{{ p.pipelines.length }} {{ t('projects.pipelines') }}</td>
      </tr>
    </tbody>
  </table>
</template>

<style scoped>
.newproj { margin-bottom: 16px; display: flex; flex-direction: column; gap: 10px; }
.form-row { display: flex; gap: 10px; align-items: center; }
.form-row label { width: 90px; color: var(--ink-dim); font-size: 13px; }
.hint { font-size: 12px; color: var(--ink-dim); }
</style>

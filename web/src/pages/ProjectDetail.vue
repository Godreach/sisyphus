<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15)
import { computed } from 'vue'
import { useRoute } from 'vue-router'
import { useI18n } from 'vue-i18n'
import { projects, builds } from '../data/mock'

const { t } = useI18n()
const route = useRoute()
const project = computed(() => projects.find((p) => p.id === route.params.id) ?? projects[0])
const projectBuilds = computed(() => builds.filter((b) => b.projectId === project.value.id))

function badgeClass(s: string) {
  return { success: 'b-ok', failure: 'b-err', running: 'b-run', queued: 'b-dim', cancelled: 'b-warn', unknown: 'b-unknown' }[s] ?? 'b-dim'
}
</script>

<template>
  <h1 class="mono">{{ project.name }}</h1>
  <div class="card meta">
    <div><b>{{ t('projects.repoUrl') }}:</b> <span class="mono">{{ project.repoUrl }}</span></div>
    <div v-if="project.defaultBranch"><b>{{ t('projects.defaultBranch') }}:</b> {{ project.defaultBranch }}</div>
    <div v-if="project.scm === 'svn'"><b>revision:</b> {{ project.revision }}</div>
  </div>

  <h2>{{ t('nav.projects') }} · Pipeline</h2>
  <table class="tbl">
    <thead><tr><th>{{ t('common.name') }}</th><th>{{ t('pipeline.revision') }}</th><th>{{ t('pipeline.stages') }}</th><th></th></tr></thead>
    <tbody>
      <tr v-for="pl in project.pipelines" :key="pl.id">
        <td style="font-weight: 600">{{ pl.name }}</td>
        <td>rev {{ pl.rev }}</td>
        <td>{{ pl.stages.map((s) => s.name).join(' → ') }}</td>
        <td class="row">
          <button class="btn primary">{{ t('common.run') }}</button>
          <RouterLink :to="`/pipelines/${pl.id}/edit`"><button class="btn">{{ t('common.edit') }}</button></RouterLink>
        </td>
      </tr>
    </tbody>
  </table>

  <h2>{{ t('overview.recentBuilds') }}</h2>
  <table class="tbl">
    <thead><tr><th>#</th><th>{{ t('common.status') }}</th><th>{{ t('common.trigger') }}</th><th>commit</th><th>{{ t('common.time') }}</th><th></th></tr></thead>
    <tbody>
      <tr v-for="b in projectBuilds" :key="b.id">
        <td><RouterLink :to="`/builds/${b.id}`" class="mono">#{{ b.number }}</RouterLink></td>
        <td><span class="badge" :class="badgeClass(b.status)">{{ t(`builds.${b.status}`) }}</span></td>
        <td>{{ b.triggeredBy }}</td>
        <td class="mono">{{ b.commit }}</td>
        <td>{{ b.startedAt }}</td>
        <td class="row">
          <button class="btn">{{ t('common.rerunAll') }}</button>
          <button v-if="b.status === 'failure'" class="btn">{{ t('common.rerunFailed') }}</button>
        </td>
      </tr>
    </tbody>
  </table>
</template>

<style scoped>
.meta { display: flex; gap: 24px; margin-bottom: 4px; font-size: 13px; }
</style>

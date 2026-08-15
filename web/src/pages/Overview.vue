<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15)
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import { agents, builds } from '../data/mock'

const { t } = useI18n()

const online = computed(() => agents.filter((a) => a.state === 'online').length)
const offline = computed(() => agents.filter((a) => a.state === 'offline').length)
const queueDepth = computed(() => 2)
const running = computed(() => builds.filter((b) => b.status === 'running').length)
const diskWarn = computed(() => agents.filter((a) => a.diskFreeGb / a.diskTotalGb < 0.15).length)
const noMatch = computed(() => 1)

const stats = computed(() => [
  { label: t('overview.statAgents'), value: `${online.value}/${agents.length}`, warn: offline.value > 0, warnText: t('overview.warnAgentsOffline', { n: offline.value }) },
  { label: t('overview.statQueue'), value: String(queueDepth.value), warn: false },
  { label: t('overview.statRunning'), value: String(running.value), warn: false },
  { label: t('overview.statDisk'), value: String(diskWarn.value), warn: diskWarn.value > 0, warnText: t('overview.warnDisk', { n: diskWarn.value }) },
])

const warnings = computed(() => {
  const w = []
  if (offline.value) w.push(t('overview.warnAgentsOffline', { n: offline.value }))
  if (noMatch.value) w.push(t('overview.warnNoMatch', { n: noMatch.value }))
  if (diskWarn.value) w.push(t('overview.warnDisk', { n: diskWarn.value }))
  return w
})

function badgeClass(s: string) {
  return { success: 'b-ok', failure: 'b-err', running: 'b-run', queued: 'b-dim', cancelled: 'b-warn', unknown: 'b-unknown' }[s] ?? 'b-dim'
}
</script>

<template>
  <h1>{{ t('overview.title') }}</h1>

  <div class="stats">
    <div v-for="s in stats" :key="s.label" class="card stat" :class="{ warn: s.warn }">
      <div class="stat-label">{{ s.label }}</div>
      <div class="stat-value">{{ s.value }}</div>
    </div>
  </div>

  <div v-if="warnings.length" class="card warnline">
    <div v-for="w in warnings" :key="w" class="warn-item">⚠ {{ w }}</div>
  </div>
  <div v-else class="card okline">✓ {{ t('overview.allGood') }}</div>

  <h2>{{ t('overview.recentBuilds') }}</h2>
  <table class="tbl">
    <thead>
      <tr>
        <th>{{ t('common.name') }}</th>
        <th>{{ t('common.status') }}</th>
        <th>{{ t('common.trigger') }}</th>
        <th>{{ t('common.time') }}</th>
        <th>{{ t('common.duration') }}</th>
      </tr>
    </thead>
    <tbody>
      <tr v-for="b in builds" :key="b.id">
        <td>
          <RouterLink :to="`/builds/${b.id}`" class="mono">
            sisyphus / main-ci #{{ b.number }}<span v-if="b.attempt > 1"> · {{ t('builds.attempt', { n: b.attempt }) }}</span>
          </RouterLink>
        </td>
        <td><span class="badge" :class="badgeClass(b.status)">{{ t(`builds.${b.status}`) }}</span></td>
        <td>{{ b.triggeredBy }}</td>
        <td>{{ b.startedAt }}</td>
        <td>{{ b.durationSec ? `${Math.floor(b.durationSec / 60)}m${b.durationSec % 60}s` : '—' }}</td>
      </tr>
    </tbody>
  </table>
</template>

<style scoped>
.stats { display: grid; grid-template-columns: repeat(4, 1fr); gap: 12px; margin-bottom: 14px; }
.stat-label { font-size: 12px; color: var(--ink-dim); }
.stat-value { font-size: 26px; font-weight: 700; margin-top: 4px; }
.stat.warn .stat-value { color: var(--warn); }
.warnline { border-color: #fcd34d; background: #fffbeb; margin-bottom: 8px; }
.warn-item { color: #92400e; padding: 2px 0; }
.okline { color: var(--ok); margin-bottom: 8px; }
</style>

<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15)
import { ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { agents } from '../data/mock'

const { t } = useI18n()
const showRegister = ref(false)

function stateBadge(s: string) {
  return { online: 'b-ok', offline: 'b-err', draining: 'b-warn', incompatible: 'b-unknown' }[s] ?? 'b-dim'
}
</script>

<template>
  <div class="row" style="justify-content: space-between">
    <h1>{{ t('agents.title') }} <span class="dim">{{ agents.filter((a) => a.state === 'online').length }}/{{ agents.length }}</span></h1>
    <button class="btn primary" @click="showRegister = !showRegister">{{ t('agents.register') }}</button>
  </div>

  <div v-if="showRegister" class="card reg">
    <div class="mono cmd">
      sisyphus-agent --server https://ci.example.com --registration-code <b>SISAR-9f2K-...-x7Qd</b>
    </div>
    <div class="hint">{{ t('agents.regCode') }} · 一次性使用 · 换取长期 per-Agent token（sisa_ 前缀）</div>
  </div>

  <table class="tbl">
    <thead>
      <tr>
        <th>{{ t('common.name') }}</th>
        <th>{{ t('common.status') }}</th>
        <th>{{ t('agents.version') }}</th>
        <th>{{ t('agents.platform') }}</th>
        <th>{{ t('agents.slots') }}</th>
        <th>{{ t('agents.labels') }}</th>
        <th>{{ t('agents.diskUsage') }}</th>
      </tr>
    </thead>
    <tbody>
      <tr v-for="a in agents" :key="a.id">
        <td><RouterLink :to="`/agents/${a.id}`" style="font-weight: 600" class="mono">{{ a.name }}</RouterLink></td>
        <td><span class="badge" :class="stateBadge(a.state)">{{ t(`agents.${a.state}`) }}</span></td>
        <td class="mono">{{ a.version }}</td>
        <td>{{ a.platform }}</td>
        <td>{{ a.slotsUsed }}/{{ a.slotsTotal }}</td>
        <td class="labels">
          <span v-for="l in a.systemLabels" :key="l" class="lab sys">{{ l }}</span>
          <span v-for="l in a.customLabels" :key="l" class="lab">{{ l }}</span>
        </td>
        <td>
          <div class="disk" :class="{ low: a.diskFreeGb / a.diskTotalGb < 0.15 }">
            <div class="bar"><div class="fill" :style="{ width: `${100 - (a.diskFreeGb / a.diskTotalGb) * 100}%` }"></div></div>
            <span class="mono">{{ a.diskFreeGb }}G 可用</span>
          </div>
        </td>
      </tr>
    </tbody>
  </table>
</template>

<style scoped>
.dim { color: var(--ink-dim); font-weight: 400; font-size: 14px; }
.reg { margin-bottom: 14px; }
.cmd { background: #0f172a; color: #d7e0ee; border-radius: 6px; padding: 10px 12px; font-size: 12.5px; }
.hint { font-size: 12px; color: var(--ink-dim); margin-top: 6px; }
.labels { display: flex; gap: 4px; flex-wrap: wrap; }
.lab { font-size: 11px; border: 1px solid var(--line); border-radius: 4px; padding: 0 5px; color: var(--ink-dim); }
.lab.sys { background: #eef2ff; border-color: #c7d2fe; color: #4338ca; }
.disk { display: flex; align-items: center; gap: 8px; }
.disk.low .fill { background: var(--err); }
.bar { width: 60px; height: 6px; border-radius: 3px; background: #e5e7eb; overflow: hidden; }
.fill { height: 100%; background: var(--ok); }
</style>

<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15)
import { computed, ref } from 'vue'
import { useRoute } from 'vue-router'
import { useI18n } from 'vue-i18n'
import { agents } from '../data/mock'

const { t } = useI18n()
const route = useRoute()
const agent = computed(() => agents.find((a) => a.id === route.params.id) ?? agents[0])
const newLabel = ref('')

function stateBadge(s: string) {
  return { online: 'b-ok', offline: 'b-err', draining: 'b-warn', incompatible: 'b-unknown' }[s] ?? 'b-dim'
}
</script>

<template>
  <div class="row" style="justify-content: space-between">
    <h1 class="mono">{{ agent.name }}</h1>
    <div class="row">
      <button v-if="agent.state !== 'incompatible'" class="btn">{{ t('agents.upgradeOne') }} -> 1.0.3</button>
      <button class="btn danger">{{ t('agents.disable') }}</button>
    </div>
  </div>

  <div class="grid">
    <div class="card">
      <div class="row" style="margin-bottom: 10px">
        <span class="badge" :class="stateBadge(agent.state)">{{ t(`agents.${agent.state}`) }}</span>
        <span class="dim">{{ t('agents.versionCurrent') }} <b class="mono">{{ agent.version }}</b></span>
      </div>
      <div class="kv"><span>{{ t('agents.platform') }}</span><b>{{ agent.platform }}</b></div>
      <div class="kv"><span>{{ t('agents.slots') }}</span><b>{{ agent.slotsUsed }} / {{ agent.slotsTotal }}</b></div>
      <div class="kv"><span>last seen</span><b>{{ agent.lastSeen }}</b></div>
      <div v-if="agent.state === 'draining'" class="drain">⏳ 排空中：停接新任务，等待 1 个运行中任务终态后自动换入 1.0.3</div>
      <div v-if="agent.state === 'incompatible'" class="drain">⛔ 窗口外（0.9.7 < 1.0.2）：任务面拒连，升级面保留</div>
    </div>

    <div class="card">
      <h2 style="margin-top: 0">{{ t('agents.diskUsage') }}</h2>
      <div class="kv"><span>卷剩余 / 总量</span><b class="mono">{{ agent.diskFreeGb }} G / {{ agent.diskTotalGb }} G</b></div>
      <div class="kv"><span>{{ t('agents.cacheUsage') }}（记账）</span><b class="mono">{{ agent.cacheGb }} G</b></div>
      <div class="kv"><span>{{ t('agents.workspaceUsage') }}（10min 采样）</span><b class="mono">{{ agent.workspaceGb }} G</b></div>
      <div class="bar"><div class="fill" :style="{ width: `${100 - (agent.diskFreeGb / agent.diskTotalGb) * 100}%` }"></div></div>
    </div>

    <div class="card">
      <h2 style="margin-top: 0">{{ t('agents.labels') }}</h2>
      <div class="lgroup">{{ t('agents.systemLabels') }}（不可手编）</div>
      <div class="labels">
        <span v-for="l in agent.systemLabels" :key="l" class="lab sys">{{ l }}</span>
      </div>
      <div class="lgroup">{{ t('agents.customLabels') }}</div>
      <div class="labels">
        <span v-for="l in agent.customLabels" :key="l" class="lab">{{ l }} ✕</span>
        <input v-model="newLabel" type="text" style="width: 90px" placeholder="+ rust" />
      </div>
    </div>

    <div class="card">
      <h2 style="margin-top: 0">工作区 & 缓存</h2>
      <table class="tbl" style="border: none">
        <thead><tr><th>pipeline / job</th><th>占用</th><th></th></tr></thead>
        <tbody>
          <tr><td class="mono">sisyphus / build-linux</td><td>12.1 G</td><td><button class="btn danger">{{ t('agents.cleanupWorkspace') }}</button></td></tr>
          <tr><td class="mono">sisyphus / test</td><td>7.5 G</td><td><button class="btn danger">{{ t('agents.cleanupWorkspace') }}</button></td></tr>
          <tr><td class="mono">cache: cargo-sisyphus</td><td>18.2 G</td><td><button class="btn danger">{{ t('agents.clearCache') }}</button></td></tr>
        </tbody>
      </table>
    </div>
  </div>
</template>

<style scoped>
.grid { display: grid; grid-template-columns: repeat(2, 1fr); gap: 12px; align-items: start; }
.kv { display: flex; justify-content: space-between; padding: 5px 0; border-bottom: 1px dashed var(--line); font-size: 13px; }
.kv span { color: var(--ink-dim); }
.dim { color: var(--ink-dim); font-size: 13px; }
.drain { margin-top: 10px; font-size: 12.5px; background: #fffbeb; color: #92400e; border-radius: 6px; padding: 8px 10px; }
.bar { height: 8px; border-radius: 4px; background: #e5e7eb; overflow: hidden; margin-top: 10px; }
.fill { height: 100%; background: var(--ok); }
.lgroup { font-size: 12px; color: var(--ink-dim); margin: 8px 0 4px; }
.labels { display: flex; gap: 4px; flex-wrap: wrap; align-items: center; }
.lab { font-size: 12px; border: 1px solid var(--line); border-radius: 4px; padding: 1px 6px; color: var(--ink-dim); }
.lab.sys { background: #eef2ff; border-color: #c7d2fe; color: #4338ca; }
</style>

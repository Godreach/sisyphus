<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15)
import { useI18n } from 'vue-i18n'
import { agents, agentUpgrade } from '../data/mock'

const { t } = useI18n()

function stateBadge(s: string) {
  return { online: 'b-ok', offline: 'b-err', draining: 'b-warn', incompatible: 'b-unknown' }[s] ?? 'b-dim'
}
</script>

<template>
  <h1>{{ t('nav.upgrade') }}</h1>

  <div class="cards">
    <div class="card">
      <div class="k">{{ t('agents.versionCurrent') }}（Server）</div>
      <div class="v mono">{{ agentUpgrade.serverVersion }}</div>
      <div class="sub">兼容窗口 {{ agentUpgrade.compatWindow }}</div>
    </div>
    <div class="card">
      <div class="k">{{ t('agents.versionAvail') }}（已上传包）</div>
      <div class="v mono">{{ agentUpgrade.uploadedVersion }} <span class="sub">· {{ agentUpgrade.uploadedAt }} 上传 · sha256 ✓</span></div>
      <div class="row">
        <button class="btn">{{ t('common.edit') }}（上传新包）</button>
        <button class="btn primary">{{ t('agents.upgradeAll') }}</button>
      </div>
    </div>
  </div>

  <table class="tbl">
    <thead><tr><th>{{ t('common.name') }}</th><th>{{ t('common.status') }}</th><th>{{ t('agents.version') }}</th><th>升级动作</th><th>排空</th></tr></thead>
    <tbody>
      <tr v-for="a in agents" :key="a.id">
        <td class="mono">{{ a.name }}</td>
        <td><span class="badge" :class="stateBadge(a.state)">{{ t(`agents.${a.state}`) }}</span></td>
        <td class="mono">
          {{ a.version }}
          <span v-if="a.version !== agentUpgrade.uploadedVersion" class="up">-> {{ agentUpgrade.uploadedVersion }}</span>
        </td>
        <td>
          <button v-if="a.version !== agentUpgrade.uploadedVersion" class="btn">{{ t('agents.upgradeOne') }}</button>
          <span v-else class="dim">✓</span>
        </td>
        <td class="dim">{{ a.state === 'draining' ? '1 个任务运行中' : '-' }}</td>
      </tr>
    </tbody>
  </table>
  <div class="hint">指令持久化、离线补发；排空 = 停接新任务 + 全部终态后原子换入；旧二进制留 .old，3 次启动失败自动退回</div>
</template>

<style scoped>
.cards { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; margin-bottom: 14px; }
.k { font-size: 12px; color: var(--ink-dim); }
.v { font-size: 22px; font-weight: 700; margin: 4px 0; }
.sub { font-size: 12px; color: var(--ink-dim); }
.up { color: var(--accent); }
.hint { font-size: 12px; color: var(--ink-dim); margin-top: 10px; }
.dim { color: var(--ink-dim); }
</style>

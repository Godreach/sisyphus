<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15)
import { useI18n } from 'vue-i18n'
import { auditEntries } from '../data/mock'

const { t } = useI18n()
</script>

<template>
  <h1>{{ t('audit.title') }}</h1>
  <div class="hint">{{ t('audit.onlyAdmin') }} · 只录安全事件 · 永久保留 · 机密只记名不记值</div>

  <table class="tbl">
    <thead><tr><th>{{ t('common.time') }}</th><th>{{ t('common.operator') }}</th><th>{{ t('audit.event') }}</th><th>detail</th></tr></thead>
    <tbody>
      <tr v-for="(e, i) in auditEntries" :key="i">
        <td class="mono">{{ e.time }}</td>
        <td>{{ e.operator }}</td>
        <td><span class="badge" :class="e.event.includes('失败') ? 'b-err' : 'b-dim'">{{ e.event }}</span></td>
        <td class="dim mono">{{ e.detail }}</td>
      </tr>
    </tbody>
  </table>
</template>

<style scoped>
.hint { font-size: 12px; color: var(--ink-dim); margin: -8px 0 12px; }
.dim { color: var(--ink-dim); }
</style>

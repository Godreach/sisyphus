<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15)
import { ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { secrets } from '../data/mock'

const { t } = useI18n()
const adding = ref(false)
const newValue = ref('')
const flashOnce = ref('')
</script>

<template>
  <div class="row" style="justify-content: space-between">
    <h1>{{ t('secrets.title') }}</h1>
    <button class="btn primary" @click="adding = !adding">{{ t('secrets.setValue') }}</button>
  </div>
  <div class="hint">{{ t('secrets.neverReadable') }} · AEAD 加密落库 · 日志离机前脱敏为 *** · viewer/runner 连名不可见</div>

  <div v-if="adding" class="card add">
    <div class="row">
      <input v-model="newValue" type="text" placeholder="NAME" style="width: 200px" />
      <input type="password" placeholder="value（仅创建时可见）" style="flex: 1" />
      <button class="btn primary" @click="flashOnce = 'sis-...（示例 PAT）已复制'; adding = false">{{ t('common.save') }}</button>
    </div>
    <div v-if="flashOnce" class="flash mono">{{ flashOnce }}</div>
  </div>

  <table class="tbl">
    <thead><tr><th>{{ t('secrets.name') }}</th><th>{{ t('common.time') }}</th><th>{{ t('common.operator') }}</th><th>{{ t('secrets.referenced') }}</th><th></th></tr></thead>
    <tbody>
      <tr v-for="s in secrets" :key="s.name">
        <td class="mono" style="font-weight: 600">{{ s.name }}</td>
        <td>{{ s.updatedAt }}</td>
        <td>{{ s.updatedBy }}</td>
        <td class="dim">{{ s.referencedBy.join(', ') }}</td>
        <td class="row"><button class="btn">{{ t('secrets.setValue') }}</button><button class="btn danger">{{ t('common.delete') }}</button></td>
      </tr>
    </tbody>
  </table>
</template>

<style scoped>
.hint { font-size: 12px; color: var(--ink-dim); margin: -8px 0 12px; }
.add { margin-bottom: 14px; }
.flash { margin-top: 8px; color: var(--ok); font-size: 12.5px; }
.dim { color: var(--ink-dim); font-size: 12.5px; }
</style>

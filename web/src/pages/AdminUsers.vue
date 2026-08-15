<script setup lang="ts">
// PROTOTYPE - throwaway (ticket #15)
import { ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { users } from '../data/mock'

const { t } = useI18n()
const flashOnce = ref('')
</script>

<template>
  <div class="row" style="justify-content: space-between">
    <h1>{{ t('users.title') }}</h1>
    <button class="btn primary">{{ t('common.create') }}</button>
  </div>
  <div class="hint">注册开关默认关 · 账号只禁用不物理删除 · argon2id</div>

  <table class="tbl">
    <thead><tr><th>{{ t('common.name') }}</th><th>{{ t('users.role') }}</th><th></th><th></th></tr></thead>
    <tbody>
      <tr v-for="u in users" :key="u.name">
        <td style="font-weight: 600">{{ u.name }}</td>
        <td>
          <span class="badge" :class="u.role === 'globalAdmin' ? 'b-unknown' : 'b-dim'">
            {{ u.role === 'globalAdmin' ? t('users.globalAdmin') : u.role }}
          </span>
        </td>
        <td>
          <button class="btn" @click="flashOnce = `sis_pat_9f2K…x7Qd（${u.name}，仅此一次可见）`">{{ t('users.newPat') }}</button>
          <span v-if="flashOnce.startsWith(u.name) || flashOnce.includes(u.name)" class="flash mono">{{ flashOnce }}</span>
        </td>
        <td>
          <button class="btn" :class="{ danger: !u.disabled }">{{ u.disabled ? t('agents.enable') : t('users.disable') }}</button>
        </td>
      </tr>
    </tbody>
  </table>
</template>

<style scoped>
.hint { font-size: 12px; color: var(--ink-dim); margin: -8px 0 12px; }
.flash { color: var(--ok); font-size: 12px; margin-left: 6px; }
</style>

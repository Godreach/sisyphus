<script setup lang="ts">
// 应用壳（ADR-0020 IA：底部 zh/EN 即时切换 + 路由出口）。
// 侧栏（概览/项目/Agent/管理 四区）随 12 页 IA 页面票落地；本壳挂路由
// 出口、语言切换与已登录用户的登出动作（B4-T2 登出闭环）。
// 登出后整页回登录页：清 cookie + 清认证态（401 回调的 redirect 不适用，
// 登出是主动离开，不回跳原目标）。

import { computed } from 'vue'
import { useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'

import { currentLocale, setLocale } from '@/i18n'
import { useAuthStore } from '@/stores/auth'

const { t } = useI18n()
const router = useRouter()
const auth = useAuthStore()

const locale = computed(() => currentLocale())
const isAuthed = computed(() => auth.isAuthed)
const username = computed(() => auth.user?.username ?? '')

function toggleLocale(): void {
  setLocale(locale.value === 'zh-CN' ? 'en-US' : 'zh-CN')
}

async function signOut(): Promise<void> {
  await auth.logout()
  await router.replace({ name: 'login' })
}
</script>

<template>
  <div class="app-shell">
    <main class="app-main">
      <RouterView />
    </main>

    <footer class="app-footer">
      <div v-if="isAuthed" class="footer-user">
        <span class="footer-username">{{ username }}</span>
        <button type="button" class="footer-logout" @click="signOut">
          {{ t('auth.logout') }}
        </button>
      </div>
      <button type="button" class="lang-switch" @click="toggleLocale">
        {{ t('app.langSwitch') }}
      </button>
    </footer>
  </div>
</template>

<script setup lang="ts">
// 应用壳（ADR-0020 IA：底部 zh/EN 即时切换 + 路由出口）。
// 侧栏（概览/项目/Agent/管理 四区）随 12 页 IA 页面票落地；本票壳只挂
// 路由出口与语言切换，双语切换在骨架期即可验证（i18n 对账 + 布局纪律）。

import { computed } from 'vue'
import { useI18n } from 'vue-i18n'

import { currentLocale, setLocale } from '@/i18n'

const { t } = useI18n()

const locale = computed(() => currentLocale())

function toggleLocale(): void {
  setLocale(locale.value === 'zh-CN' ? 'en-US' : 'zh-CN')
}
</script>

<template>
  <div class="app-shell">
    <main class="app-main">
      <RouterView />
    </main>

    <footer class="app-footer">
      <button type="button" class="lang-switch" @click="toggleLocale">
        {{ t('app.langSwitch') }}
      </button>
    </footer>
  </div>
</template>

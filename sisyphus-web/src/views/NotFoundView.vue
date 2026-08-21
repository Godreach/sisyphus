<script setup lang="ts">
// 404（未命中路由，ADR-0020 SPA fallback 语义：未知路径在 UI 侧给提示）。
// #90: 使用 Naive UI NResult + NButton 重写，确保错误页面与整体风格一致。

import { useRouter } from 'vue-router'
import { NResult, NButton } from 'naive-ui'
import { HomeOutline } from '@vicons/ionicons5'
import { useI18n } from 'vue-i18n'

const { t } = useI18n()
const router = useRouter()

function goHome(): void {
  void router.replace({ name: 'overview' })
}
</script>

<template>
  <div class="not-found-page">
    <n-result status="404" :title="t('errors.notFoundTitle')" :description="t('errors.notFoundDesc')">
      <template #footer>
        <n-button type="primary" @click="goHome">
          <template #icon>
            <n-icon :component="HomeOutline" />
          </template>
          {{ t('errors.backToHome') }}
        </n-button>
      </template>
    </n-result>
  </div>
</template>

<script setup lang="ts">
// 404（未命中路由，ADR-0020 SPA fallback 语义：未知路径在 UI 侧给提示）。
// #90: Naive UI NResult + NButton；#112 定稿设计语言：全屏无侧栏（未登录）或
// 壳内就地（已登录）均居中，与认证面同源居中规则。

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

<style scoped>
/* 居中：未登录走 app-bare（app-main 已是纵向 flex 容器，margin:auto 居中）；
   已登录在壳内 app-main 就地居中（min-height + flex 居中 NResult）。 */
.not-found-page {
  margin: auto;
  width: 100%;
  min-height: 60vh;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 24px;
}
</style>

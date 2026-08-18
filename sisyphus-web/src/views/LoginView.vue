<script setup lang="ts">
// 登录页（ADR-0014，票 B4-T2 细化；B4-T1 立骨架）。
// - 登录成功换 cookie 会话（HttpOnly + SameSite=Lax，浏览器自动携带）。
// - 401（用户名或密码错误）与 429（限流）统一错误展示（code 分支）。
// - 登录成功回跳原目标：`route.query.redirect`（路由守卫写入）。

import { ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import { describeSubmitError } from '@/api/errors'
import { useAuthStore } from '@/stores/auth'
import { useI18n } from 'vue-i18n'

const { t } = useI18n()
const route = useRoute()
const router = useRouter()
const auth = useAuthStore()

const username = ref('')
const password = ref('')
const submitting = ref(false)
const errorMessage = ref('')

async function submit(): Promise<void> {
  errorMessage.value = ''
  submitting.value = true
  try {
    // 登录换会话 cookie（auth store 单一动作），成功即回跳原目标。
    await auth.login(username.value, password.value)
    const redirect = typeof route.query.redirect === 'string' ? route.query.redirect : '/'
    await router.replace(redirect)
  } catch (err) {
    errorMessage.value = describeSubmitError(err)
  } finally {
    submitting.value = false
  }
}
</script>

<template>
  <div class="login-page">
    <form class="login-card" @submit.prevent="submit">
      <h1>{{ t('app.name') }}</h1>
      <p class="login-tagline">{{ t('auth.loginTagline') }}</p>

      <label class="field">
        <span>{{ t('auth.username') }}</span>
        <input v-model="username" name="username" autocomplete="username" required />
      </label>

      <label class="field">
        <span>{{ t('auth.password') }}</span>
        <input v-model="password" type="password" name="password" autocomplete="current-password" required />
      </label>

      <p v-if="errorMessage" class="login-error" role="alert">{{ errorMessage }}</p>

      <button type="submit" :disabled="submitting">
        {{ submitting ? t('auth.loggingIn') : t('auth.login') }}
      </button>
    </form>
  </div>
</template>

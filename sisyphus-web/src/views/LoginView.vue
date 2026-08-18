<script setup lang="ts">
// 登录页（ADR-0014，票 B4-T2 细化；B4-T1 立骨架）。
// - 登录成功换 cookie 会话（HttpOnly + SameSite=Lax，浏览器自动携带）。
// - 401（用户名或密码错误）与 429（限流）统一错误展示（code 分支）。
// - 登录成功回跳原目标：`route.query.redirect`（路由守卫写入）。

import { ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import { authApi } from '@/api/client'
import { ApiError, NETWORK_ERROR_CODE } from '@/api/http'
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

/** 登录失败的错误信息（按 code 分支）：401 / 429 / 网络 / 其它。 */
function describeError(err: unknown): string {
  if (err instanceof ApiError) {
    if (err.code === 'RATE_LIMITED') {
      const ms = err.retryAfterMs
      return ms != null ? t('auth.loginRateLimited', { seconds: Math.ceil(ms / 1000) }) : t('auth.loginRateLimitedGeneric')
    }
    if (err.code === NETWORK_ERROR_CODE) {
      return t('errors.network')
    }
    // 401 与其它 4xx：直接用后端 message（人读、可展示）。
    return err.message
  }
  return t('errors.generic')
}

async function submit(): Promise<void> {
  errorMessage.value = ''
  submitting.value = true
  try {
    const me = await authApi.login({ username: username.value, password: password.value })
    auth.setAuthed({ username: me.username, isAdmin: me.is_admin })
    // 登录成功回跳原目标（路由守卫写入的 redirect 查询参数）。
    const redirect = typeof route.query.redirect === 'string' ? route.query.redirect : '/'
    await router.replace(redirect)
  } catch (err) {
    errorMessage.value = describeError(err)
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

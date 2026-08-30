<script setup lang="ts">
// 登录页（ADR-0014，票 B4-T2 细化；B4-T1 立骨架）。
// - 登录成功换 cookie 会话（HttpOnly + SameSite=Lax，浏览器自动携带）。
// - 401（用户名或密码错误）与 429（限流）统一错误展示（code 分支）。
// - 登录成功回跳原目标：`route.query.redirect`（路由守卫写入）。
// #88: 使用 Naive UI 组件重写，验证主题配置和组件集成。

import { ref, computed } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { NCard, NForm, NFormItem, NInput, NButton, NAlert, NCheckbox, NIcon } from 'naive-ui'
import { PersonOutline, LockClosedOutline } from '@vicons/ionicons5'

import { describeSubmitError } from '@/api/errors'
import { useAuthStore } from '@/stores/auth'
import { useI18n } from 'vue-i18n'

const { t } = useI18n()
const route = useRoute()
const router = useRouter()
const auth = useAuthStore()

const username = ref('')
const password = ref('')
/** 保持登录（契约先行字段 remember_me，票 #114）：默认勾选。 */
const rememberMe = ref(true)
const submitting = ref(false)
const errorMessage = ref('')
const formRef = ref<InstanceType<typeof NForm> | null>(null)

const rules = computed(() => ({
  username: {
    required: true,
    message: t('auth.usernameRequired'),
    trigger: 'blur',
  },
  password: {
    required: true,
    message: t('auth.passwordRequired'),
    trigger: 'blur',
  },
}))

async function submit(): Promise<void> {
  errorMessage.value = ''
  try {
    await formRef.value?.validate()
  } catch {
    // 校验失败，不提交
    return
  }
  submitting.value = true
  try {
    // 登录换会话 cookie（auth store 单一动作；remember_me 走保持登录
    // 有效期 cookie），成功即回跳原目标。
    await auth.login(username.value, password.value, rememberMe.value)
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
    <n-card class="login-card" :bordered="false">
      <h1>{{ t('app.name') }}</h1>
      <p class="login-tagline">{{ t('auth.loginTagline') }}</p>

      <n-form ref="formRef" :model="{ username, password }" :rules="rules" @submit.prevent="submit">
        <n-form-item path="username" :label="t('auth.username')">
          <n-input
            v-model:value="username"
            :placeholder="t('auth.username')"
            :input-props="{ name: 'username', autocomplete: 'username' }"
          >
            <template #prefix>
              <n-icon :component="PersonOutline" />
            </template>
          </n-input>
        </n-form-item>

        <n-form-item path="password" :label="t('auth.password')">
          <n-input
            v-model:value="password"
            type="password"
            show-password-on="mousedown"
            :placeholder="t('auth.password')"
            :input-props="{ name: 'password', autocomplete: 'current-password' }"
          >
            <template #prefix>
              <n-icon :component="LockClosedOutline" />
            </template>
          </n-input>
        </n-form-item>

        <n-checkbox
          v-model:checked="rememberMe"
          class="login-remember"
          data-testid="remember-me"
          :label="t('auth.rememberMe')"
        />

        <n-alert v-if="errorMessage" type="error" :title="errorMessage" role="alert" />

        <n-button
          type="primary"
          attr-type="submit"
          :disabled="submitting"
          :loading="submitting"
          block
        >
          {{ submitting ? t('auth.loggingIn') : t('auth.login') }}
        </n-button>
      </n-form>
    </n-card>
  </div>
</template>

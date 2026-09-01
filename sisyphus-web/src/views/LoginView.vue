<script setup lang="ts">
// 登录页（ADR-0014，票 B4-T2 细化；#88 Naive UI 迁移；#112 定稿设计语言）。
// - 全屏无侧栏、居中卡片（spec #100：认证面统一到定稿设计语言；首屏品牌
//   标识 + 卡片层级，达发布级首装观感）。卡片走设计 Token（surface 底 +
//   12px 圆角 + 边框 + 轻阴影），与三主页面 sisy-card 同源。
// - 登录成功换 cookie 会话（HttpOnly + SameSite=Lax，浏览器自动携带）。
// - 401（用户名或密码错误）与 429（限流）统一错误展示（code 分支）。
// - 登录成功回跳原目标：`route.query.redirect`（路由守卫写入）。
// - remember_me（契约先行字段，票 #114）：默认勾选 → 保持登录有效期 cookie。

import { ref, computed } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { NForm, NFormItem, NInput, NButton, NAlert, NCheckbox, NIcon } from 'naive-ui'
import { PersonOutline, LockClosedOutline } from '@vicons/ionicons5'

import AuthCard from '@/components/base/AuthCard.vue'
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
    <AuthCard class="login-card">
      <p class="login-tagline">{{ t('auth.loginTagline') }}</p>

      <n-form
        ref="formRef"
        :model="{ username, password }"
        :rules="rules"
        label-placement="top"
        @submit.prevent="submit"
      >
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

        <n-alert v-if="errorMessage" type="error" :title="errorMessage" role="alert" class="login-alert" />

        <n-button
          type="primary"
          attr-type="submit"
          :disabled="submitting"
          :loading="submitting"
          block
          class="login-submit"
        >
          {{ submitting ? t('auth.loggingIn') : t('auth.login') }}
        </n-button>
      </n-form>
    </AuthCard>
  </div>
</template>

<style scoped>
/* 全屏无侧栏、居中卡片（app-bare 内 app-main 已是纵向 flex 容器，本根用
   margin:auto 居中且高于视口时不裁剪顶部——可上滚）。卡片外壳与品牌标识
   由 AuthCard base 组件提供（票 #112，ADR-0023）；本 scoped 仅留登录页专属。 */
.login-page {
  margin: auto;
  width: 100%;
  max-width: 420px;
  padding: 24px 16px;
}

.login-tagline {
  margin: 0 0 20px;
  color: var(--sisy-color-text-secondary);
  font-size: 13px;
}

/* 保持登录复选框（契约先行字段 remember_me，票 #114）。 */
.login-remember {
  margin: 4px 0 12px;
}

.login-alert {
  margin-bottom: 12px;
}

.login-submit {
  margin-top: 4px;
}

/* 窄屏：页面根内边距收紧（卡片内边距由 AuthCard 自适应）。 */
@media (max-width: 767px) {
  .login-page {
    padding: 16px 12px;
  }
}
</style>

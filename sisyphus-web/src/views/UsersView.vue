<script setup lang="ts">
// 用户 / PAT 管理页（ADR-0014，票 B4-T6）：用户生命周期 + 个人访问令牌。
//
// 管理区全局 admin 面。用户管理端点全局 admin 专属（403 → admin-only 退化态）；
// PAT 端点权限 = owner 本人（v1 无 scope 细分），本页消费当前（全局 admin）用户
// 自身的令牌。
//
// - 用户：`GET/POST /users` + `PATCH /users/{name}` { disabled }（禁用/启用）+
//   `PUT /users/{name}/password`（代办重置密码）。建号时经 `is_admin` 设全局
//   admin；**切换已有用户 admin 标志的端点尚未交付**（PATCH 仅 disabled）→
//   退化标注（建号时可设，已有用户展示只读 badge）。
// - PAT：`GET/POST /auth/tokens` + `DELETE /auth/tokens/{id}`。创建响应一次性
//   返回完整令牌（明文仅此一次，展示后即丢弃）；列表无值形态；吊销删行。

import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'

import { tokensApi, usersApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import type { CreatedTokenResponse, TokenResponse, UserResponse } from '@/api/types'
import { formatDateTime } from '@/utils/format'

const { t } = useI18n()

// --- 用户管理 ---------------------------------------------------------------

const users = ref<UserResponse[] | null>(null)
const usersError = ref('')
/** 403（非全局 admin）→ admin-only 退化态：不渲染用户表/PAT。 */
const adminOnly = ref(false)

const showUserForm = ref(false)
const newUsername = ref('')
const newPassword = ref('')
const newIsAdmin = ref(false)
const creatingUser = ref(false)
const createUserError = ref('')

/** 重置密码（行内展开，按名标记）。 */
const resettingName = ref<string | null>(null)
const resetPasswordValue = ref('')
const resetting = ref(false)
const resetError = ref('')

/** 禁用/启用 busy（按名标记）。 */
const togglingUserName = ref<string | null>(null)

// --- PAT --------------------------------------------------------------------

const tokens = ref<TokenResponse[]>([])
const tokensError = ref('')

const showTokenForm = ref(false)
const tokenName = ref('')
const tokenExpiresLocal = ref('')
const creatingToken = ref(false)
const createTokenError = ref('')
/** 创建 PAT 响应：完整令牌明文仅此一次，展示后即丢弃。 */
const createdToken = ref<CreatedTokenResponse | null>(null)
/** 吊销 busy（按 id 标记，禁对应按钮——防双击连发两次 DELETE，第二次必 404）。 */
const revokingTokenId = ref<number | null>(null)

onMounted(() => {
  void Promise.all([loadUsers(), loadTokens()])
})

const canCreateUser = computed(
  () => newUsername.value.trim() !== '' && newPassword.value !== '' && !creatingUser.value,
)

const canCreateToken = computed(
  () => tokenName.value.trim() !== '' && !creatingToken.value,
)

/** 加载用户清单（全局 admin 专属）。403 → admin-only 退化。 */
async function loadUsers(): Promise<void> {
  usersError.value = ''
  adminOnly.value = false
  try {
    users.value = await usersApi.list()
  } catch (err) {
    if (err instanceof ApiError && err.status === 403) {
      users.value = null
      adminOnly.value = true
      return
    }
    users.value = null
    usersError.value = describeSubmitError(err)
  }
}

/** 建号：`POST /users`（is_admin 默认 false，建号时显式设全局 admin）+ 刷新。 */
async function createUser(): Promise<void> {
  createUserError.value = ''
  creatingUser.value = true
  try {
    await usersApi.create({
      username: newUsername.value.trim(),
      password: newPassword.value,
      is_admin: newIsAdmin.value,
    })
    showUserForm.value = false
    newUsername.value = ''
    newPassword.value = ''
    newIsAdmin.value = false
    await loadUsers()
  } catch (err) {
    createUserError.value = describeSubmitError(err)
  } finally {
    creatingUser.value = false
  }
}

/** 禁用/启用切换：`PATCH /users/{name}` { disabled }，禁用即踢线。 */
async function toggleUserDisabled(user: UserResponse): Promise<void> {
  togglingUserName.value = user.username
  try {
    await usersApi.patch(user.username, { disabled: !user.disabled })
    await loadUsers()
  } catch (err) {
    usersError.value = describeSubmitError(err)
  } finally {
    togglingUserName.value = null
  }
}

function startResetPassword(user: UserResponse): void {
  resettingName.value = user.username
  resetPasswordValue.value = ''
  resetError.value = ''
}

function cancelResetPassword(): void {
  resettingName.value = null
}

/** 代办重置密码：`PUT /users/{name}/password` { new_password }，成功 204 + 刷新。 */
async function submitResetPassword(user: UserResponse): Promise<void> {
  resetError.value = ''
  resetting.value = true
  try {
    await usersApi.resetPassword(user.username, { new_password: resetPasswordValue.value })
    resettingName.value = null
    resetPasswordValue.value = ''
  } catch (err) {
    resetError.value = describeSubmitError(err)
  } finally {
    resetting.value = false
  }
}

// --- PAT --------------------------------------------------------------------

/** 加载当前用户 PAT（owner 本人；名/创建时间/过期，永不含令牌值）。 */
async function loadTokens(): Promise<void> {
  tokensError.value = ''
  try {
    const rows = await tokensApi.list()
    tokens.value = rows
  } catch (err) {
    tokens.value = []
    tokensError.value = describeSubmitError(err)
  }
}

/** datetime-local 字符串 → Unix 毫秒（空串 → undefined = 永不过期）。 */
function tokenExpiryMs(): number | undefined {
  if (tokenExpiresLocal.value === '') return undefined
  const ms = new Date(tokenExpiresLocal.value).getTime()
  return Number.isFinite(ms) ? ms : undefined
}

/** 创建 PAT：`POST /auth/tokens` → 完整令牌明文仅此一次 + 刷新列表。 */
async function createToken(): Promise<void> {
  createTokenError.value = ''
  creatingToken.value = true
  try {
    const created = await tokensApi.create({
      name: tokenName.value.trim(),
      expires_at: tokenExpiryMs(),
    })
    createdToken.value = created
    showTokenForm.value = false
    tokenName.value = ''
    tokenExpiresLocal.value = ''
    await loadTokens()
  } catch (err) {
    createTokenError.value = describeSubmitError(err)
  } finally {
    creatingToken.value = false
  }
}

/** 丢弃一次性令牌面板（此后任何端点都无法找回）。 */
function dismissToken(): void {
  createdToken.value = null
}

/** 吊销 PAT：`DELETE /auth/tokens/{id}`，下一请求即 401 + 刷新。按 id 标记
 *  busy 禁按钮，防双击连发两次 DELETE（第二次必 404——他人 id 不暴露存在性，
 *  但本端点是 owner 本人，双发仍属浪费请求且产生噪声错误）。 */
async function revokeToken(id: number): Promise<void> {
  revokingTokenId.value = id
  try {
    await tokensApi.revoke(id)
    await loadTokens()
  } catch (err) {
    tokensError.value = describeSubmitError(err)
  } finally {
    revokingTokenId.value = null
  }
}
</script>

<template>
  <div class="admin-page users-page">
    <div class="page-header">
      <h1 class="page-title">{{ t('routes.adminUsers') }}</h1>
    </div>

    <!-- 403 退化态：仅全局管理员可见。 -->
    <p v-if="adminOnly" class="form-hint">{{ t('admin.adminOnly') }}</p>

    <template v-else>
      <!-- 用户管理。 -->
      <section class="detail-section users-section">
        <div class="users-section-head">
          <h2>{{ t('users.title') }}</h2>
          <button
            v-if="!showUserForm"
            type="button"
            class="btn-primary"
            name="user-new"
            @click="showUserForm = true"
          >
            {{ t('users.newUser') }}
          </button>
        </div>

        <p v-if="usersError" class="form-error" role="alert">{{ usersError }}</p>

        <!-- 切换已有用户全局 admin 标志的端点尚未交付（PATCH 仅 disabled）：
             建号时可设 is_admin，已有用户仅展示只读 badge——显式标注退化。 -->
        <p class="form-hint">{{ t('users.adminToggleUnavailable') }}</p>

        <!-- 建号表单（用户名 + 密码 + 全局 admin 复选——建号时设 admin）。 -->
        <form v-if="showUserForm" class="user-form" @submit.prevent>
          <label class="field">
            <span>{{ t('users.username') }}</span>
            <input v-model="newUsername" name="user-username" :placeholder="t('users.usernamePlaceholder')" />
          </label>
          <p class="form-hint">{{ t('users.usernameHint') }}</p>
          <label class="field">
            <span>{{ t('users.password') }}</span>
            <input v-model="newPassword" type="password" name="user-password" />
          </label>
          <p class="form-hint">{{ t('users.passwordHint') }}</p>
          <label class="field user-admin-field">
            <input v-model="newIsAdmin" type="checkbox" name="user-is-admin" />
            <span>{{ t('users.isAdmin') }}</span>
          </label>
          <div class="user-form-actions">
            <button
              type="button"
              class="btn-primary"
              name="user-create"
              :disabled="!canCreateUser"
              @click="createUser"
            >
              {{ creatingUser ? t('users.creating') : t('users.create') }}
            </button>
            <button type="button" class="btn-secondary" name="user-cancel" @click="showUserForm = false">
              {{ t('users.cancel') }}
            </button>
          </div>
          <p v-if="createUserError" class="form-error" role="alert">{{ createUserError }}</p>
        </form>

        <table v-if="users" class="user-table">
          <thead>
            <tr>
              <th>{{ t('users.username') }}</th>
              <th>{{ t('users.role') }}</th>
              <th>{{ t('users.status') }}</th>
              <th>{{ t('users.actions') }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="u in users" :key="u.id">
              <td class="mono">{{ u.username }}</td>
              <td>
                <span v-if="u.is_admin" class="user-admin-badge">{{ t('users.adminBadge') }}</span>
                <span v-else class="form-hint">{{ t('users.userBadge') }}</span>
              </td>
              <td>
                <span v-if="u.disabled" class="user-disabled-badge">{{ t('users.disabled') }}</span>
                <span v-else class="user-active-badge">{{ t('users.active') }}</span>
              </td>
              <td>
                <div class="user-row-actions">
                  <button
                    type="button"
                    class="btn-secondary"
                    name="user-toggle"
                    :disabled="togglingUserName === u.username"
                    @click="toggleUserDisabled(u)"
                  >
                    {{ u.disabled ? t('users.enable') : t('users.disable') }}
                  </button>
                  <button
                    v-if="resettingName !== u.username"
                    type="button"
                    class="btn-secondary"
                    name="user-reset"
                    @click="startResetPassword(u)"
                  >
                    {{ t('users.resetPassword') }}
                  </button>
                </div>
                <form v-if="resettingName === u.username" class="user-reset-form" @submit.prevent>
                  <label class="field">
                    <span>{{ t('users.newPassword') }}</span>
                    <input v-model="resetPasswordValue" type="password" name="reset-password" />
                  </label>
                  <div class="user-form-actions">
                    <button
                      type="button"
                      class="btn-primary"
                      name="reset-submit"
                      :disabled="resetting"
                      @click="submitResetPassword(u)"
                    >
                      {{ resetting ? t('users.resetting') : t('users.resetSubmit') }}
                    </button>
                    <button type="button" class="btn-secondary" name="reset-cancel" @click="cancelResetPassword">
                      {{ t('users.cancel') }}
                    </button>
                  </div>
                  <p v-if="resetError" class="form-error" role="alert">{{ resetError }}</p>
                </form>
              </td>
            </tr>
          </tbody>
        </table>
        <p v-else-if="!usersError" class="form-hint">{{ t('users.empty') }}</p>
      </section>

      <!-- PAT 管理（当前用户令牌；创建时明文一次 + 可吊销）。 -->
      <section class="detail-section users-section">
        <div class="users-section-head">
          <h2>{{ t('tokens.title') }}</h2>
          <button
            v-if="!showTokenForm && !createdToken"
            type="button"
            class="btn-primary"
            name="token-new"
            @click="showTokenForm = true"
          >
            {{ t('tokens.newToken') }}
          </button>
        </div>

        <p v-if="tokensError" class="form-error" role="alert">{{ tokensError }}</p>

        <!-- 一次性令牌面板（明文仅此一次，展示后即丢弃）。 -->
        <div v-if="createdToken" class="token-creds" role="alert">
          <p class="token-creds-title">{{ t('tokens.oneTime') }}</p>
          <dl>
            <dt>{{ t('tokens.tokenLabel') }}</dt>
            <dd class="mono">{{ createdToken.token }}</dd>
            <dt>{{ t('tokens.name') }}</dt>
            <dd class="mono">{{ createdToken.name }}</dd>
          </dl>
          <p class="token-creds-warn">{{ t('tokens.warn') }}</p>
          <div class="token-creds-actions">
            <button type="button" class="btn-secondary" name="token-dismiss" @click="dismissToken">
              {{ t('tokens.dismiss') }}
            </button>
          </div>
        </div>

        <form v-if="showTokenForm" class="token-form" @submit.prevent>
          <label class="field">
            <span>{{ t('tokens.name') }}</span>
            <input v-model="tokenName" name="token-name" :placeholder="t('tokens.namePlaceholder')" />
          </label>
          <label class="field">
            <span>{{ t('tokens.expiresAt') }}</span>
            <input v-model="tokenExpiresLocal" type="datetime-local" name="token-expires" />
          </label>
          <p class="form-hint">{{ t('tokens.expiresHint') }}</p>
          <div class="user-form-actions">
            <button
              type="button"
              class="btn-primary"
              name="token-create"
              :disabled="!canCreateToken"
              @click="createToken"
            >
              {{ creatingToken ? t('tokens.creating') : t('tokens.create') }}
            </button>
            <button type="button" class="btn-secondary" name="token-cancel" @click="showTokenForm = false">
              {{ t('users.cancel') }}
            </button>
          </div>
          <p v-if="createTokenError" class="form-error" role="alert">{{ createTokenError }}</p>
        </form>

        <table v-if="tokens.length > 0" class="user-table">
          <thead>
            <tr>
              <th>{{ t('tokens.name') }}</th>
              <th>{{ t('tokens.expiresAt') }}</th>
              <th>{{ t('tokens.createdAt') }}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="tk in tokens" :key="tk.id">
              <td class="mono">{{ tk.name }}</td>
              <td>{{ tk.expires_at != null ? formatDateTime(tk.expires_at) : t('tokens.neverExpires') }}</td>
              <td>{{ formatDateTime(tk.created_at) }}</td>
              <td>
                <button
                  type="button"
                  class="btn-secondary"
                  name="token-revoke"
                  :disabled="revokingTokenId === tk.id"
                  @click="revokeToken(tk.id)"
                >
                  {{ t('tokens.revoke') }}
                </button>
              </td>
            </tr>
          </tbody>
        </table>
        <p v-else-if="!tokensError" class="form-hint">{{ t('tokens.empty') }}</p>
      </section>
    </template>
  </div>
</template>

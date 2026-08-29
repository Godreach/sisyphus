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
//   退化标注（建号时可设，已有用户展示只读 NTag）。
// - PAT：`GET/POST /auth/tokens` + `DELETE /auth/tokens/{id}`。创建响应一次性
//   返回完整令牌（明文仅此一次，NCode 展示后即丢弃）；列表无值形态；吊销
//   删行（NPopconfirm 确认——吊销立即生效）。
// #95: 使用 Naive UI 组件重写——用户/PAT 分区改 NCard（header-extra 新建入口）、
// 两张清单改 NDataTable（角色/状态列 NTag、启用/禁用改 NSwitch、吊销经
// NPopconfirm）、建号/重置密码/建令牌改 NModal + NForm、一次性令牌 NCode +
// 复制按钮（与 AgentListView 凭据弹窗同纪律）、成功操作 NMessage toast、
// 错误态 NAlert。

import { computed, h, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  NAlert,
  NButton,
  NCard,
  NCode,
  NDataTable,
  NDatePicker,
  NDescriptions,
  NDescriptionsItem,
  NEmpty,
  NForm,
  NFormItem,
  NIcon,
  NInput,
  NModal,
  NPopconfirm,
  NSkeleton,
  NSwitch,
  NTag,
  useMessage,
  type DataTableColumns,
} from 'naive-ui'
import { ClipboardOutline } from '@vicons/ionicons5'

import { tokensApi, usersApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import type { CreatedTokenResponse, TokenResponse, UserResponse } from '@/api/types'
import { formatDateTime } from '@/utils/format'

const { t } = useI18n()
const message = useMessage()

// --- 用户管理 ---------------------------------------------------------------

const users = ref<UserResponse[] | null>(null)
const usersError = ref('')
const usersLoading = ref(true)
/** 403（非全局 admin）→ admin-only 退化态：不渲染用户表/PAT。 */
const adminOnly = ref(false)

/** 建号弹窗。 */
const showUserForm = ref(false)
const newUsername = ref('')
const newPassword = ref('')
const newIsAdmin = ref(false)
const creatingUser = ref(false)
const createUserError = ref('')

/** 重置密码弹窗（按用户打开）。 */
const resettingUser = ref<UserResponse | null>(null)
const resetPasswordValue = ref('')
const resetting = ref(false)
const resetError = ref('')

/** 禁用/启用 busy（按名标记，开关转圈）。 */
const togglingUserName = ref<string | null>(null)

// --- PAT --------------------------------------------------------------------

const tokens = ref<TokenResponse[]>([])
const tokensError = ref('')
/** 首载中（true 期间不闪「暂无令牌」空态——数据到达后回落）。 */
const tokensLoading = ref(true)

/** 建令牌弹窗。 */
const showTokenForm = ref(false)
const tokenName = ref('')
/** 过期时间（NDatePicker datetime 值形态：Unix 毫秒 | null；null = 永不过期）。 */
const tokenExpiresAt = ref<number | null>(null)
const creatingToken = ref(false)
const createTokenError = ref('')
/** 创建 PAT 响应：完整令牌明文仅此一次，展示后即丢弃。 */
const createdToken = ref<CreatedTokenResponse | null>(null)
/** 吊销 busy（按 id 标记，防双击连发两次 DELETE——第二次必 404）。 */
const revokingTokenId = ref<number | null>(null)

onMounted(() => {
  void Promise.all([loadUsers(), loadTokens()])
})

const canCreateUser = computed(
  () => newUsername.value.trim() !== '' && newPassword.value !== '' && !creatingUser.value,
)

const canCreateToken = computed(() => tokenName.value.trim() !== '' && !creatingToken.value)

/** 用户表列（角色/状态列 NTag；动作列 NSwitch 启用/禁用 + 重置密码按钮）。 */
const userColumns = computed<DataTableColumns<UserResponse>>(() => [
  {
    title: t('users.username'),
    key: 'username',
    render: (row) => h('span', { class: 'mono' }, row.username),
  },
  {
    title: t('users.role'),
    key: 'role',
    render: (row) =>
      h(
        NTag,
        { size: 'small', type: row.is_admin ? 'warning' : 'default', bordered: false },
        { default: () => (row.is_admin ? t('users.adminBadge') : t('users.userBadge')) },
      ),
  },
  {
    title: t('users.status'),
    key: 'status',
    render: (row) =>
      h(
        NTag,
        { size: 'small', type: row.disabled ? 'error' : 'success', bordered: false },
        { default: () => (row.disabled ? t('users.disabled') : t('users.active')) },
      ),
  },
  {
    title: t('users.actions'),
    key: 'actions',
    render: (row) =>
      h('div', { class: 'user-row-actions' }, [
        h(NSwitch, {
          size: 'small',
          class: 'user-toggle',
          value: !row.disabled,
          loading: togglingUserName.value === row.username,
          'onUpdate:value': () => void toggleUserDisabled(row),
        }),
        h(
          NButton,
          { size: 'small', name: 'user-reset', onClick: () => startResetPassword(row) },
          { default: () => t('users.resetPassword') },
        ),
      ]),
  },
])

const userRowKey = (row: UserResponse): number => row.id

/** PAT 表列（吊销经 NPopconfirm——立即生效不可逆）。 */
const tokenColumns = computed<DataTableColumns<TokenResponse>>(() => [
  {
    title: t('tokens.name'),
    key: 'name',
    render: (row) => h('span', { class: 'mono' }, row.name),
  },
  {
    title: t('tokens.expiresAt'),
    key: 'expires_at',
    render: (row) => (row.expires_at != null ? formatDateTime(row.expires_at) : t('tokens.neverExpires')),
  },
  {
    title: t('tokens.createdAt'),
    key: 'created_at',
    render: (row) => formatDateTime(row.created_at),
  },
  {
    title: '',
    key: 'actions',
    width: 100,
    render: (row) =>
      h(
        NPopconfirm,
        {
          positiveText: t('common.confirm'),
          negativeText: t('common.cancel'),
          onPositiveClick: () => void revokeToken(row.id),
        },
        {
          trigger: () =>
            h(
              NButton,
              {
                size: 'small',
                name: 'token-revoke',
                loading: revokingTokenId.value === row.id,
              },
              { default: () => t('tokens.revoke') },
            ),
          default: () => t('tokens.revokeConfirm'),
        },
      ),
  },
])

const tokenRowKey = (row: TokenResponse): number => row.id

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
  } finally {
    usersLoading.value = false
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
    message.success(t('users.created'))
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
    message.success(user.disabled ? t('users.enabledMsg') : t('users.disabledMsg'))
    await loadUsers()
  } catch (err) {
    usersError.value = describeSubmitError(err)
  } finally {
    togglingUserName.value = null
  }
}

function startResetPassword(user: UserResponse): void {
  resettingUser.value = user
  resetPasswordValue.value = ''
  resetError.value = ''
}

function cancelResetPassword(): void {
  resettingUser.value = null
}

/** 代办重置密码：`PUT /users/{name}/password` { new_password }，成功 204。 */
async function submitResetPassword(): Promise<void> {
  const user = resettingUser.value
  if (!user) return
  resetError.value = ''
  resetting.value = true
  try {
    await usersApi.resetPassword(user.username, { new_password: resetPasswordValue.value })
    resettingUser.value = null
    resetPasswordValue.value = ''
    message.success(t('users.passwordReset'))
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
  } finally {
    tokensLoading.value = false
  }
}

/** 创建 PAT：`POST /auth/tokens` → 完整令牌明文仅此一次 + 刷新列表。 */
async function createToken(): Promise<void> {
  createTokenError.value = ''
  creatingToken.value = true
  try {
    const created = await tokensApi.create({
      name: tokenName.value.trim(),
      expires_at: tokenExpiresAt.value ?? undefined,
    })
    createdToken.value = created
    showTokenForm.value = false
    tokenName.value = ''
    tokenExpiresAt.value = null
    message.success(t('tokens.createdMsg'))
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

/** 复制文本到剪贴板（不可用时静默——内容在 NCode 框内可手选）。 */
async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text)
    message.success(t('tokens.copied'))
  } catch {
    // 剪贴板 API 不可用（非安全上下文等）：不打断流程。
  }
}

/** 吊销 PAT：`DELETE /auth/tokens/{id}`，下一请求即 401 + 刷新。 */
async function revokeToken(id: number): Promise<void> {
  revokingTokenId.value = id
  try {
    await tokensApi.revoke(id)
    message.success(t('tokens.revokedMsg'))
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
    <!-- 403 退化态：仅全局管理员可见。 -->
    <p v-if="adminOnly" class="form-hint">{{ t('admin.adminOnly') }}</p>

    <template v-else>
      <!-- 用户管理。 -->
      <n-card :title="t('users.title')" size="small" class="users-card">
        <template #header-extra>
          <n-button type="primary" size="small" name="user-new" @click="showUserForm = true">
            {{ t('users.newUser') }}
          </n-button>
        </template>

        <n-alert v-if="usersError" type="error" :title="usersError" role="alert" class="users-card-alert" />

        <!-- 切换已有用户全局 admin 标志的端点尚未交付（PATCH 仅 disabled）：
             建号时可设 is_admin，已有用户仅展示只读 NTag——显式标注退化。 -->
        <p class="form-hint">{{ t('users.adminToggleUnavailable') }}</p>

        <div v-if="usersLoading && !usersError" class="users-skeleton">
          <n-skeleton v-for="i in 3" :key="i" text :repeat="1" height="28px" class="users-skeleton-row" />
        </div>

        <div v-else-if="users && users.length === 0" class="users-empty">
          <n-empty :description="t('users.empty')" />
        </div>

        <n-data-table
          v-else-if="users"
          :columns="userColumns"
          :data="users"
          :row-key="userRowKey"
          :bordered="false"
          :single-line="true"
          size="small"
          :scroll-x="560"
          class="users-table"
        />
      </n-card>

      <!-- PAT 管理（当前用户令牌；创建时明文一次 + 可吊销）。 -->
      <n-card :title="t('tokens.title')" size="small" class="users-card">
        <template #header-extra>
          <n-button
            v-if="!createdToken"
            type="primary"
            size="small"
            name="token-new"
            @click="showTokenForm = true"
          >
            {{ t('tokens.newToken') }}
          </n-button>
        </template>

        <n-alert v-if="tokensError" type="error" :title="tokensError" role="alert" class="users-card-alert" />

        <!-- 首载骨架屏（防止「暂无令牌」空态闪烁——数据到达后替换）。 -->
        <div v-if="tokensLoading && !tokensError" class="users-skeleton">
          <n-skeleton v-for="i in 2" :key="i" text height="28px" class="users-skeleton-row" />
        </div>

        <div v-else-if="tokens.length === 0 && !tokensError" class="users-empty">
          <n-empty :description="t('tokens.empty')" />
        </div>

        <n-data-table
          v-else
          :columns="tokenColumns"
          :data="tokens"
          :row-key="tokenRowKey"
          :bordered="false"
          :single-line="true"
          size="small"
          :scroll-x="640"
          class="tokens-table"
        />
      </n-card>

      <!-- 建号弹窗（用户名 + 密码 + 全局 admin 开关——建号时设 admin）。 -->
      <n-modal
        v-model:show="showUserForm"
        preset="card"
        :title="t('users.newUser')"
        style="width: 440px"
        :bordered="false"
      >
        <n-form label-placement="top" @submit.prevent="createUser">
          <n-form-item :label="t('users.username')" :show-require-mark="true">
            <n-input
              v-model:value="newUsername"
              :input-props="{ name: 'user-username' }"
              :placeholder="t('users.usernamePlaceholder')"
            />
          </n-form-item>
          <p class="form-hint">{{ t('users.usernameHint') }}</p>
          <n-form-item :label="t('users.password')" :show-require-mark="true">
            <n-input
              v-model:value="newPassword"
              type="password"
              show-password-on="click"
              :input-props="{ name: 'user-password' }"
            />
          </n-form-item>
          <p class="form-hint">{{ t('users.passwordHint') }}</p>
          <n-form-item :label="t('users.isAdmin')">
            <n-switch v-model:value="newIsAdmin" class="user-admin-switch" />
          </n-form-item>
          <n-alert
            v-if="createUserError"
            type="error"
            :title="createUserError"
            role="alert"
            class="users-modal-alert"
          />
          <div class="modal-actions">
            <n-button @click="showUserForm = false">{{ t('common.cancel') }}</n-button>
            <n-button
              type="primary"
              name="user-create"
              :disabled="!canCreateUser"
              :loading="creatingUser"
              @click="createUser"
            >
              {{ creatingUser ? t('users.creating') : t('users.create') }}
            </n-button>
          </div>
        </n-form>
      </n-modal>

      <!-- 重置密码弹窗。 -->
      <n-modal
        :show="resettingUser !== null"
        preset="card"
        :title="t('users.resetPassword')"
        style="width: 440px"
        :bordered="false"
        @update:show="(show: boolean) => { if (!show) cancelResetPassword() }"
      >
        <n-form label-placement="top" @submit.prevent="submitResetPassword">
          <n-descriptions :column="1" size="small" bordered class="users-reset-desc">
            <n-descriptions-item :label="t('users.username')">
              <span class="mono">{{ resettingUser?.username }}</span>
            </n-descriptions-item>
          </n-descriptions>
          <n-form-item :label="t('users.newPassword')" :show-require-mark="true">
            <n-input
              v-model:value="resetPasswordValue"
              type="password"
              show-password-on="click"
              :input-props="{ name: 'reset-password' }"
            />
          </n-form-item>
          <n-alert v-if="resetError" type="error" :title="resetError" role="alert" class="users-modal-alert" />
          <div class="modal-actions">
            <n-button @click="cancelResetPassword">{{ t('common.cancel') }}</n-button>
            <n-button
              type="primary"
              name="reset-submit"
              :loading="resetting"
              @click="submitResetPassword"
            >
              {{ resetting ? t('users.resetting') : t('users.resetSubmit') }}
            </n-button>
          </div>
        </n-form>
      </n-modal>

      <!-- 建令牌弹窗（令牌名 + 过期时间；留空 = 永不过期）。 -->
      <n-modal
        v-model:show="showTokenForm"
        preset="card"
        :title="t('tokens.newToken')"
        style="width: 440px"
        :bordered="false"
      >
        <n-form label-placement="top" @submit.prevent="createToken">
          <n-form-item :label="t('tokens.name')" :show-require-mark="true">
            <n-input
              v-model:value="tokenName"
              :input-props="{ name: 'token-name' }"
              :placeholder="t('tokens.namePlaceholder')"
            />
          </n-form-item>
          <n-form-item :label="t('tokens.expiresAt')">
            <n-date-picker
              v-model:value="tokenExpiresAt"
              type="datetime"
              clearable
              class="users-token-expiry"
              :is-date-disabled="(ts: number) => ts < Date.now()"
            />
          </n-form-item>
          <p class="form-hint">{{ t('tokens.expiresHint') }}</p>
          <n-alert
            v-if="createTokenError"
            type="error"
            :title="createTokenError"
            role="alert"
            class="users-modal-alert"
          />
          <div class="modal-actions">
            <n-button @click="showTokenForm = false">{{ t('common.cancel') }}</n-button>
            <n-button
              type="primary"
              name="token-create"
              :disabled="!canCreateToken"
              :loading="creatingToken"
              @click="createToken"
            >
              {{ creatingToken ? t('tokens.creating') : t('tokens.create') }}
            </n-button>
          </div>
        </n-form>
      </n-modal>

      <!-- 一次性令牌弹窗（明文仅此一次，NCode 展示 + 复制，丢弃即不可找回）。 -->
      <n-modal
        :show="createdToken !== null"
        preset="card"
        :title="t('tokens.oneTime')"
        style="width: 520px"
        :bordered="false"
        :mask-closable="false"
        @update:show="(show: boolean) => { if (!show) dismissToken() }"
      >
        <n-alert type="warning" :show-icon="true" class="users-modal-alert">
          {{ t('tokens.warn') }}
        </n-alert>
        <n-descriptions :column="1" size="small" bordered class="users-creds-desc">
          <n-descriptions-item :label="t('tokens.name')">
            <span class="mono">{{ createdToken?.name }}</span>
          </n-descriptions-item>
          <n-descriptions-item :label="t('tokens.tokenLabel')">
            <n-code :code="createdToken?.token ?? ''" class="users-cred-code" />
            <n-button
              size="tiny"
              quaternary
              type="primary"
              name="token-copy"
              @click="copyText(createdToken?.token ?? '')"
            >
              <template #icon><n-icon :component="ClipboardOutline" /></template>
            </n-button>
          </n-descriptions-item>
        </n-descriptions>
        <div class="modal-actions">
          <n-button type="primary" name="token-dismiss" @click="dismissToken">
            {{ t('tokens.dismiss') }}
          </n-button>
        </div>
      </n-modal>
    </template>
  </div>
</template>

<style scoped>
.users-card {
  margin-top: 4px;
}

.users-card-alert {
  margin-bottom: 8px;
}

.users-skeleton {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.users-skeleton-row {
  width: 100%;
}

.users-empty {
  padding: 16px 0;
}

.user-row-actions {
  display: flex;
  align-items: center;
  gap: 12px;
}

.users-token-expiry {
  width: 100%;
}

.modal-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 8px;
}

.users-modal-alert {
  margin-bottom: 8px;
}

.users-reset-desc {
  margin-bottom: 12px;
}

.users-cred-code {
  margin-right: 6px;
}

.mono {
  font-family: ui-monospace, 'Cascadia Code', Consolas, monospace;
  font-size: 12px;
  word-break: break-all;
}
</style>

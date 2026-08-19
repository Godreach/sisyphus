<script setup lang="ts">
// 初始化引导 wizard（ADR-0010/0020，票 B4-T2）。
//
// 三步（管理员 -> 首个 Agent -> 首个项目）各自可跳过、均带 CLI 等价提示：
// - 管理员步：`POST /auth/setup` 建首个全局管理员（仅用户表为空时可用；
//   空库判定与进入条件由路由守卫经 `isSetupNeeded()` 探测）。提交成功即
//   自动登录（登录是独立一步——见 `api/auth.rs` setup handler 注释），
//   步骤完成后进入 Agent 步。
// - Agent 步：`POST /agents` 建首个 Agent 条目，响应含一次性注册码 +
//   per-Agent token（明文仅此一次，本步展示后即丢弃）；按目标 OS 生成
//   复制即用注册命令（`sisyphus-agent --server-url … --reg-key …`，
//   ADR-0010/ADR-0007）。跳过即不建。
// - 项目步：`POST /projects` 建首个项目（git/svn + 仓库 URL + 可选默认
//   分支）；跳过即不建。
//
// CLI 等价提示：每步头部给 headless/Docker 用户对应的命令行（跑过即视为
// 引导完成，ADR-0010「headless 等价」）。用户显式跳过全部步骤即视为引导
// 完成（dismiss），此后守卫不再把受保护页重定向回 /setup。

import { computed, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'

import { agentsApi, projectsApi, setupApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import { useAuthStore } from '@/stores/auth'
import { buildAgentRegisterCommand, type AgentTargetOs } from '@/utils/agentCommand'

const { t } = useI18n()
const router = useRouter()
const auth = useAuthStore()

/** 当前步骤（0 = 管理员，1 = Agent，2 = 项目）。 */
const step = ref(0)
/** 管理员步提交中 / Agent 步建条目中 / 项目步提交中。 */
const submitting = ref(false)
/** 步骤内错误信息（按 code 分支展示）。 */
const errorMessage = ref('')

/** 管理员步表单。 */
const adminUsername = ref('admin')
const adminPassword = ref('')

/** Agent 步：建条目响应（token + 注册码明文仅此一次；展示后即清）。 */
const agentCreds = ref<{ token: string; registerCode: string; agentName: string } | null>(null)
const agentName = ref('')
/** 已选目标 OS（复制命令按 OS 分档）。 */
const targetOs = ref<AgentTargetOs>('linux')

/** 项目步表单。 */
const projectName = ref('')
const scmType = ref<'git' | 'svn'>('git')
const scmUrl = ref('')
const defaultBranch = ref('')

const isLastStep = computed(() => step.value === 2)

/** 逐步错误清空 + 进入下一步。 */
function goTo(stepIndex: number): void {
  errorMessage.value = ''
  step.value = stepIndex
}

/** 管理员步：`POST /auth/setup` 建首个全局管理员（仅用户表为空时可用），
 *  再 `POST /auth/login` 换会话（setup 只建号不立会话——登录是独立一步，
 *  见 `api/auth.rs`）。422（输入校验）与 429 按统一错误形态就地展示；
 *  404（非空库）视为引导已完成，回落登录页。 */
async function createAdmin(): Promise<void> {
  errorMessage.value = ''
  submitting.value = true
  try {
    const creds = { username: adminUsername.value, password: adminPassword.value }
    await setupApi.setup(creds)
    await auth.login(creds.username, creds.password)
    goTo(1)
  } catch (err) {
    if (err instanceof ApiError && err.status === 404) {
      // 用户表非空（引导已完成，如 CLI 已建管理员）：不在此建，回落登录。
      auth.dismissSetupFlow()
      await router.replace({ name: 'login' })
      return
    }
    errorMessage.value = describeSubmitError(err)
  } finally {
    submitting.value = false
  }
}

/** Agent 步：`POST /agents` 建条目，展示一次性注册码 + per-Agent token
 *  （明文仅此一次）与按目标 OS 的复制即用注册命令。建完后停留本步供
 *  复制/核对，点「下一步」才进项目步（token/注册码永不二次展示）。 */
async function createAgent(): Promise<void> {
  errorMessage.value = ''
  submitting.value = true
  try {
    const created = await agentsApi.create({ name: agentName.value.trim() || 'build-1' })
    agentCreds.value = {
      token: created.token,
      registerCode: created.register_code,
      agentName: created.agent.name,
    }
  } catch (err) {
    errorMessage.value = describeSubmitError(err)
  } finally {
    submitting.value = false
  }
}

/** 项目步：`POST /projects` 建首个项目（git/svn）。 */
async function createProject(): Promise<void> {
  errorMessage.value = ''
  submitting.value = true
  try {
    await projectsApi.create({
      name: projectName.value.trim() || 'my-project',
      scm_type: scmType.value,
      scm_url: scmUrl.value.trim(),
      default_branch: defaultBranch.value.trim() || null,
    })
    finishSetup()
  } catch (err) {
    errorMessage.value = describeSubmitError(err)
  } finally {
    submitting.value = false
  }
}

/** 结束引导：显式离开即视为引导完成（ADR-0010 可跳过），进首页。 */
function finishSetup(): void {
  auth.dismissSetupFlow()
  void router.replace({ name: 'overview' })
}

/** 跳过当前步骤（不执行对应请求）。Agent 步跳过即不建条目、无注册码。 */
function skip(): void {
  if (isLastStep.value) {
    finishSetup()
  } else {
    goTo(step.value + 1)
  }
}

/**
 * 各步 CLI 等价命令（ADR-0010「headless 等价」：跑过即视为引导完成）。
 * 语言中立（命令本身不翻译），故直接在此构建、不经 i18n 编译器（避免
 * JSON 花括号被 message compiler 当插值解析）。
 */
const cliAdminCommand =
  `curl -X POST http://<server>:8080/api/v1/auth/setup ` +
  `-H 'Content-Type: application/json' ` +
  `-d '{"username":"admin","password":"…"}'`
const cliProjectCommand =
  `curl -X POST http://<server>:8080/api/v1/projects ` +
  `-H 'Content-Type: application/json' -H 'Authorization: Bearer <PAT>' ` +
  `-d '{"name":"my-project","scm_type":"git","scm_url":"https://…"}'`

/**
 * 按目标 OS 生成复制即用注册命令（ADR-0010/0007）。命令形态抽到
 * `@/utils/agentCommand` 与 Agent 列表页建条目共用（不复制漂移）；本处仅
 * 注入建条目响应的一次性注册码。详见 `buildAgentRegisterCommand` 注释。
 */
function agentCommand(os: AgentTargetOs): string {
  return buildAgentRegisterCommand(os, agentCreds.value?.registerCode ?? '')
}

/** 复制当前目标 OS 的注册命令到剪贴板（不可用时回落选中态提示）。 */
async function copyCommand(): Promise<void> {
  const cmd = agentCommand(targetOs.value)
  try {
    await navigator.clipboard.writeText(cmd)
  } catch {
    // 剪贴板 API 不可用（非安全上下文等）：不打断流程，命令本就在框内可选。
  }
}
</script>

<template>
  <div class="setup-page">
    <h1>{{ t('app.name') }}</h1>
    <p class="setup-intro">{{ t('setup.intro') }}</p>

    <!-- 步骤进度（三步各自可跳过）。 -->
    <ol class="setup-steps">
      <li :class="{ now: step === 0, done: step > 0 }">
        <span class="setup-dot">{{ step > 0 ? '✓' : 1 }}</span>
        {{ t('setup.step1Title') }}
      </li>
      <li :class="{ now: step === 1, done: step > 1 }">
        <span class="setup-dot">{{ step > 1 ? '✓' : 2 }}</span>
        {{ t('setup.step2Title') }}
      </li>
      <li :class="{ now: step === 2, done: step > 2 }">
        <span class="setup-dot">{{ step > 2 ? '✓' : 3 }}</span>
        {{ t('setup.step3Title') }}
      </li>
    </ol>

    <form class="setup-card" @submit.prevent>
      <!-- 步骤 1：管理员。 -->
      <section v-if="step === 0">
        <h2>{{ t('setup.step1Title') }}</h2>
        <p class="setup-desc">{{ t('setup.step1Desc') }}</p>
        <label class="field">
          <span>{{ t('auth.username') }}</span>
          <input v-model="adminUsername" name="admin-username" autocomplete="username" />
        </label>
        <label class="field">
          <span>{{ t('auth.password') }}</span>
          <input v-model="adminPassword" type="password" name="admin-password" autocomplete="new-password" />
        </label>
        <pre class="setup-cli">{{ t('setup.cliLabel') }}<br /><code>{{ cliAdminCommand }}</code></pre>
      </section>

      <!-- 步骤 2：Agent（注册码 + token + 按 OS 复制命令）。 -->
      <section v-else-if="step === 1">
        <h2>{{ t('setup.step2Title') }}</h2>
        <p class="setup-desc">{{ t('setup.step2Desc') }}</p>

        <template v-if="agentCreds">
          <!-- 已建条目：一次性明文展示 + 按目标 OS 命令。 -->
          <div class="setup-creds" role="alert">
            <p class="setup-creds-title">{{ t('setup.credsOneTime') }}</p>
            <dl>
              <dt>{{ t('setup.agentNameLabel') }}</dt>
              <dd class="mono">{{ agentCreds.agentName }}</dd>
              <dt>{{ t('setup.registerCodeLabel') }}</dt>
              <dd class="mono">{{ agentCreds.registerCode }}</dd>
              <dt>{{ t('setup.agentTokenLabel') }}</dt>
              <dd class="mono">{{ agentCreds.token }}</dd>
            </dl>
            <p class="setup-creds-warn">{{ t('setup.credsWarn') }}</p>
          </div>

          <label class="field">
            <span>{{ t('setup.targetOs') }}</span>
            <select v-model="targetOs">
              <option value="linux">Linux / macOS</option>
              <option value="windows">Windows</option>
            </select>
          </label>

          <div class="setup-cmd">
            <code>{{ agentCommand(targetOs) }}</code>
            <button type="button" @click="copyCommand">
              {{ t('setup.copy') }}
            </button>
          </div>
          <p class="setup-cmd-note">{{ t('setup.cmdNote') }}</p>
        </template>

        <template v-else>
          <!-- 未建条目：表单 + 建条目动作。 -->
          <label class="field">
            <span>{{ t('setup.agentNameLabel') }}</span>
            <input v-model="agentName" name="agent-name" :placeholder="t('setup.agentNamePlaceholder')" />
          </label>
          <p class="setup-desc">{{ t('setup.agentHint') }}</p>
        </template>
      </section>

      <!-- 步骤 3：项目。 -->
      <section v-else>
        <h2>{{ t('setup.step3Title') }}</h2>
        <p class="setup-desc">{{ t('setup.step3Desc') }}</p>
        <label class="field">
          <span>{{ t('projects.name') }}</span>
          <input v-model="projectName" name="project-name" :placeholder="t('setup.projectNamePlaceholder')" />
        </label>
        <label class="field">
          <span>{{ t('projects.scmType') }}</span>
          <select v-model="scmType">
            <option value="git">git</option>
            <option value="svn">svn</option>
          </select>
        </label>
        <label class="field">
          <span>{{ t('projects.scmUrl') }}</span>
          <input v-model="scmUrl" name="project-url" :placeholder="t('projects.scmUrlPlaceholder')" />
        </label>
        <label v-if="scmType === 'git'" class="field">
          <span>{{ t('projects.defaultBranch') }}</span>
          <input v-model="defaultBranch" name="project-branch" :placeholder="t('projects.defaultBranchPlaceholder')" />
        </label>
        <pre class="setup-cli">{{ t('setup.cliLabel') }}<br /><code>{{ cliProjectCommand }}</code></pre>
      </section>

      <p v-if="errorMessage" class="setup-error" role="alert">{{ errorMessage }}</p>

      <div class="setup-actions">
        <button type="button" class="setup-skip" @click="skip">
          {{ isLastStep ? t('setup.finish') : t('setup.skip') }}
        </button>
        <button
          v-if="step === 1 && agentCreds"
          type="button"
          class="setup-primary"
          :disabled="submitting"
          @click="goTo(2)"
        >
          {{ t('setup.next') }}
        </button>
        <button
          v-else-if="step === 0"
          type="button"
          class="setup-primary"
          :disabled="submitting || !adminUsername.trim() || adminPassword.length < 8"
          @click="createAdmin"
        >
          {{ submitting ? t('setup.submitting') : t('setup.createAdmin') }}
        </button>
        <button
          v-else-if="step === 1"
          type="button"
          class="setup-primary"
          :disabled="submitting"
          @click="createAgent"
        >
          {{ submitting ? t('setup.submitting') : t('setup.createAgent') }}
        </button>
        <button
          v-else
          type="button"
          class="setup-primary"
          :disabled="submitting || !projectName.trim() || !scmUrl.trim()"
          @click="createProject"
        >
          {{ submitting ? t('setup.submitting') : t('setup.createProject') }}
        </button>
      </div>
    </form>

    <button type="button" class="setup-done" @click="finishSetup">
      {{ t('setup.doneLink') }}
    </button>
  </div>
</template>

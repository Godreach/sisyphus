<script setup lang="ts">
// 初始化引导 wizard（ADR-0010/0020，票 B4-T2；#89 Naive UI 迁移；#112 定稿
// 设计语言：全屏无侧栏、居中卡片、首屏品牌标识，首装体验达发布级）。
//
// 三步（管理员 -> 首个 Agent -> 首个项目）各自可跳过、均带 CLI 等价提示：
// - 管理员步：`POST /auth/setup` 建首个全局管理员（仅用户表为空时可用；
//   空库判定与进入条件由路由守卫经 `isSetupNeeded()` 探测）。提交成功即
//   自动登录（登录是独立一步——见 `api/auth.rs` setup handler 注释），
//   步骤完成后进入 Agent 步。
// - Agent 步：`POST /agents` 建首个 Agent 条目，响应含一次性注册码 +
//   per-Agent token（明文仅此一次，本步展示后即丢弃）；按目标 OS 生成
//   复制即用注册命令（`sisyphus-agent --server-url … --reg-key …`，
//   ADR-0010/ADR-0007）。跳过即不建。一次性凭据展示与构建机页建条目弹窗
//   同源（NAlert 警示 + NDescriptions 明文行 + 复制即用命令）。
// - 项目步：`POST /projects` 建首个项目（git/svn + 仓库 URL + 可选默认
//   分支）；跳过即不建。
//
// CLI 等价提示：每步头部给 headless/Docker 用户对应的命令行（跑过即视为
// 引导完成，ADR-0010「headless 等价」）。用户显式跳过全部步骤即视为引导
// 完成（dismiss），此后守卫不再把受保护页重定向回 /setup。

import { computed, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useI18n } from 'vue-i18n'
import {
  NSteps,
  NStep,
  NButton,
  NInput,
  NSelect,
  NAlert,
  NCode,
  NIcon,
  NForm,
  NFormItem,
  NDescriptions,
  NDescriptionsItem,
  useMessage,
  type StepsProps,
} from 'naive-ui'
import { ClipboardOutline } from '@vicons/ionicons5'

import { agentsApi, projectsApi, setupApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import { ApiError } from '@/api/http'
import { useAuthStore } from '@/stores/auth'
import { buildAgentRegisterCommand, type AgentTargetOs } from '@/utils/agentCommand'
import AuthCard from '@/components/base/AuthCard.vue'

const { t } = useI18n()
const router = useRouter()
const auth = useAuthStore()
const message = useMessage()

/** 目标 OS 选项（ADR-0010 发布矩阵：Windows 带 .exe；linux/macos 同形）。 */
const osOptions: { label: string; value: AgentTargetOs }[] = [
  { label: 'Linux / macOS', value: 'linux' },
  { label: 'Windows', value: 'windows' },
]
/** 仓库类型选项（git 默认带分支；svn 无默认分支输入）。 */
const scmTypeOptions = [
  { label: 'git', value: 'git' },
  { label: 'svn', value: 'svn' },
]

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

/** NSteps 各步骤状态映射。 */
function stepStatus(index: number): StepsProps['status'] {
  if (index < step.value) return 'finish'
  if (index === step.value) return 'process'
  return 'wait'
}

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
    message.success(t('setup.step1Title') + ' ✓')
    goTo(1)
  } catch (err) {
    if (err instanceof ApiError && err.status === 404) {
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
    message.success(t('setup.step2Title') + ' ✓')
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
    message.success(t('setup.step3Title') + ' ✓')
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

/** 复制文本到剪贴板。 */
async function copyToClipboard(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text)
    message.success(t('setup.copy') + ' ✓')
  } catch {
    // 剪贴板 API 不可用（非安全上下文等）：不打断流程。
  }
}
</script>

<template>
  <div class="setup-page">
    <AuthCard class="setup-card">
      <p class="setup-intro">{{ t('setup.intro') }}</p>

      <!-- 步骤进度（Naive UI NSteps）。 -->
      <n-steps :current="step + 1" class="setup-steps">
        <n-step :title="t('setup.step1Title')" :status="stepStatus(0)" />
        <n-step :title="t('setup.step2Title')" :status="stepStatus(1)" />
        <n-step :title="t('setup.step3Title')" :status="stepStatus(2)" />
      </n-steps>

      <!-- 步骤 1：管理员。 -->
      <section v-if="step === 0" class="setup-step">
        <h2 class="setup-step-title">{{ t('setup.step1Title') }}</h2>
        <p class="setup-desc">{{ t('setup.step1Desc') }}</p>

        <n-alert v-if="errorMessage" type="error" :title="errorMessage" role="alert" class="setup-alert" />

        <!-- CLI 等价（headless/Docker）：复制即用 curl。 -->
        <div class="setup-cli">
          <div class="setup-cli-head">
            <span class="setup-cli-label">{{ t('setup.cliLabel') }}</span>
            <n-button size="small" type="primary" @click="copyToClipboard(cliAdminCommand)">
              {{ t('setup.copy') }}
            </n-button>
          </div>
          <n-code :code="cliAdminCommand" language="bash" word-wrap />
        </div>

        <n-form label-placement="top" class="setup-form">
          <n-form-item :label="t('auth.username')">
            <n-input
              v-model:value="adminUsername"
              :input-props="{ name: 'admin-username', autocomplete: 'username' }"
            />
          </n-form-item>
          <n-form-item :label="t('auth.password')">
            <n-input
              v-model:value="adminPassword"
              type="password"
              show-password-on="mousedown"
              :input-props="{ name: 'admin-password', autocomplete: 'new-password' }"
            />
          </n-form-item>
        </n-form>
      </section>

      <!-- 步骤 2：Agent（注册码 + token + 按 OS 复制命令）。 -->
      <section v-else-if="step === 1" class="setup-step">
        <h2 class="setup-step-title">{{ t('setup.step2Title') }}</h2>
        <p class="setup-desc">{{ t('setup.step2Desc') }}</p>

        <n-alert v-if="errorMessage" type="error" :title="errorMessage" role="alert" class="setup-alert" />

        <template v-if="agentCreds">
          <!-- 已建条目：一次性明文展示（NAlert 警示 + NDescriptions 明文行）+
               按目标 OS 命令。与构建机页建条目弹窗同源（设计语言统一）。 -->
          <n-alert type="warning" :show-icon="true" :title="t('setup.credsOneTime')" class="setup-alert">
            {{ t('setup.credsWarn') }}
          </n-alert>
          <n-descriptions :column="1" size="small" bordered class="setup-creds-desc">
            <n-descriptions-item :label="t('setup.agentNameLabel')">
              <span class="mono">{{ agentCreds.agentName }}</span>
            </n-descriptions-item>
            <n-descriptions-item :label="t('setup.registerCodeLabel')">
              <n-code :code="agentCreds.registerCode" />
              <n-button
                size="tiny"
                quaternary
                type="primary"
                name="setup-copy-code"
                @click="copyToClipboard(agentCreds.registerCode)"
              >
                <template #icon><n-icon :component="ClipboardOutline" /></template>
              </n-button>
            </n-descriptions-item>
            <n-descriptions-item :label="t('setup.agentTokenLabel')">
              <n-code :code="agentCreds.token" />
              <n-button
                size="tiny"
                quaternary
                type="primary"
                name="setup-copy-token"
                @click="copyToClipboard(agentCreds.token)"
              >
                <template #icon><n-icon :component="ClipboardOutline" /></template>
              </n-button>
            </n-descriptions-item>
          </n-descriptions>

          <div class="setup-os-row">
            <span class="setup-field-label">{{ t('setup.targetOs') }}</span>
            <n-select
              v-model:value="targetOs"
              :options="osOptions"
              class="setup-os-select"
              :virtual-scroll="false"
            />
            <n-button size="small" type="primary" name="setup-copy-cmd" @click="copyToClipboard(agentCommand(targetOs))">
              {{ t('setup.copy') }}
            </n-button>
          </div>
          <n-code :code="agentCommand(targetOs)" language="bash" word-wrap class="setup-cmd-code" />
          <p class="form-hint">{{ t('setup.cmdNote') }}</p>
        </template>

        <template v-else>
          <!-- 未建条目：表单 + 建条目动作。 -->
          <n-form label-placement="top" class="setup-form">
            <n-form-item :label="t('setup.agentNameLabel')">
              <n-input
                v-model:value="agentName"
                :input-props="{ name: 'agent-name' }"
                :placeholder="t('setup.agentNamePlaceholder')"
              />
            </n-form-item>
          </n-form>
          <p class="setup-desc">{{ t('setup.agentHint') }}</p>
        </template>
      </section>

      <!-- 步骤 3：项目。 -->
      <section v-else class="setup-step">
        <h2 class="setup-step-title">{{ t('setup.step3Title') }}</h2>
        <p class="setup-desc">{{ t('setup.step3Desc') }}</p>

        <n-alert v-if="errorMessage" type="error" :title="errorMessage" role="alert" class="setup-alert" />

        <div class="setup-cli">
          <div class="setup-cli-head">
            <span class="setup-cli-label">{{ t('setup.cliLabel') }}</span>
            <n-button size="small" type="primary" @click="copyToClipboard(cliProjectCommand)">
              {{ t('setup.copy') }}
            </n-button>
          </div>
          <n-code :code="cliProjectCommand" language="bash" word-wrap />
        </div>

        <n-form label-placement="top" class="setup-form">
          <n-form-item :label="t('projects.name')">
            <n-input
              v-model:value="projectName"
              :input-props="{ name: 'project-name' }"
              :placeholder="t('setup.projectNamePlaceholder')"
            />
          </n-form-item>
          <n-form-item :label="t('projects.scmType')">
            <n-select v-model:value="scmType" :options="scmTypeOptions" />
          </n-form-item>
          <n-form-item :label="t('projects.scmUrl')">
            <n-input
              v-model:value="scmUrl"
              :input-props="{ name: 'project-url' }"
              :placeholder="t('projects.scmUrlPlaceholder')"
            />
          </n-form-item>
          <n-form-item v-if="scmType === 'git'" :label="t('projects.defaultBranch')">
            <n-input
              v-model:value="defaultBranch"
              :input-props="{ name: 'project-branch' }"
              :placeholder="t('projects.defaultBranchPlaceholder')"
            />
          </n-form-item>
        </n-form>
      </section>

      <div class="setup-actions">
        <n-button @click="skip">
          {{ isLastStep ? t('setup.finish') : t('setup.skip') }}
        </n-button>

        <n-button
          v-if="step === 1 && agentCreds"
          type="primary"
          @click="goTo(2)"
        >
          {{ t('setup.next') }}
        </n-button>
        <n-button
          v-else-if="step === 0"
          type="primary"
          :disabled="submitting || !adminUsername.trim() || adminPassword.length < 8"
          :loading="submitting"
          @click="createAdmin"
        >
          {{ submitting ? t('setup.submitting') : t('setup.createAdmin') }}
        </n-button>
        <n-button
          v-else-if="step === 1"
          type="primary"
          :disabled="submitting"
          :loading="submitting"
          @click="createAgent"
        >
          {{ submitting ? t('setup.submitting') : t('setup.createAgent') }}
        </n-button>
        <n-button
          v-else
          type="primary"
          :disabled="submitting || !projectName.trim() || !scmUrl.trim()"
          :loading="submitting"
          @click="createProject"
        >
          {{ submitting ? t('setup.submitting') : t('setup.createProject') }}
        </n-button>
      </div>
    </AuthCard>

    <n-button quaternary type="primary" class="setup-done" @click="finishSetup">
      {{ t('setup.doneLink') }}
    </n-button>
  </div>
</template>

<style scoped>
/* 全屏无侧栏、居中卡片（app-bare 内 app-main 已是纵向 flex 容器，本根用
   margin:auto 居中且高于视口时不裁剪顶部——可上滚；引导内容偏高故宽度较
   登录页略宽）。卡片外壳与品牌标识由 AuthCard base 组件提供（票 #112，
   ADR-0023）；本 scoped 仅留引导页专属。 */
.setup-page {
  margin: auto;
  width: 100%;
  max-width: 600px;
  padding: 24px 16px;
}

.setup-intro {
  margin: 0 0 16px;
  color: var(--sisy-color-text-secondary);
  font-size: 13px;
  line-height: 1.6;
}

.setup-steps {
  margin: 0 0 20px;
}

/* 各步正文区。 */
.setup-step {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.setup-step-title {
  margin: 0;
  font-size: 15px;
  font-weight: 600;
  color: var(--sisy-color-text);
}

.setup-desc {
  margin: 0;
  color: var(--sisy-color-text-secondary);
  font-size: 13px;
  line-height: 1.6;
}

.setup-alert {
  margin: 0;
}

/* CLI 等价命令块：标签行 + 复制按钮 + NCode。 */
.setup-cli {
  display: flex;
  flex-direction: column;
  gap: 8px;
  background: var(--sisy-color-bg);
  border: 1px solid var(--sisy-color-border);
  border-radius: var(--sisy-radius);
  padding: 12px;
}

.setup-cli-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}

.setup-cli-label {
  font-size: 12px;
  color: var(--sisy-color-text-secondary);
  line-height: 1.4;
}

.setup-form {
  margin: 0;
}

/* 一次性凭据明文行（NDescriptions 描边表）。 */
.setup-creds-desc {
  margin: 0;
}

.mono {
  font-family: ui-monospace, 'Cascadia Code', Consolas, monospace;
  font-size: 12px;
  word-break: break-all;
}

/* 目标 OS 选择 + 命令复制行。 */
.setup-os-row {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}

.setup-field-label {
  font-size: 13px;
  color: var(--sisy-color-text-secondary);
}

.setup-os-select {
  width: 180px;
}

.setup-cmd-code {
  margin: 0;
}

/* 动作行：跳过 / 主按钮（建号·建条目·建项目·下一步）。 */
.setup-actions {
  display: flex;
  justify-content: space-between;
  align-items: center;
  border-top: 1px solid var(--sisy-color-border);
  padding-top: 16px;
  gap: 12px;
  margin-top: 20px;
}

/* 底部「全部跳过」退出链接（卡片外，居中）。 */
.setup-done {
  margin: 16px auto 0;
  display: flex;
}

/* 窄屏：页面根内边距收紧（卡片内边距由 AuthCard 自适应）；OS 选择行折行。 */
@media (max-width: 767px) {
  .setup-page {
    padding: 16px 12px;
  }

  .setup-os-select {
    width: 100%;
  }
}
</style>

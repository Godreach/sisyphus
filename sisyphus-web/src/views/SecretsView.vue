<script setup lang="ts">
// 机密管理页（ADR-0015，spec #110 定稿铺开）：项目机密「只记名不记值」
// 纪律在 UI 成立。设计语言与三主页面/项目详情同源——页面头 + sisy-card
// 卡片区（机密名清单行内删除）+ NModal 写/覆写表单（建条目动作收进页头
// 与空态，票 #106/#108 同形态）。
//
// 管理区全局 admin 面（用户卡弹出菜单入口 + 路由守卫兜底）。机密端点为
// 项目 admin 档——全局 admin 隐含全部项目的项目 admin（ADR-0014），故本页
// 以项目下拉选择任一项目后管理其机密。
//
// - 列名：`GET /projects/{name}/secrets` → 仅名清单（值形态任何端点不回显）。
// - 写/覆写：`PUT /projects/{name}/secrets/{secret}` { value }（同名即覆写，
//   成功 204 无值形态）；机密名取自路径段，env 键字符集（字母数字 + `_`），
//   非法名 422。
// - 删：`DELETE /projects/{name}/secrets/{secret}`（名消失即可观察语义）。
// - 语义提示：值只写不读、永不可读回；`${}` 插值不解析机密值（防进命令串/
//   日志回显）；任务 env 键与机密名冲突在 pipeline 保存时校验。
//
// 事实态纪律：首载骨架屏、清单失败整页报错 + 重试、删除失败 toast 行内
// 感知、写失败弹窗内报错。403 说明：本页经路由守卫 `meta.admin` 门控，
// 仅全局 admin 可达；全局 admin 隐含项目 admin——对可达会话不会 403，
// 故不设退化分支（避免不可达死代码），错误统一经 describeSubmitError 就地展示。

import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { NAlert, NButton, NEmpty, NForm, NFormItem, NIcon, NInput, NModal, NPopconfirm, NSelect, NSkeleton, useMessage } from 'naive-ui'
import { AddOutline } from '@vicons/ionicons5'

import { projectsApi, secretsApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import type { ProjectResponse } from '@/api/types'

const { t } = useI18n()
const message = useMessage()

const projects = ref<ProjectResponse[] | null>(null)
const projectError = ref('')
const loadingProjects = ref(true)
const selectedProject = ref('')

const secrets = ref<string[] | null>(null)
const secretsError = ref('')
const loadingSecrets = ref(false)

/** 写/覆写弹窗（open 即表单；提交成功关闭并刷新清单）。 */
const writeOpen = ref(false)
const newName = ref('')
const newValue = ref('')
const saving = ref(false)
const saveError = ref('')

/** 删除 busy（按名标记，按钮转圈）。 */
const deletingName = ref<string | null>(null)

onMounted(loadProjects)

const canSave = computed(
  () => newName.value.trim() !== '' && newValue.value !== '' && !saving.value,
)

const projectOptions = computed(() =>
  (projects.value ?? []).map((p) => ({ label: p.name, value: p.name })),
)

/** 机密名清单卡副标（计数；与构建机页 card-subtitle 同形态）。 */
const countText = computed(() =>
  secrets.value != null ? t('secrets.count', { n: secrets.value.length }) : '',
)

/** 项目加载失败重试（整页报错 + 重试，事实态纪律）。 */
function retryProjects(): void {
  void loadProjects()
}

/** 项目下拉：全局 admin 全量；选首个为默认。无项目 → 空态。 */
async function loadProjects(): Promise<void> {
  loadingProjects.value = true
  projectError.value = ''
  try {
    const list = await projectsApi.list()
    projects.value = list
    if (selectedProject.value === '' && list.length > 0) {
      selectedProject.value = list[0]!.name
    }
  } catch (err) {
    projects.value = null
    projectError.value = describeSubmitError(err)
  } finally {
    loadingProjects.value = false
  }
}

/** 加载所选项目的机密名清单（值形态任何端点不回显）。 */
async function loadSecrets(): Promise<void> {
  if (selectedProject.value === '') {
    secrets.value = null
    return
  }
  loadingSecrets.value = true
  secretsError.value = ''
  try {
    const names = await secretsApi.list(selectedProject.value)
    secrets.value = names.map((n) => n.name)
  } catch (err) {
    secrets.value = null
    secretsError.value = describeSubmitError(err)
  } finally {
    loadingSecrets.value = false
  }
}

watch(selectedProject, () => {
  // 初始 loadProjects 设置 selectedProject 会触发本 watch 完成首次加载；
  // 手动切换同样经此路径（切换重置写表单由 writeOpen watch 兜底）。
  void loadSecrets()
})

// ===== 写/覆写弹窗 =====

/** 打开弹窗：清空上次输入与错误（预填无——机密值永不可读回，没有「编辑」）。 */
function openWrite(): void {
  newName.value = ''
  newValue.value = ''
  saveError.value = ''
  writeOpen.value = true
}

/** 写/覆写机密：`PUT .../secrets/{secret}` { value }，成功 204 + 刷新列表。
 *  机密名取自路径段（env 键字符集）；值在请求体只写不读。 */
async function saveSecret(): Promise<void> {
  saveError.value = ''
  saving.value = true
  try {
    await secretsApi.put(selectedProject.value, newName.value.trim(), {
      value: newValue.value,
    })
    writeOpen.value = false
    newName.value = ''
    newValue.value = ''
    message.success(t('secrets.saved'))
    await loadSecrets()
  } catch (err) {
    saveError.value = describeSubmitError(err)
  } finally {
    saving.value = false
  }
}

/** 删除机密：`DELETE .../secrets/{secret}`，成功 204 + 刷新列表。失败 toast
 *  行内感知（只影响该行，不整页报错——与构建机开关同纪律）。 */
async function deleteSecret(name: string): Promise<void> {
  deletingName.value = name
  try {
    await secretsApi.delete(selectedProject.value, name)
    message.success(t('secrets.deleted'))
    await loadSecrets()
  } catch (err) {
    message.error(describeSubmitError(err))
  } finally {
    deletingName.value = null
  }
}

/** 切换项目：重置表单残留（弹窗未开时无感；开着时内容随项目失效）。 */
function changeProject(): void {
  saveError.value = ''
}
</script>

<template>
  <div class="admin-page secrets-page">
    <!-- 项目清单失败：整页报错 + 重试（事实态纪律）。 -->
    <n-alert v-if="projectError" type="error" :title="projectError" role="alert">
      <n-button size="small" name="secrets-retry" @click="retryProjects">
        {{ t('secrets.retry') }}
      </n-button>
    </n-alert>

    <!-- 首载骨架屏（数据到达后替换）。 -->
    <div v-else-if="loadingProjects" class="secrets-skeleton" data-testid="secrets-skeleton">
      <n-skeleton text height="32px" width="220px" class="secrets-skeleton-row" />
      <n-skeleton text :repeat="4" height="44px" class="secrets-skeleton-row" />
    </div>

    <!-- 无项目空态：引导先建项目（机密挂在项目下）。 -->
    <div v-else-if="projects && projects.length === 0" class="secrets-empty-page">
      <n-empty :description="t('secrets.noProjects')" />
    </div>

    <template v-else-if="projects">
      <!-- 页头：项目选择 + 写/覆写动作（建条目收进页头，票 #106/#108 形态）。 -->
      <header class="page-header secrets-header">
        <div class="secrets-project-row">
          <span class="secrets-toolbar-label">{{ t('secrets.project') }}</span>
          <n-select
            v-model:value="selectedProject"
            :options="projectOptions"
            class="secrets-project-select"
            :virtual-scroll="false"
            @update:value="changeProject"
          />
        </div>
        <button type="button" class="btn-outline blue" name="secret-new" @click="openWrite">
          <n-icon :component="AddOutline" />
          {{ t('secrets.writeTitle') }}
        </button>
      </header>

      <!-- 机密名清单卡（原型 sisy-card 表形态：表头 + 分隔行 + mono 名）。 -->
      <section class="sisy-card secrets-table-card" aria-label="secret names">
        <div class="card-header">
          <div>
            <h2 class="card-title">{{ t('secrets.listTitle') }}</h2>
            <div v-if="countText" class="card-subtitle">{{ countText }}</div>
          </div>
        </div>

        <!-- 首载/切换加载骨架屏（数据到达后替换）。 -->
        <div v-if="loadingSecrets" class="card-skeleton">
          <n-skeleton text :repeat="3" height="40px" />
        </div>

        <n-alert v-else-if="secretsError" type="error" :title="secretsError" role="alert" class="card-alert">
          <n-button size="small" name="secrets-list-retry" @click="loadSecrets">
            {{ t('secrets.retry') }}
          </n-button>
        </n-alert>

        <div v-else-if="secrets && secrets.length === 0" class="secrets-empty">
          <n-empty :description="t('secrets.empty')">
            <template #extra>
              <p class="form-hint">{{ t('secrets.emptyHint') }}</p>
              <n-button type="primary" size="small" class="secrets-empty-btn" name="secret-new-empty" @click="openWrite">
                {{ t('secrets.writeTitle') }}
              </n-button>
            </template>
          </n-empty>
        </div>

        <!-- 机密名清单（无值列——write-only 语义，值任何端点不回显）。 -->
        <template v-else-if="secrets">
          <div class="secrets-thead">
            <span>{{ t('secrets.name') }}</span>
            <span class="secrets-thead-actions" />
          </div>
          <div v-for="name in secrets" :key="name" class="secrets-row" :data-testid="`secret-row-${name}`">
            <span class="mono secret-name">{{ name }}</span>
            <div class="secrets-row-actions">
              <!-- 删除经原生气泡确认（危险操作 NPopconfirm——Agent 详情/用户页同纪律），
                   确认文案完整传递不可恢复语义。 -->
              <n-popconfirm
                :positive-text="t('common.confirm')"
                :negative-text="t('common.cancel')"
                @positive-click="deleteSecret(name)"
              >
                <template #trigger>
                  <button
                    type="button"
                    class="btn-outline red"
                    name="secret-delete"
                    :data-testid="`secret-delete-${name}`"
                    :disabled="deletingName === name"
                  >
                    {{ deletingName === name ? t('secrets.deleting') : t('secrets.delete') }}
                  </button>
                </template>
                {{ t('secrets.deleteConfirm', { name }) }}
              </n-popconfirm>
            </div>
          </div>

          <!-- 语义提示：值只写不读 + ${} 不解析 + env 键冲突（${} 经具名参数
               传入，避免 vue-i18n 把字面量 ${} 当空占位符编译）。 -->
          <p class="form-hint secrets-discipline">{{ t('secrets.discipline', { interp: '${}' }) }}</p>
        </template>
      </section>
    </template>

    <!-- 写/覆写弹窗（名 + 值；值永不可读回，故无「当前值」回填）。 -->
    <n-modal
      v-model:show="writeOpen"
      preset="card"
      :title="t('secrets.writeTitle')"
      style="width: 480px"
      :bordered="false"
    >
      <n-form label-placement="top" @submit.prevent="saveSecret">
        <n-form-item :label="t('secrets.name')" :show-require-mark="true">
          <n-input
            v-model:value="newName"
            :input-props="{ name: 'secret-name' }"
            :placeholder="t('secrets.namePlaceholder')"
          />
        </n-form-item>
        <p class="form-hint">{{ t('secrets.nameHint') }}</p>
        <n-form-item :label="t('secrets.value')">
          <n-input
            v-model:value="newValue"
            type="textarea"
            :rows="3"
            :input-props="{ name: 'secret-value' }"
            :placeholder="t('secrets.valuePlaceholder')"
          />
        </n-form-item>
        <p class="form-hint">{{ t('secrets.valueHint') }}</p>
        <n-alert v-if="saveError" type="error" :title="saveError" role="alert" class="secrets-modal-alert" />
        <div class="modal-actions">
          <n-button @click="writeOpen = false">{{ t('common.cancel') }}</n-button>
          <n-button
            type="primary"
            name="secret-save"
            :disabled="!canSave"
            :loading="saving"
            @click="saveSecret"
          >
            {{ saving ? t('secrets.saving') : t('secrets.save') }}
          </n-button>
        </div>
      </n-form>
    </n-modal>
  </div>
</template>

<style scoped>
.secrets-page {
  gap: 16px;
}

.secrets-skeleton {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.secrets-skeleton-row {
  width: 100%;
}

.secrets-empty-page {
  padding: 48px 0;
}

/* 页头：项目选择 + 写/覆写动作。 */
.secrets-header {
  align-items: center;
  flex-wrap: wrap;
}

.secrets-project-row {
  display: flex;
  align-items: center;
  gap: 10px;
  min-width: 0;
}

.secrets-toolbar-label {
  font-size: 13px;
  color: var(--sisy-color-text-secondary);
}

.secrets-project-select {
  width: 260px;
}

/* 清单卡副标（计数）。 */
.card-subtitle {
  font-size: 12px;
  color: var(--sisy-color-text-secondary);
  margin-top: 2px;
}

.card-alert {
  margin: 0 20px 16px;
}

.card-alert button {
  margin-top: 8px;
}

.card-skeleton {
  padding: 0 20px 16px;
}

/* 表头（原型 table-head 形态）。 */
.secrets-thead {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 20px;
  height: 40px;
  border-top: 1px solid var(--sisy-color-border);
  border-bottom: 1px solid var(--sisy-color-border);
}

.secrets-thead span {
  font-size: 12px;
  font-weight: 500;
  color: var(--sisy-color-text-secondary);
}

.secrets-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 0 20px;
  min-height: 48px;
  border-bottom: 1px solid var(--sisy-color-border-light);
  transition: background 0.15s;
}

.secrets-row:last-of-type {
  border-bottom: none;
}

.secrets-row:hover {
  background: var(--sisy-color-bg);
}

.mono {
  font-family: ui-monospace, 'Cascadia Code', Consolas, monospace;
  font-size: 12px;
  word-break: break-all;
}

.secrets-row-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
}

/* 语义提示收在清单尾部（信息就地可读，不与表单耦合）。 */
.secrets-discipline {
  padding: 12px 20px 16px;
  margin: 0;
  line-height: 1.6;
  max-width: 720px;
}

/* 空态。 */
.secrets-empty {
  padding: 24px 0 32px;
}

.secrets-empty-btn {
  margin-top: 12px;
}

/* 弹窗。 */
.secrets-modal-alert {
  margin-bottom: 8px;
}

.modal-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 8px;
}
</style>

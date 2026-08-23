<script setup lang="ts">
// 机密管理页（ADR-0015，票 B4-T6）：项目机密「只记名不记值」纪律在 UI 成立。
//
// 管理区全局 admin 面（侧栏 is_admin 门控 + 路由守卫兜底）。机密端点为项目
// admin 档——全局 admin 隐含全部项目的项目 admin（ADR-0014），故本页以项目
// 下拉选择任一项目后管理其机密。
//
// - 列名：`GET /projects/{name}/secrets` → 仅名清单（值形态任何端点不回显）。
// - 写/覆写：`PUT /projects/{name}/secrets/{secret}` { value }（同名即覆写，
//   成功 204 无值形态）；机密名取自路径段，env 键字符集（字母数字 + `_`），
//   非法名 422。
// - 删：`DELETE /projects/{name}/secrets/{secret}`（名消失即可观察语义）。
// - 语义提示：值只写不读、永不可读回；`${}` 插值不解析机密值（防进命令串/
//   日志回显）；任务 env 键与机密名冲突在 pipeline 保存时校验。
// #95: 使用 Naive UI 组件重写——项目下拉改 NSelect、写/覆写表单改 NCard +
// NForm、机密名清单改 NDataTable（删除经 NPopconfirm 确认）、成功操作改
// NMessage toast、错误态 NAlert、加载 NSkeleton、空态 NEmpty。

import { computed, h, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  NAlert,
  NButton,
  NCard,
  NDataTable,
  NEmpty,
  NForm,
  NFormItem,
  NInput,
  NPopconfirm,
  NSelect,
  NSkeleton,
  useMessage,
  type DataTableColumns,
} from 'naive-ui'

import { projectsApi, secretsApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import type { ProjectResponse } from '@/api/types'

/** 机密名行（NDataTable 行形态：清单端点只回名，无值列——write-only 语义）。 */
interface SecretRow {
  name: string
}

const { t } = useI18n()
const message = useMessage()

const projects = ref<ProjectResponse[] | null>(null)
const projectError = ref('')
const selectedProject = ref('')

const secrets = ref<string[] | null>(null)
const secretsError = ref('')
const loadingSecrets = ref(false)

/** 写/覆写表单。 */
const newName = ref('')
const newValue = ref('')
const saving = ref(false)
const saveError = ref('')

/** 删除 busy（按名标记，按钮转圈）。 */
const deletingName = ref<string | null>(null)

onMounted(loadProjects)

const canSave = computed(
  () => selectedProject.value !== '' && newName.value.trim() !== '' && newValue.value !== '' && !saving.value,
)

const projectOptions = computed(() =>
  (projects.value ?? []).map((p) => ({ label: p.name, value: p.name })),
)

const secretRows = computed<SecretRow[]>(() =>
  (secrets.value ?? []).map((name) => ({ name })),
)

// 403 退化态说明：本页经侧栏 `is_admin` 门控 + 路由守卫 `meta.admin` 兜底，
// 仅全局 admin 可达；而全局 admin 隐含全部项目的项目 admin 权限（ADR-0014），
// 机密端点为项目 admin 档——故对可达会话不会 403。三姊妹管理页（审计 /
// Agent 升级 / 用户）端点为全局 admin 专属，is_admin 被撤销会 403 → 各自
// 带 `adminOnly` 退化态；本页 403 不可达，故不加该分支（避免不可达死代码），
// 错误统一经下方 catch 落 `describeSubmitError` 就地展示。

/** NDataTable 列（机密名 mono + 删除 NPopconfirm；无值列——值任何端点不回显）。 */
const columns = computed<DataTableColumns<SecretRow>>(() => [
  {
    title: t('secrets.name'),
    key: 'name',
    render: (row) => h('span', { class: 'mono' }, row.name),
  },
  {
    title: '',
    key: 'actions',
    width: 110,
    render: (row) =>
      h(
        NPopconfirm,
        {
          positiveText: t('common.confirm'),
          negativeText: t('common.cancel'),
          onPositiveClick: () => void deleteSecret(row.name),
        },
        {
          trigger: () =>
            h(
              NButton,
              {
                size: 'small',
                name: 'secret-delete',
                loading: deletingName.value === row.name,
              },
              { default: () => t('secrets.delete') },
            ),
          default: () => t('secrets.deleteConfirm', { name: row.name }),
        },
      ),
  },
])

const rowKey = (row: SecretRow): string => row.name

/** 项目下拉：全局 admin 全量；选首个为默认。无项目 → 空态。 */
async function loadProjects(): Promise<void> {
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

/** 写/覆写机密：`PUT .../secrets/{secret}` { value }，成功 204 + 刷新列表。 */
async function saveSecret(): Promise<void> {
  saveError.value = ''
  saving.value = true
  try {
    await secretsApi.put(selectedProject.value, newName.value.trim(), {
      value: newValue.value,
    })
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

/** 删除机密：`DELETE .../secrets/{secret}`，成功 204 + 刷新列表。 */
async function deleteSecret(name: string): Promise<void> {
  deletingName.value = name
  try {
    await secretsApi.delete(selectedProject.value, name)
    message.success(t('secrets.deleted'))
    await loadSecrets()
  } catch (err) {
    secretsError.value = describeSubmitError(err)
  } finally {
    deletingName.value = null
  }
}

/** 切换项目：重置表单。机密清单的重新加载由 `watch(selectedProject)` 驱动
 *  （v-model 改值即触发），此处不重复发请求。 */
function changeProject(): void {
  newName.value = ''
  newValue.value = ''
  saveError.value = ''
}

watch(selectedProject, () => {
  // 初始 loadProjects 设置 selectedProject 会触发本 watch 完成首次加载；
  // 手动切换同样经此路径（changeProject 仅重置表单，加载由 watch 驱动）。
  void loadSecrets()
})
</script>

<template>
  <div class="admin-page secrets-page">
    <div class="page-header">
      <h1 class="page-title">{{ t('routes.adminSecrets') }}</h1>
    </div>

    <n-alert v-if="projectError" type="error" :title="projectError" role="alert" />

    <!-- 无项目空态：引导先建项目（机密挂在项目下）。 -->
    <p v-else-if="projects && projects.length === 0" class="form-hint">{{ t('secrets.noProjects') }}</p>

    <template v-else-if="projects">
      <div class="secrets-toolbar">
        <span class="secrets-toolbar-label">{{ t('secrets.project') }}</span>
        <n-select
          v-model:value="selectedProject"
          :options="projectOptions"
          class="secrets-project-select"
          :virtual-scroll="false"
          @update:value="changeProject"
        />
      </div>

      <!-- 写/覆写表单（名 + 值；值永不可读回，故无「当前值」回填）。 -->
      <n-card :title="t('secrets.writeTitle')" size="small" class="secrets-form-card">
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
          <div class="secrets-form-actions">
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
          <n-alert v-if="saveError" type="error" :title="saveError" role="alert" class="secrets-form-alert" />
        </n-form>
      </n-card>

      <!-- 语义提示：值只写不读 + ${} 不解析 + env 键冲突（${} 经具名参数传入，
           避免 vue-i18n 把字面量 ${} 当空占位符编译）。 -->
      <p class="form-hint secrets-discipline">{{ t('secrets.discipline', { interp: '${}' }) }}</p>

      <h2 class="secrets-list-title">{{ t('secrets.listTitle') }}</h2>

      <!-- 首载/切换加载骨架屏（数据到达后替换）。 -->
      <div v-if="loadingSecrets" class="secrets-skeleton">
        <n-skeleton v-for="i in 3" :key="i" text :repeat="1" height="28px" class="secrets-skeleton-row" />
      </div>

      <n-alert v-else-if="secretsError" type="error" :title="secretsError" role="alert" />

      <div v-else-if="secrets && secrets.length === 0" class="secrets-empty">
        <n-empty :description="t('secrets.empty')" />
      </div>

      <!-- 机密名清单（无值列——write-only 语义，值任何端点不回显）。 -->
      <n-data-table
        v-else-if="secrets"
        :columns="columns"
        :data="secretRows"
        :row-key="rowKey"
        :bordered="false"
        :single-line="true"
        size="small"
        :scroll-x="420"
        class="secrets-table"
      />
    </template>
  </div>
</template>

<style scoped>
.secrets-toolbar {
  display: flex;
  align-items: center;
  gap: 10px;
}

.secrets-toolbar-label {
  font-size: 14px;
  color: var(--n-text-color-3, #7f8792);
}

.secrets-project-select {
  width: 260px;
}

.secrets-form-card {
  max-width: 560px;
}

.secrets-form-actions {
  display: flex;
  gap: 8px;
  margin-top: 4px;
}

.secrets-form-alert {
  margin-top: 8px;
}

.secrets-discipline {
  max-width: 640px;
  line-height: 1.6;
}

.secrets-list-title {
  margin: 8px 0 0;
  font-size: 16px;
}

.secrets-skeleton {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin: 8px 0;
}

.secrets-skeleton-row {
  width: 100%;
}

.secrets-empty {
  padding: 24px 0;
}

.secrets-table {
  max-width: 560px;
}

.mono {
  font-family: ui-monospace, 'Cascadia Code', Consolas, monospace;
  font-size: 12px;
  word-break: break-all;
}
</style>

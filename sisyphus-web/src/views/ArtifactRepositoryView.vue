<script setup lang="ts">
// 一级制品库入口（票 #122，ADR-0026）：未配置 S3 时明确不可用；
// 已配置时展示脱敏后端摘要。浏览/下载属后续票；本页只做状态与管理员连接自检。

import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { NAlert, NButton, NEmpty, NIcon, NResult, NSkeleton, useMessage } from 'naive-ui'
import { RefreshOutline } from '@vicons/ionicons5'

import { artifactRepositoryApi, s3ConfigApi } from '@/api/client'
import { describeSubmitError } from '@/api/errors'
import type { ArtifactRepositoryStatus, S3TestReportDto } from '@/api/types'
import { useAuthStore } from '@/stores/auth'

const { t } = useI18n()
const message = useMessage()
const auth = useAuthStore()
const isAdmin = computed(() => auth.user?.isAdmin === true)

const loading = ref(true)
const errorMessage = ref('')
const status = ref<ArtifactRepositoryStatus | null>(null)

const testing = ref(false)
const report = ref<S3TestReportDto | null>(null)
const testError = ref('')

onMounted(() => {
  void loadStatus()
})

async function loadStatus(): Promise<void> {
  loading.value = true
  errorMessage.value = ''
  try {
    status.value = await artifactRepositoryApi.status()
  } catch (err) {
    errorMessage.value = describeSubmitError(err)
    status.value = null
  } finally {
    loading.value = false
  }
}

const canTest = computed(
  () => isAdmin.value && status.value?.available === true && !testing.value,
)

async function testConnection(): Promise<void> {
  testing.value = true
  testError.value = ''
  report.value = null
  try {
    const result = await s3ConfigApi.testConnection()
    report.value = result
    if (result.ok) {
      message.success(t('artifacts.testOk'))
    } else {
      message.error(t('artifacts.testFail'))
    }
  } catch (err) {
    testError.value = describeSubmitError(err)
    message.error(testError.value)
  } finally {
    testing.value = false
  }
}
</script>

<template>
  <div class="artifact-repo-page" data-testid="artifact-repo-page">
    <n-skeleton v-if="loading" data-testid="artifact-repo-skeleton" text :repeat="4" />

    <n-alert
      v-else-if="errorMessage"
      type="error"
      data-testid="artifact-repo-error"
      :title="t('artifacts.loadError')"
    >
      <p>{{ errorMessage }}</p>
      <n-button size="small" @click="loadStatus">
        <template #icon>
          <n-icon :component="RefreshOutline" />
        </template>
        {{ t('artifacts.retry') }}
      </n-button>
    </n-alert>

    <n-result
      v-else-if="status && !status.available"
      status="warning"
      data-testid="artifact-repo-unavailable"
      :title="t('artifacts.unavailableTitle')"
      :description="t('artifacts.unavailableDesc')"
    />

    <div v-else-if="status?.available" class="artifact-repo-ready" data-testid="artifact-repo-available">
      <h2 class="artifact-repo-title">{{ t('artifacts.availableTitle') }}</h2>
      <p class="artifact-repo-desc">{{ t('artifacts.availableDesc') }}</p>
      <dl v-if="status.backend" class="artifact-repo-backend">
        <div>
          <dt>{{ t('artifacts.endpoint') }}</dt>
          <dd>{{ status.backend.endpoint }}</dd>
        </div>
        <div>
          <dt>{{ t('artifacts.region') }}</dt>
          <dd>{{ status.backend.region }}</dd>
        </div>
        <div>
          <dt>{{ t('artifacts.bucket') }}</dt>
          <dd>{{ status.backend.bucket }}</dd>
        </div>
        <div>
          <dt>{{ t('artifacts.prefix') }}</dt>
          <dd>{{ status.backend.prefix || t('artifacts.prefixEmpty') }}</dd>
        </div>
      </dl>
      <n-button
        v-if="isAdmin"
        type="primary"
        data-testid="artifact-repo-test"
        :loading="testing"
        :disabled="!canTest"
        @click="testConnection"
      >
        {{ t('artifacts.testConnection') }}
      </n-button>
      <n-alert v-if="testError" type="error" style="margin-top: 16px">{{ testError }}</n-alert>
      <ul v-if="report" class="artifact-repo-checks" data-testid="artifact-repo-report">
        <li v-for="check in report.checks" :key="check.op">
          {{ check.op }}: {{ check.ok ? t('artifacts.checkOk') : t('artifacts.checkFail') }}
          <span v-if="check.detail"> — {{ check.detail }}</span>
        </li>
      </ul>
    </div>

    <n-empty v-else :description="t('artifacts.loadError')" />
  </div>
</template>

<style scoped>
.artifact-repo-page {
  padding: 24px;
  max-width: 720px;
}
.artifact-repo-title {
  margin: 0 0 8px;
  font-size: 20px;
  font-weight: 600;
}
.artifact-repo-desc {
  margin: 0 0 16px;
  color: var(--n-text-color-3, #86868b);
}
.artifact-repo-backend {
  display: grid;
  gap: 8px 24px;
  margin: 0 0 20px;
  grid-template-columns: auto 1fr;
}
.artifact-repo-backend > div {
  display: contents;
}
.artifact-repo-backend dt {
  color: var(--n-text-color-3, #86868b);
}
.artifact-repo-backend dd {
  margin: 0;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
.artifact-repo-checks {
  margin: 16px 0 0;
  padding-left: 20px;
}
</style>
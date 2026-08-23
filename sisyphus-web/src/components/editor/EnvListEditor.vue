<script setup lang="ts">
// 环境变量键值清单编辑器（票 B4-T8，ADR-0006）：任务级 env 与 Pipeline 级 env
// 共用同一形态——名/值对清单，就地增删。
//
// 受控组件契约：结构变更（增/删行）经 `add`/`remove` 事件交父处理——父持有数组
// 并按需懒初始化（任务级 `job.env` 缺省 undefined，父在 add 时 `??=` 钉成响应式
// 数组再 push，**不在渲染期 mutate**，保 ADR-0009「定义原样往返」——空 load→save
// 不给任务添 `env: []` 噪声）。行内字段编辑（名/值）仍 v-model 就地改行对象属性
// （此时数组已是父持有的真数组，行对象即其元素，突变落回父态）。
// #96: 迁移 Naive UI——名/值输入改 NInput、增删按钮改 NButton，交互不变。

import { NButton, NInput } from 'naive-ui'

import type { EnvVar } from '@/model/pipeline'

defineProps<{
  /** 父持有的 env 数组（行对象即其元素，v-model 改属性落回父态）。 */
  env: EnvVar[]
  /** `name=""` 前缀，供测试选择器区分多实例（如 `job-env` / `pipe-env`）。 */
  nameAttr: string
  addLabel: string
  removeLabel: string
  emptyLabel: string
  nameLabel: string
  valueLabel: string
}>()

const emit = defineEmits<{
  (e: 'add'): void
  (e: 'remove', index: number): void
}>()
</script>

<template>
  <div class="env-list-editor">
    <p v-if="env.length === 0" class="form-hint">{{ emptyLabel }}</p>
    <div v-for="(e, i) in env" :key="i" class="kv-row">
      <n-input
        v-model:value="e.name"
        :input-props="{ name: `${nameAttr}-${i}-name` }"
        :placeholder="nameLabel"
      />
      <n-input
        v-model:value="e.value"
        :input-props="{ name: `${nameAttr}-${i}-value` }"
        :placeholder="valueLabel"
      />
      <n-button size="small" :name="`${nameAttr}-${i}-remove`" @click="emit('remove', i)">
        {{ removeLabel }}
      </n-button>
    </div>
    <n-button size="small" dashed :name="`${nameAttr}-add`" @click="emit('add')">
      {{ addLabel }}
    </n-button>
  </div>
</template>

<style scoped>
.env-list-editor {
  display: flex;
  flex-direction: column;
  gap: 8px;
  align-items: flex-start;
}

.kv-row {
  display: grid;
  grid-template-columns: 1fr 1fr auto;
  gap: 6px;
  align-items: center;
  width: 100%;
}
</style>

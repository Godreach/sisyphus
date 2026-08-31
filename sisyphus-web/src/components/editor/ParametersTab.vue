<script setup lang="ts">
// 参数页签（票 B4-T8，ADR-0006）：Pipeline 级参数四种类型 string/number/bool/enum，
// 必填带默认值（R1）、enum 给候选项（R2）。校验错误按 `parameters[i]` 前缀定位。
//
// 反应式约定：`parameters` 是父持有的响应式数组，就地 mutate（同 EnvListEditor）。
// 默认值统一语义：空 = 无默认（undefined）——string/number/enum 空输入归 undefined
// （使 R1「必填参数必须带默认值」对四种类型皆可达；空串默认值是退化态，编辑器
// 不提供）。bool 默认 = 三态下拉（true/false/无默认），未选 = undefined。类型切换
// 清默认（旧默认的类型已与新类型不符）。
// #96: 迁移 Naive UI——类型改 NSelect、必填改 NSwitch、number 默认改
// NInputNumber、bool 默认改三态 NSelect、其余输入改 NInput，交互不变。
// 票 #109: 定稿设计语言——参数卡走白卡语义（12px 圆角无描边）+ 卡片清单间距，
// 交互不变。

import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import { NButton, NForm, NFormItem, NInput, NInputNumber, NSelect, NSwitch } from 'naive-ui'

import type { Parameter, ParameterType } from '@/model/pipeline'
import { errorsForField, linesToText, textToLines } from '@/model/editor'

const props = defineProps<{
  parameters: Parameter[]
  /** 展示用错误清单（本地校验或服务端 422，同形 {path,message}）。 */
  errors: { path: string; message: string }[]
}>()

const { t } = useI18n()

const TYPES: ParameterType[] = ['string', 'number', 'bool', 'enum']

function typeKey(ty: ParameterType): string {
  return `editor.paramType${ty[0]!.toUpperCase()}${ty.slice(1)}`
}

/** NSelect 选项（label 需 i18n 响应 locale 切换，经 computed 重算）。 */
const typeOptions = computed(() =>
  TYPES.map((ty) => ({ label: t(typeKey(ty)), value: ty })),
)
const boolDefaultOptions = computed(() => [
  { label: t('editor.paramNoDefault'), value: '' },
  { label: 'true', value: 'true' },
  { label: 'false', value: 'false' },
])

/** 类型切换：清默认值（旧默认的类型与新类型不符）。 */
function onTypeChange(p: Parameter, ty: ParameterType): void {
  p.type = ty
  p.default = undefined
}

function add(): void {
  // 新参数无默认值（undefined）——必填时 R1 会拦下，引导填默认。
  props.parameters.push({ name: '', type: 'string', required: false })
}

function remove(i: number): void {
  props.parameters.splice(i, 1)
}

/** enum 候选项：textarea ↔ string[]（每行一个，丢弃空行）。 */
function choicesLines(p: Parameter): string {
  return linesToText(p.choices ?? [])
}

function setChoices(p: Parameter, text: string): void {
  const next = textToLines(text)
  p.choices = next.length > 0 ? next : undefined
}

// 默认值取/设（按类型分支，空 = undefined）。
function defaultStr(p: Parameter): string {
  return typeof p.default === 'string' ? p.default : ''
}
function setDefaultStr(p: Parameter, v: string): void {
  p.default = v === '' ? undefined : v
}
function setDefaultNum(p: Parameter, v: number | null): void {
  p.default = v == null ? undefined : v
}
// bool 默认：三态 NSelect —— ''（无默认）/ 'true' / 'false'。开关无法表达「无默认」，
// 故用下拉使显式 false 默认可设（如 verbose=false 默认），R1 对 bool 亦可达
// （required + 选 true 或 false）。空 = undefined（无默认）。
function defaultBoolKey(p: Parameter): string {
  if (p.default === true) return 'true'
  if (p.default === false) return 'false'
  return ''
}
function setDefaultBool(p: Parameter, v: string | number | null): void {
  p.default = v === 'true' ? true : v === 'false' ? false : undefined
}

function paramErrors(i: number): { path: string; message: string }[] {
  return errorsForField(props.errors, `parameters[${i}]`)
}
</script>

<template>
  <section class="editor-tab">
    <h2>{{ t('editor.paramsTitle') }}</h2>
    <p class="form-hint">{{ t('editor.paramsHint') }}</p>

    <p v-if="parameters.length === 0" class="form-hint">{{ t('editor.paramsEmpty') }}</p>

    <n-form label-placement="top" class="params-form">
      <div v-for="(p, i) in parameters" :key="i" class="param-card">
        <div class="param-row">
          <n-form-item :label="t('editor.paramName')" class="param-field">
            <n-input v-model:value="p.name" :input-props="{ name: `param-${i}-name` }" />
          </n-form-item>
          <n-form-item :label="t('editor.paramType')" class="param-field">
            <n-select
              :name="`param-${i}-type`"
              :value="p.type"
              :options="typeOptions"
              :virtual-scroll="false"
              @update:value="onTypeChange(p, $event as ParameterType)"
            />
          </n-form-item>
          <n-form-item :label="t('editor.paramRequired')" class="param-required-field">
            <n-switch v-model:value="p.required" :name="`param-${i}-required`" />
          </n-form-item>
          <div class="param-remove">
            <n-button size="small" :name="`param-${i}-remove`" @click="remove(i)">
              {{ t('editor.paramRemove') }}
            </n-button>
          </div>
        </div>

        <div class="param-row">
          <n-form-item :label="t('editor.paramDefault')" class="param-field">
            <n-input-number
              v-if="p.type === 'number'"
              class="param-default-input"
              :value="typeof p.default === 'number' ? p.default : null"
              :input-props="{ name: `param-${i}-default` }"
              @update:value="setDefaultNum(p, $event)"
            />
            <n-select
              v-else-if="p.type === 'bool'"
              class="param-default-input"
              :name="`param-${i}-default`"
              :value="defaultBoolKey(p)"
              :options="boolDefaultOptions"
              :virtual-scroll="false"
              @update:value="setDefaultBool(p, $event)"
            />
            <n-input
              v-else
              :value="defaultStr(p)"
              :input-props="{ name: `param-${i}-default` }"
              @update:value="setDefaultStr(p, $event)"
            />
          </n-form-item>
          <n-form-item :label="t('editor.paramDescription')" class="param-field">
            <n-input v-model:value="p.description" :input-props="{ name: `param-${i}-desc` }" />
          </n-form-item>
        </div>

        <!-- enum 候选项（每行一个）。 -->
        <n-form-item
          v-if="p.type === 'enum'"
          :label="t('editor.paramChoices')"
          class="param-choices-field"
        >
          <n-input
            type="textarea"
            :rows="3"
            :value="choicesLines(p)"
            :input-props="{ name: `param-${i}-choices` }"
            @update:value="setChoices(p, $event)"
          />
        </n-form-item>

        <ul v-if="paramErrors(i).length > 0" class="field-errors" role="alert">
          <li v-for="(e, ei) in paramErrors(i)" :key="ei">
            <code class="err-path">{{ e.path }}</code> {{ e.message }}
          </li>
        </ul>
      </div>
    </n-form>

    <n-button size="small" dashed name="param-add" @click="add">
      {{ t('editor.paramAdd') }}
    </n-button>
  </section>
</template>

<style scoped>
.editor-tab {
  display: flex;
  flex-direction: column;
  gap: 8px;
  align-items: flex-start;
}

.editor-tab h2 {
  margin: 0;
}

/* 参数卡纵排间距（卡片清单形态，同构建列表 rows gap）。 */
.params-form {
  width: 100%;
  display: flex;
  flex-direction: column;
  gap: 12px;
}

/* 参数卡：顶层条目 = 定稿卡片语义（白底 12px 圆角无描边，同 sisy-card）。 */
.param-card {
  border-radius: var(--sisy-radius-card);
  padding: 16px;
  display: flex;
  flex-direction: column;
  gap: 4px;
  background: var(--sisy-color-surface);
}

.param-row {
  display: flex;
  gap: 12px;
  flex-wrap: wrap;
  align-items: flex-start;
}

.param-field {
  flex: 1 1 160px;
  min-width: 140px;
}

.param-required-field {
  flex: 0 0 auto;
}

.param-remove {
  flex: 0 0 auto;
  align-self: flex-end;
}

.param-default-input {
  width: 160px;
}

.param-choices-field {
  width: 100%;
}

.field-errors {
  list-style: none;
  margin: 4px 0 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
  font-size: 12px;
  color: var(--sisy-color-danger);
}

.field-errors li {
  line-height: 1.4;
}

.err-path {
  font-family: ui-monospace, 'Cascadia Code', Consolas, monospace;
  font-size: 12px;
  color: var(--sisy-color-text-secondary);
  border: 1px solid var(--sisy-color-border);
  border-radius: 3px;
  padding: 0 4px;
  margin-right: 4px;
}
</style>

<script setup lang="ts">
// 参数页签（票 B4-T8，ADR-0006）：Pipeline 级参数四种类型 string/number/bool/enum，
// 必填带默认值（R1）、enum 给候选项（R2）。校验错误按 `parameters[i]` 前缀定位。
//
// 反应式约定：`parameters` 是父持有的响应式数组，就地 mutate（同 EnvListEditor）。
// 默认值统一语义：空 = 无默认（undefined）——string/number/enum 空输入归 undefined
// （使 R1「必填参数必须带默认值」对四种类型皆可达；空串默认值是退化态，编辑器
// 不提供）。bool 默认 = 复选（true/false），未勾选 = undefined。类型切换清默认
// （旧默认的类型已与新类型不符）。

import { useI18n } from 'vue-i18n'

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
function defaultNum(p: Parameter): string {
  return typeof p.default === 'number' ? String(p.default) : ''
}
function setDefaultStr(p: Parameter, v: string): void {
  p.default = v === '' ? undefined : v
}
function setDefaultNum(p: Parameter, v: string): void {
  const n = Number(v)
  p.default = v === '' || !Number.isFinite(n) ? undefined : n
}
// bool 默认：三态 select —— ''（无默认）/ 'true' / 'false'。复选框无法表达「无默认」，
// 故用 select 使显式 false 默认可设（如 verbose=false 默认），R1 对 bool 亦可达
// （required + 选 true 或 false）。空 = undefined（无默认）。
function defaultBoolKey(p: Parameter): string {
  if (p.default === true) return 'true'
  if (p.default === false) return 'false'
  return ''
}
function setDefaultBool(p: Parameter, v: string): void {
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

    <div v-for="(p, i) in parameters" :key="i" class="param-card">
      <div class="param-row">
        <label class="field">
          <span>{{ t('editor.paramName') }}</span>
          <input :name="`param-${i}-name`" v-model="p.name" autocomplete="off" />
        </label>
        <label class="field">
          <span>{{ t('editor.paramType') }}</span>
          <select
            :name="`param-${i}-type`"
            :value="p.type"
            @change="onTypeChange(p, ($event.target as HTMLSelectElement).value as ParameterType)"
          >
            <option v-for="ty in TYPES" :key="ty" :value="ty">{{ t(typeKey(ty)) }}</option>
          </select>
        </label>
        <label class="field param-required-field">
          <span>{{ t('editor.paramRequired') }}</span>
          <input type="checkbox" :name="`param-${i}-required`" v-model="p.required" />
        </label>
        <div class="param-remove">
          <button type="button" class="btn" :name="`param-${i}-remove`" @click="remove(i)">
            {{ t('editor.paramRemove') }}
          </button>
        </div>
      </div>

      <div class="param-row">
        <label class="field">
          <span>{{ t('editor.paramDefault') }}</span>
          <input
            v-if="p.type === 'number'"
            type="number"
            :name="`param-${i}-default`"
            :value="defaultNum(p)"
            @input="setDefaultNum(p, ($event.target as HTMLInputElement).value)"
          />
          <select
            v-else-if="p.type === 'bool'"
            :name="`param-${i}-default`"
            :value="defaultBoolKey(p)"
            @change="setDefaultBool(p, ($event.target as HTMLSelectElement).value)"
          >
            <option value="">{{ t('editor.paramNoDefault') }}</option>
            <option value="true">true</option>
            <option value="false">false</option>
          </select>
          <input
            v-else
            :name="`param-${i}-default`"
            :value="defaultStr(p)"
            @input="setDefaultStr(p, ($event.target as HTMLInputElement).value)"
            autocomplete="off"
          />
        </label>
        <label class="field">
          <span>{{ t('editor.paramDescription') }}</span>
          <input :name="`param-${i}-desc`" v-model="p.description" autocomplete="off" />
        </label>
      </div>

      <!-- enum 候选项（每行一个）。 -->
      <label v-if="p.type === 'enum'" class="field param-choices-field">
        <span>{{ t('editor.paramChoices') }}</span>
        <textarea
          :name="`param-${i}-choices`"
          :value="choicesLines(p)"
          @input="setChoices(p, ($event.target as HTMLTextAreaElement).value)"
          rows="3"
        ></textarea>
      </label>

      <ul v-if="paramErrors(i).length > 0" class="field-errors" role="alert">
        <li v-for="(e, ei) in paramErrors(i)" :key="ei">
          <code class="err-path">{{ e.path }}</code> {{ e.message }}
        </li>
      </ul>
    </div>

    <button type="button" class="btn" name="param-add" @click="add">
      {{ t('editor.paramAdd') }}
    </button>
  </section>
</template>

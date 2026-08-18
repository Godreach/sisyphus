#!/usr/bin/env node
// i18n catalog 对账脚本（ADR-0003 后果项 / ADR-0020 双语纪律，票 B4-T1）。
//
// 强制 zh/en catalog 的 key 集合完全一致（同构嵌套对象）：zh 为源语言、
// en 全量对译——缺 key 或多余 key 都视为失败（多余 key 说明 en 里加了
// zh 没有的文案，违反「zh 源语言、en 对译」）。
//
// 挂进前端 CI（vue-tsc + vitest + i18n 对账）：任一方漂移即红。
//
// 用法：node scripts/i18n-check.mjs

import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

const LOCALES_DIR = fileURLToPath(new URL('../src/i18n/locales/', import.meta.url))

const SOURCE = 'zh-CN'
const TARGET = 'en-US'

/** 嵌套 JSON → 点分 key 集合（扁平化）。 */
function flatten(obj, prefix = '') {
  const keys = new Set()
  for (const [k, v] of Object.entries(obj)) {
    const key = prefix ? `${prefix}.${k}` : k
    if (v && typeof v === 'object' && !Array.isArray(v)) {
      for (const sub of flatten(v, key)) keys.add(sub)
    } else {
      keys.add(key)
    }
  }
  return keys
}

function load(locale) {
  const file = path.join(LOCALES_DIR, `${locale}.json`)
  return JSON.parse(readFileSync(file, 'utf8'))
}

const sourceKeys = flatten(load(SOURCE))
const targetKeys = flatten(load(TARGET))

const missing = [...sourceKeys].filter((k) => !targetKeys.has(k))
const extra = [...targetKeys].filter((k) => !sourceKeys.has(k))

if (missing.length === 0 && extra.length === 0) {
  console.log(`i18n 对账通过：${SOURCE} / ${TARGET} key 集合一致（${sourceKeys.size} keys）`)
  process.exit(0)
}

console.error(`i18n 对账失败：${SOURCE}（源语言）与 ${TARGET}（对译）key 集合不一致`)
if (missing.length > 0) {
  console.error(`\n${TARGET} 缺少以下 key（须补全对译）：`)
  for (const k of missing) console.error(`  - ${k}`)
}
if (extra.length > 0) {
  console.error(`\n${TARGET} 含 ${SOURCE} 没有的 key（多余，违反「zh 源语言、en 对译」）：`)
  for (const k of extra) console.error(`  - ${k}`)
}
process.exit(1)

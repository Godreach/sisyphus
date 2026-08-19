// 前端校验与 sisyphus-model `validate` 的对账测试（票 B4-T7，ADR-0009）。
//
// 对账第一等缝：前端 `validatePipeline` 与 `sisyphus-model::validate` 对同一组样本
// （`reconcile.fixtures.json`，由 sisyphus-codegen 从 model 生成）给出一致结论。规则码单一事实源
// 在 model；本 spec 据 fixtures 的 `expectedCodes`（Rust 已自检的码 multiset）比对前端
// 产出——漂移即红。
//
// 四条断言：
// 1. valid 一致：`tsErrors.length === 0` === fixture.valid。
// 2. multiset 一致：sorted(tsErrors.code) === sorted(fixture.expectedCodes)（带计数，
//    抓「同规则触发多次」偏差）。
// 3. 防漏同步：生成的 `VALIDATION_CODES` 每条至少被一个样本覆盖。
// 4. TS `VALIDATION_CODES` 与 fixtures `rules` 集合一致（两份生成产物互校）。
//
// 不比 message 文案（耦合措辞微调）、不比 path（编辑器本地错与服务端错 path 已同形）。

import { describe, expect, it } from 'vitest'

import { VALIDATION_CODES } from '@/model/codes'
import { validatePipeline, type ValidationError } from '@/model/validate'
import type { Pipeline } from '@/model/pipeline'
import fixtures from '@/model/reconcile.fixtures.json'

interface FixtureSample {
  id: string
  valid: boolean
  expectedCodes: string[]
  json: unknown
}
interface Fixtures {
  rules: string[]
  samples: FixtureSample[]
}

const fx = fixtures as unknown as Fixtures

function sortedCodes(errs: ValidationError[]): string[] {
  return errs.map((e) => e.code).sort()
}

describe('validatePipeline 与 sisyphus-model validate 对账', () => {
  it('每样本 valid 结论一致', () => {
    for (const s of fx.samples) {
      const errs = validatePipeline(s.json as unknown as Pipeline)
      expect(errs.length === 0, `${s.id}: valid 不一致`).toBe(s.valid)
    }
  })

  it('每样本规则码 multiset 一致', () => {
    for (const s of fx.samples) {
      const errs = validatePipeline(s.json as unknown as Pipeline)
      const actual = sortedCodes(errs)
      const expected = [...s.expectedCodes].sort()
      expect(
        actual.length === expected.length && actual.every((c, i) => c === expected[i]),
        `${s.id}: 码 multiset 不一致\n  actual:   ${JSON.stringify(actual)}\n  expected: ${JSON.stringify(expected)}`,
      ).toBe(true)
    }
  })

  it('每条规则码至少被一个样本覆盖（防漏同步）', () => {
    for (const code of fx.rules) {
      const covered = fx.samples.some((s) => s.expectedCodes.includes(code))
      expect(covered, `规则码 ${code} 无样本覆盖`).toBe(true)
    }
  })

  it('TS VALIDATION_CODES 与 fixtures rules 集合一致（两份生成产物互校）', () => {
    const tsSet = new Set<string>(VALIDATION_CODES)
    const fxSet = new Set<string>(fx.rules)
    expect([...tsSet].sort()).toEqual([...fxSet].sort())
    expect(tsSet.size).toBe(fx.rules.length)
  })
})

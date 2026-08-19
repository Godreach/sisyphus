// when 表达式 accept/reject 对账测试（票 B4-T7，ADR-0006）。
//
// `sisyphus-web/src/model/when.ts` 是 `sisyphus-model/src/when.rs` 的忠实端口——
// 只判合法性（accept/reject），不产 AST。本 spec 逐用例钉 Rust 的 tokenizer/parser
// 行为，含 Plan 子代理逐字节验证的 parity 陷阱：
// - 空串 reject（UnexpectedEnd）。
// - 负数 reject：消费循环只吃 [0-9.]，前导 `-` 吃不掉 → 空文本 → reject（Rust 数字
//   arm 的 `-` 守卫实为死代码）。
// - `1.2.3` reject（Number 非 finite）；`.5`/`5.`/`1.2`/`1` accept。
// - `true`/`false` accept（字符串字面量，非布尔）。
// - `exists` 必须跟 Ident 操作数（`exists 123`/`exists "x"` reject；`exists foo`/
//   `exists ${X}` accept）；`exists` 裸 reject。
// - `${123}` accept（数字亦是合法变量名）；`${}` reject；`${x` 未闭合 reject。
// - 尾随垃圾 reject；`$`/`a$` reject（Rust 曾 panic，已修，端口须 reject 不崩）。
//
// 注意：when.ts 只管语法。`${SISY_WORKSPACE}` 语法合法 → accept；「禁 SISY_WORKSPACE」
// 是 validate.ts 的 R3 规则（字面 contains 检查），不在此处。

import { describe, expect, it } from 'vitest'

import { isValidWhen } from '@/model/when'

describe('isValidWhen（when.rs accept/reject 端口）', () => {
  it('空串 reject', () => {
    expect(isValidWhen('')).toBe(false)
  })

  it('合法等值/逻辑/存在性 accept', () => {
    expect(isValidWhen('${SISY_BRANCH} == "main"')).toBe(true)
    expect(isValidWhen('${SISY_BRANCH} == "main" && exists SISY_COMMIT_ID')).toBe(true)
    expect(isValidWhen('${SISY_BRANCH} == "main" || ${MISSING} == "x"')).toBe(true)
    expect(isValidWhen('exists SISY_COMMIT_ID')).toBe(true)
    expect(isValidWhen('${X}')).toBe(true)
    expect(isValidWhen('(a == "b")')).toBe(true)
  })

  it('数值比较 accept', () => {
    expect(isValidWhen('${SISY_BUILD_NUMBER} >= 2')).toBe(true)
    expect(isValidWhen('${SISY_BUILD_NUMBER} < 10')).toBe(true)
    expect(isValidWhen('1')).toBe(true)
    expect(isValidWhen('.5')).toBe(true)
    expect(isValidWhen('5.')).toBe(true)
    expect(isValidWhen('1.2')).toBe(true)
  })

  it('负数字面量 reject（消费循环不吃前导 -）', () => {
    expect(isValidWhen('-5')).toBe(false)
    expect(isValidWhen('-')).toBe(false)
    expect(isValidWhen('--')).toBe(false)
    expect(isValidWhen('${X} > -5')).toBe(false)
  })

  it('非法数值 reject（Number 非 finite）', () => {
    expect(isValidWhen('1.2.3')).toBe(false)
    expect(isValidWhen('.')).toBe(false)
  })

  it('true/false accept 为字符串字面量（非布尔）', () => {
    expect(isValidWhen('true')).toBe(true)
    expect(isValidWhen('false')).toBe(true)
    expect(isValidWhen('${X} == "true"')).toBe(true)
  })

  it('exists 须跟 Ident 操作数', () => {
    expect(isValidWhen('exists')).toBe(false)
    expect(isValidWhen('exists 123')).toBe(false)
    expect(isValidWhen('exists "x"')).toBe(false)
    expect(isValidWhen('exists (a == "b")')).toBe(false)
    expect(isValidWhen('exists foo')).toBe(true)
    expect(isValidWhen('exists ${X}')).toBe(true)
  })

  it('变量引用：${name} 合法性', () => {
    expect(isValidWhen('${123}')).toBe(true) // 数字亦是合法变量名
    expect(isValidWhen('${SISY_BRANCH}')).toBe(true)
    expect(isValidWhen('${}')).toBe(false) // 空名
    expect(isValidWhen('${x')).toBe(false) // 未闭合
    expect(isValidWhen('${a-b}')).toBe(false) // 非法字符
  })

  it('尾随垃圾 reject', () => {
    expect(isValidWhen('x == "a" junk')).toBe(false)
    expect(isValidWhen('exists foo bar')).toBe(false)
  })

  it('越界语法 reject（无图灵完备构造）', () => {
    expect(isValidWhen('a => b')).toBe(false)
    expect(isValidWhen('a = 1')).toBe(false)
    expect(isValidWhen('fn(x)')).toBe(false)
    expect(isValidWhen('loop { }')).toBe(false)
  })

  it('括号不匹配 reject', () => {
    expect(isValidWhen('(a == "b"')).toBe(false)
    expect(isValidWhen('a == "b")')).toBe(false)
  })

  it('非法单字符 reject', () => {
    expect(isValidWhen('a @ b')).toBe(false)
    expect(isValidWhen('@')).toBe(false)
  })

  it('尾随裸 $ reject 不崩（Rust 曾 panic，已修）', () => {
    expect(isValidWhen('$')).toBe(false)
    expect(isValidWhen('a$')).toBe(false)
    expect(isValidWhen('${X} == $')).toBe(false)
  })

  it('SISY_WORKSPACE 语法合法 → accept（禁用是 validate 的 R3，不在此处）', () => {
    expect(isValidWhen('${SISY_WORKSPACE} == "/x"')).toBe(true)
  })
})

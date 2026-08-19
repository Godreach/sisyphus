// 自定义标签 文本↔数组 互转纯逻辑单测（票 B4-T5）：
// parseLabelLines（每行一条 → 数组，trim + 去空行）/ formatLabelLines（数组 →
// 每行一条）的 round-trip 稳定。key=value 形态校验由后端做（422 定位到
// custom_labels），本函数只做纯文本切分/拼接。

import { describe, expect, it } from 'vitest'

import { formatLabelLines, parseLabelLines } from '@/utils/agentLabels'

describe('parseLabelLines', () => {
  it('按行切分 + trim 行首尾 + 去空行', () => {
    expect(parseLabelLines('region=cn\n  gpu=nvidia \n\n')).toEqual([
      'region=cn',
      'gpu=nvidia',
    ])
  })

  it('空文本 → 空数组', () => {
    expect(parseLabelLines('')).toEqual([])
  })

  it('纯空白 → 空数组', () => {
    expect(parseLabelLines('  \n\n  ')).toEqual([])
  })
})

describe('formatLabelLines', () => {
  it('数组 → 每行一条（无尾换行）', () => {
    expect(formatLabelLines(['region=cn', 'gpu=nvidia'])).toBe('region=cn\ngpu=nvidia')
  })

  it('空数组 → 空串', () => {
    expect(formatLabelLines([])).toBe('')
  })

  it('round-trip：parse(format(x)) 稳定', () => {
    const labels = ['region=cn', 'gpu=nvidia', 'arch=arm64']
    expect(parseLabelLines(formatLabelLines(labels))).toEqual(labels)
  })
})

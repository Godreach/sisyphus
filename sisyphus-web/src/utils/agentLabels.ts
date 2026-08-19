// 自定义标签 文本↔数组 互转（票 B4-T5）：建条目/编辑表单的 `custom_labels`
// 输入面（textarea 每行一条 `key=value`）与后端 `custom_labels: string[]` 契约
// 之间的纯转换。
//
// key=value 形态校验由后端做（422 定位到 `custom_labels`，`describeSubmitError`
// 拼接清单就地展示）；本函数只做纯文本切分/拼接，不判形态——前端不复制第二
// 份校验规则（ADR-0009 单一事实源纪律在 Agent 标签面的同款落地）。

/** 文本区（每行一条）→ 标签数组：trim 行首尾空白、去空行。 */
export function parseLabelLines(text: string): string[] {
  return text
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line !== '')
}

/** 标签数组 → 文本区（每行一条、无尾换行）。 */
export function formatLabelLines(labels: string[]): string {
  return labels.join('\n')
}

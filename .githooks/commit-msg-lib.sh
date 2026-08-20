# commit-msg 校验核心——Conventional Commits + 中文 subject。
# 被 .githooks/commit-msg（git hook）与 .githooks/lint-commits（CI/手动）source。
# 规则详见 docs/agents/commit-messages.md。

# 全角字符/中文按 UTF-8 字节匹配，钉 locale 防 git bash 误判。
export LC_ALL=C.UTF-8

# Conventional Commits type 全集。仓库历史用 feat/fix/docs/chore/style，
# 余者（refactor/perf/test/build/ci/revert）备用不收紧，免误杀合理新 type。
readonly _CM_TYPES='feat|fix|docs|chore|style|refactor|perf|test|build|ci|revert'

# subject 第一行正则：<type>[!][(scope)]: <非空 description>
readonly _CM_RE="^($_CM_TYPES)(\([a-z0-9_-]+\))?!?: .+"

# 校验单行 subject。合规返回 0，否则打印中文错误返回 1。
lint_subject() {
  local subject="$1"

  # merge commit 放行（仓库当前 linear 无 merge，留作保险）
  case "$subject" in
    "Merge "*) return 0 ;;
  esac

  if [[ "$subject" =~ $_CM_RE ]]; then
    return 0
  fi

  printf '%s\n' "✗ 提交信息不符合 Conventional Commits 规范" >&2
  printf '%s\n' "  subject: $subject" >&2
  printf '%s\n' "  规则: <type>[(<scope>)][!]: <中文描述>，type ∈ {feat,fix,docs,chore,style,refactor,perf,test,build,ci,revert}" >&2
  printf '%s\n' "  正例: feat: 产物链路——存储/端点/Agent 传输/前端产物区（票 #74）" >&2
  printf '%s\n' "  正例: fix(web): headless 冒烟钉中文 locale——修 CI en-US 红灯" >&2
  printf '%s\n' "  详见 docs/agents/commit-messages.md" >&2
  return 1
}

# 读 commit message 文件第一行校验（git commit-msg hook 调用，$1 = 临时文件）。
lint_first_line() {
  local file="$1"
  local subject
  subject=$(head -n1 "$file")
  lint_subject "$subject"
}

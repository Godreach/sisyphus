# Commit messages

How agents and contributors should write commit messages for this repo. The convention mirrors what the history already does — it is now written down and enforced so the one-off miss (a subject with no `type:` prefix) cannot recur.

## Rule

Every commit subject (first line) must match Conventional Commits:

```
<type>[!][(scope)]: <Chinese description>
```

- **type** — one of `feat`, `fix`, `docs`, `chore`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `revert`. The history uses `feat`/`fix`/`docs`/`chore`/`style` most often; the rest are allowed and not narrowed.
- **scope** — optional, `([a-z0-9_-]+)`. The history uses `server`, `web`, `agent`; crate names are fine too.
- **!** — optional, for a breaking change.
- **description** — non-empty, Chinese, conventionally separated from detail by an em-dash `——` and carrying the ticket in full-width parens `（票 #NN）` or `（#NN）`.

The enforcement regex (ERE):

```
^(feat|fix|docs|chore|style|refactor|perf|test|build|ci|revert)(\([a-z0-9_-]+\))?!?: .+
```

## Examples

Good:

```
feat: 产物链路——存储/端点/Agent 传输/前端产物区（票 #74）
fix(web): headless 冒烟钉中文 locale——修 CI en-US 红灯
docs: 补五 crate README——proto/model/codegen/server/agent 各一份 crate 根文档
style: cargo fmt——全仓统一 rustfmt 格式（无逻辑变更）
```

Bad (these are rejected):

```
产物链路：存储/端点/Agent 传输/前端产物区（#74）      ← no type prefix; a full-width colon is not a type
update artifacts endpoint                              ← no type prefix
feat 产物链路 …                                         ← missing ": "
```

## Ticket references

`feat`/`fix` commits conventionally end the subject with `（票 #NN）` (or `（#NN）`) and the body with `Closes #NN`. This is convention, not enforced — `docs`/`chore`/`style` commits frequently omit a ticket. The hook does not reject a missing ticket; it only enforces the `type:` prefix.

## Merge commits

Subjects starting with `Merge ` are skipped (the repo is linear today; this is insurance for future merge commits).

## Enforcement

Two surfaces run the same check (`.githooks/commit-msg-lib.sh`):

- **Local `commit-msg` hook** — `.githooks/commit-msg` rejects the commit if the subject is non-conformant. Enable once per clone (the setting is local and not committed):
  ```
  git config core.hooksPath .githooks
  ```
- **CI** — the `commit-messages` job in `.github/workflows/ci.yml` lints the range: all new commits on a pull request (`origin/<base>..HEAD`), or the pushed commits on a push event. CI calls `bash .githooks/lint-commits <range>`, the same logic as the local hook.

The scripts are pinned to LF via `.gitattributes` (`.githooks/* eol=lf`) so Windows `autocrlf` does not corrupt the `#!/usr/bin/env bash` shebang.

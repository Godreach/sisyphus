# Commit messages

How agents and contributors should write commit messages for this repo. The convention mirrors what the history already does.

## Rule

Every commit subject (first line) must follow Conventional Commits:

```
<type>[!][(scope)]: <Chinese description>
```

- **type** — one of `feat`, `fix`, `docs`, `chore`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `revert`. The history uses `feat`/`fix`/`docs`/`chore`/`style` most often; the rest are allowed and not narrowed.
- **scope** — optional, `([a-z0-9_-]+)`. The history uses `server`, `web`, `agent`; crate names are fine too.
- **!** — optional, for a breaking change.
- **description** — non-empty, Chinese, conventionally separated from detail by an em-dash `——` and carrying the ticket in full-width parens `（票 #NN）` or `（#NN）`.

## Examples

Good:

```
feat: 产物链路——存储/端点/Agent 传输/前端产物区（票 #74）
fix(web): headless 冒烟钉中文 locale——修 CI en-US 红灯
docs: 补五 crate README——proto/model/codegen/server/agent 各一份 crate 根文档
style: cargo fmt——全仓统一 rustfmt 格式（无逻辑变更）
```

Bad:

```
产物链路：存储/端点/Agent 传输/前端产物区（#74）      ← no type prefix; a full-width colon is not a type
update artifacts endpoint                              ← no type prefix
feat 产物链路 …                                         ← missing ": "
```

## Ticket references

`feat`/`fix` commits conventionally end the subject with `（票 #NN）` (or `（#NN）`) and the body with `Closes #NN`. This is convention, not enforced — `docs`/`chore`/`style` commits frequently omit a ticket.

## Enforcement

None. The convention is self-policed: there is no local hook and no CI job. (An earlier setup — `.githooks/commit-msg` plus a `commit-messages` CI job — was removed; the convention text above is unchanged.)

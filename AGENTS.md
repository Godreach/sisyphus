# sisyphus

## Agent skills

### Issue tracker

Issues are tracked in GitHub Issues (repo: `Godreach/sisyphus`) via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default triage labels: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.

### Commit messages

Commit subjects follow Conventional Commits (`<type>[(<scope>)][!]: <Chinese subject>`). A local `commit-msg` hook (`.githooks/`, enable with `git config core.hooksPath .githooks`) and a CI job enforce the prefix. See `docs/agents/commit-messages.md`.

<!-- caniclose checklist v1 -->
# Close-out checklist

`/caniclose` reads this file on every full run and judges each item below
against the session: an item still open is a row of its report. Edit the
items to fit this project; the binary never rewrites this file.

## Standard items

- Run `/update-kb` before the last commit, so what the session learned is in
  the knowledge base.
- Commit your own changes with `/git-commit`.
- When working in a linked worktree, land it with `/git-worktree-merge`.

Removing a standard line here does not disable the checks built into
caniclose: those always run.

## Project items

- After a code change: `make lint test` green before the task is called done.
- After a finished task: the installed binary (`~/.local/bin/auto-compress-apfs`) carries the
  change (`--version`, `--help` or a read-only run). `make release` is handed to the owner while
  SentinelOne may kill Claude.app when the release binary is linked.
- `compress --apply` and `install-launchd` were never run on the real `HOME` to check behaviour;
  tests used temp trees and a temporary `HOME`.
- A change to the walk/compress path was treated as CRITICAL: the original is replaced only by
  the verified `RENAME_SWAP` of a compressed copy, only `/usr/bin/ditto` (and `/bin/launchctl`
  from the launchd commands) is executed.
- No new dependency beyond `serde`, `toml`, `globset` was added without the owner's approval.
- Nothing committed mentions the untracked local files (CLAUDE.md, AGENTS.md, .claude/,
  launchd/, local.mk).

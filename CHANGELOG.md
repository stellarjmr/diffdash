# Changelog

All notable changes to `diffdash` are tracked here.

## Unreleased

- New **Branches** tab (key `4`). Log and Stats only follow HEAD, so work on another branch, such as an agent's branch in its own worktree, did not show up until it was merged. The tab lists every local branch with commits ahead of and behind the current branch, and the files and `+/-` lines it changed since its merge base with HEAD (the range a pull request shows). The lower pane lists the selected branch's files; Enter opens the whole branch diff in less. The tab refreshes by itself when any branch tip moves. Comparisons are cached by HEAD and tip commit, so only branches that moved are recomputed. A branch with no common history shows that on its own row instead of failing the tab. `--once` prints a Branches section too.
- Fix: the 2 s poll could still rewrite `.git/index`, and so take `.git/index.lock`, whenever a tracked file had a new mtime but unchanged content (for example after an agent or formatter rewrote it as-is). An agent's `git commit` at that moment could fail. `--no-optional-locks` does not stop `git diff` from refreshing the index, so every git call now also passes `-c diff.autoRefreshIndex=false`. The git call behind Enter (the pager) used neither setting and now uses both.

## 0.1.0 - 2026-11-02

- Initial version: a read-only TUI with three tabs that show line counts. **Changes** lists staged, unstaged and untracked files with `+/-` counts, bars and totals, refreshing every 2 s. **Log** shows the commit graph with per-commit `+/-` and a pane listing each commit's files. **Stats** gives 7d/30d/90d/all totals by day, top-level directory and author.
- Untracked files are counted line by line (`git diff --stat` leaves them out), with a per-file cache keyed on mtime and size so large untracked files are not re-read on every poll.
- Every git call uses `--no-optional-locks`, so polling never takes `.git/index.lock` and cannot break a concurrent `git commit` by an agent or the user.
- The Changes title shows totals since HEAD (`git diff HEAD` plus untracked files), so a file that is both staged and unstaged is counted once.
- Diffs open in `less -R -+F`, so a user's `LESS=-F` cannot close a short diff before it can be read.
- The UI uses only ANSI palette colors and reverse-video selection, so it stays readable in both light and dark themes. All panels have rounded corners. The active tab, the diffdash badge and the selected Stats range are rounded pills with Powerline half-circle caps.
- `--once` prints all three views as plain text for scripts and quick checks.

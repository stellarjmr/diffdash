# Changelog

All notable changes to `diffdash` are tracked here.

## Unreleased

## 0.1.0 - 2026-11-02

- Initial version: a read-only TUI with three tabs that show line counts. **Changes** lists staged, unstaged and untracked files with `+/-` counts, bars and totals, refreshing every 2 s. **Log** shows the commit graph with per-commit `+/-` and a pane listing each commit's files. **Stats** gives 7d/30d/90d/all totals by day, top-level directory and author.
- Untracked files are counted line by line (`git diff --stat` leaves them out), with a per-file cache keyed on mtime and size so large untracked files are not re-read on every poll.
- Every git call uses `--no-optional-locks`, so polling never takes `.git/index.lock` and cannot break a concurrent `git commit` by an agent or the user.
- The Changes title shows totals since HEAD (`git diff HEAD` plus untracked files), so a file that is both staged and unstaged is counted once.
- Diffs open in `less -R -+F`, so a user's `LESS=-F` cannot close a short diff before it can be read.
- The UI uses only ANSI palette colors and reverse-video selection, so it stays readable in both light and dark themes. All panels have rounded corners. The active tab, the diffdash badge and the selected Stats range are rounded pills with Powerline half-circle caps.
- `--once` prints all three views as plain text for scripts and quick checks.

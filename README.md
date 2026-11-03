# diffdash

A read-only terminal dashboard for **how many lines changed** in a git repository: per file, per commit, per day, per directory and per author. Built for watching a coding agent edit a local repo — the working tree view refreshes every 2 seconds.

diffdash never stages, commits, or writes to the repository. It shells out to your installed `git`.

## Views

| Tab | Shows |
|---|---|
| **1 Changes** | Staged, unstaged and untracked files, each with `+added -deleted` and a bar scaled to the largest change, plus section subtotals. The title shows the total since HEAD (`git diff HEAD` plus untracked files), so a file that is both staged and unstaged is counted once. Untracked files are counted too (plain `git diff --stat` leaves them out). Refreshes every 2 s. |
| **2 Log** | Commit graph with files changed and `+/-` per commit; the lower pane lists the selected commit's files, largest change first. |
| **3 Stats** | Commits and `+/-` totals for the last 7 / 30 / 90 days or all history, broken down by day, top-level directory and author. |

## Install

```bash
cargo install --path .
```

## Run

```bash
diffdash              # current repository
diffdash ~/path/repo  # another repository
diffdash --once       # print all three views as plain text and exit (Log capped at 30 commits)
```

Options:

```
-n, --limit N     commits loaded in the Log tab (default 300)
-r, --range R     initial Stats range: 7d, 30d, 90d, all (default 30d)
    --once        print all three views as plain text and exit
                  (the Log section shows at most 30 commits)
-h, --help        show help
-V, --version     show version
```

Keys:

```
1 2 3 / Tab       switch tab
j k / arrows      move              g G   top / bottom
Ctrl-d Ctrl-u     page down / up
Enter             open the selected file's diff (Changes) or the commit (Log) in less
t                 cycle Stats range
r                 refresh now
q / Esc           quit
```

## How numbers are counted

- Line counts come from `git diff --numstat` and `git log --numstat`. Binary files show `bin` and add nothing to totals.
- Untracked files count every line as added; the count is cached per file and redone only when the file's size or mtime changes.
- Merge commits show `merge` in the Log tab. git reports no per-file stats for them, and Stats leaves them out.
- Stats group commits by commit date, the date `--since` filters on, so a rebased commit lands on the day it was rewritten. The Log tab shows the author date, like `git log`.
- All git calls pass `--no-optional-locks`, so the 2 s polling never holds `.git/index.lock` and cannot make a `git commit` running at the same time fail.
- Diffs open in `less -R -+F`. `-+F` overrides a `LESS=-F` setting, which would otherwise close short diffs at once.
- Colors come from the terminal's ANSI palette and the selection uses reverse video, so the UI follows light and dark themes.
- Panels have rounded corners. The active tab and Stats range are drawn as rounded pills using the Powerline half circles U+E0B6 and U+E0B4. Ghostty and kitty draw these glyphs themselves; other terminals need a Nerd Font or Powerline-patched font.

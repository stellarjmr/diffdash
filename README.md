# diffdash

A read-only terminal dashboard for **how many lines changed** in a git repository: per file, per commit, per branch, per day, per directory and per author. Built for watching a coding agent edit a local repo — the working tree view refreshes every 2 seconds.

diffdash never stages, commits, or writes to the repository. It shells out to your installed `git`.

## Views

| Tab | Shows |
|---|---|
| **1 Changes** | Staged, unstaged and untracked files, each with `+added -deleted` and a bar scaled to the largest change, plus section subtotals. The title shows the total since HEAD (`git diff HEAD` plus untracked files), so a file that is both staged and unstaged is counted once. Untracked files are counted too (plain `git diff --stat` leaves them out). Refreshes every 2 s. |
| **2 Log** | Commit graph with files changed and `+/-` per commit; the lower pane lists the selected commit's files, largest change first. |
| **3 Stats** | Commits and `+/-` totals for the last 7 / 30 / 90 days or all history, broken down by day, top-level directory and author. |
| **4 Branches** | Every local branch, most recently committed first, with the commits it is ahead of and behind the current branch and the files and `+/-` lines it changed since the two forked (the range a pull request shows). The lower pane lists the selected branch's files, largest change first. Updates by itself when any branch moves, for example when an agent commits in its own worktree. |

## Requirements

- macOS on Apple Silicon for the prebuilt Homebrew package.
- `git` on your `PATH`. diffdash runs it for every view.

## Install with Homebrew

```bash
brew tap stellarjmr/tool
brew install stellarjmr/tool/diffdash
```

The formula installs a prebuilt macOS Apple Silicon binary from GitHub Releases. It does not build from source and does not require Rust.

## Build from source

```bash
cargo install --path .
```

## Run

```bash
diffdash              # current repository
diffdash ~/path/repo  # another repository
diffdash --once       # print all four views as plain text and exit (Log capped at 30 commits)
```

Options:

```
-n, --limit N     commits loaded in the Log tab (default 300)
-r, --range R     initial Stats range: 7d, 30d, 90d, all (default 30d)
    --once        print all four views as plain text and exit
                  (the Log section shows at most 30 commits)
-h, --help        show help
-V, --version     show version
```

Keys:

```
1 2 3 4 / Tab     switch tab
j k / arrows      move              g G   top / bottom
Ctrl-d Ctrl-u     page down / up
Enter             open in less: the selected file's diff (Changes), the commit (Log),
                  or everything the branch changed since its merge base (Branches)
t                 cycle Stats range
r                 refresh now
q / Esc           quit
```

## How numbers are counted

- Line counts come from `git diff --numstat` and `git log --numstat`. Binary files show `bin` and add nothing to totals.
- Untracked files count every line as added; the count is cached per file and redone only when the file's size or mtime changes.
- Merge commits show `merge` in the Log tab. git reports no per-file stats for them, and Stats leaves them out.
- Stats group commits by commit date, the date `--since` filters on, so a rebased commit lands on the day it was rewritten. The Log tab shows the author date, like `git log`.
- Log and Stats follow HEAD only. Other branches appear in the Branches tab, which compares each one with HEAD from their merge base, the same range as `git diff HEAD...branch`: `ahead` / `behind` come from `git rev-list --left-right --count HEAD...branch`, lines from `git diff --numstat <merge-base> branch`. Commits HEAD gained after the fork do not count against the branch.
- Branches counts committed work only. A branch checked out in another worktree is tagged `worktree`; its uncommitted changes are not included (run `diffdash <that worktree>` to see them). A branch that shares no history with HEAD shows `no common history`. Only local branches are listed, not remote-tracking ones.
- The 2 s poll also reads the branch tips (one `git for-each-ref`). Branches reloads when a tip or HEAD moves, and only branches whose tip or HEAD changed are recomputed.
- All git calls pass `--no-optional-locks` and `-c diff.autoRefreshIndex=false`, so the 2 s polling never rewrites `.git/index` or holds `.git/index.lock`, and cannot make a `git commit` running at the same time fail. The second setting is needed because `git diff` otherwise refreshes the index when a tracked file was rewritten with unchanged content, `--no-optional-locks` or not. That refresh is left to your own next git command.
- Diffs open in `less -R -+F`. `-+F` overrides a `LESS=-F` setting, which would otherwise close short diffs at once.
- Colors come from the terminal's ANSI palette and the selection uses reverse video, so the UI follows light and dark themes.
- Panels have rounded corners, and the tab bar at the top and the key hints at the bottom sit in rounded boxes of their own. The active tab and Stats range are drawn as rounded pills using the Powerline half circles U+E0B6 and U+E0B4. Ghostty and kitty draw these glyphs themselves; other terminals need a Nerd Font or Powerline-patched font.
- In a narrow pane, the file lists make room in this order: the padding of the number columns, then the bar, then the path. A path that still does not fit loses whole directories from the left, so the file name stays visible (`…/auth/login.rs`); only a file name wider than the column is itself cut (`…ogin.rs`). A rename that does not fit shows just its new path after an arrow (`→ …/auth/login.rs`). The diff behind Enter and `--once` always show full paths.
- In a narrow pane, the Log tab's commit list gives up the padding of its columns, then the author, the date and the hash, in that order, to keep 24 cells of subject. The pane below still shows all three for the selected commit.
- The Branches list favors branch names. In a narrow pane it gives up the padding of its columns, then the bar, then the words in `1 ahead 0 behind` (shown as `↑1 ↓0`), then the date. A name that still does not fit loses its tags (`worktree`, `no common history`) and then its start (`…/feature`). Below about 40 columns the ahead, behind and file counts go too, so that a name stays readable; `+` and `-` always stay. The pane below still shows the full name and counts for the selected branch, as far as its title fits.

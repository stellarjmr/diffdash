# Changelog

All notable changes to `diffdash` are tracked here.

## Unreleased

- Fix: file names could not be read in a narrow pane, such as a split screen. The file lists (Changes and the lower panes of Log and Branches) kept 41 cells for the numbers and the bar at any width, and a long path was clipped on the right, so at 60 columns a row showed `crates/diffd` and never reached the file. The lists now make room in this order: the padding of the number columns, the bar, then the path. A path that still does not fit loses whole directories from the left (`…/auth/login.rs`), and a rename shows only its new path after an arrow (`→ …/auth/login.rs`). A pane where every path already fit looks the same as before. Enter and `--once` still show full paths.
- Fix: in a narrow pane, the Log tab's commit list lost the subject first. The author, date and number columns had fixed widths and the commit column got what was left, so at 60 columns a row read `* f512 t` and at 40 columns the hash and subject were gone. The list now gives up the padding, then the author, the date and the hash, in that order, to keep 24 cells of subject; the pane below still shows all three for the selected commit. Columns are hidden whole instead of being clipped to `8 minut`. A pane with room for that much subject looks the same as before.
- Fix: in a narrow pane, the Branches tab's list hid the branch names; at 60 columns none were left. The fixed columns took all the room and the name column got the rest, despite the rule that names win over the bar. The list now gives up the padding, then the bar, then the words in `1 ahead 0 behind` (now `↑1 ↓0`), then the date. A name that still does not fit loses its tags, then its start (`…/feature`), like a file path. Below about 40 columns the ahead, behind and file counts go too, so that a name stays readable; `+` and `-` always stay. A pane where names and a whole bar fit looks the same as before.
- Fix: in a narrow pane, the Stats tab cut off what each row was about. Its columns had fixed widths, so at 60 columns every day read `2026`, and directories and authors read `(roo` and `陈志`. Each table now measures what it holds and gives up the padding of its number columns, then its bar, then the commit count; `+` and `-` are never cut. A day that still does not fit shows `11-01`, and a name is cut on the right (`Christop…`). Tables also look different at 80 columns and up to 100: less padding and longer bars. A table looks as before once a bar of 4 cells fits beside its padding, which is from 86 columns for By day and from 100 for the other two. An author or directory longer than 17 cells is no longer cut at any width: it takes up to 40 cells from the bar. The box on top now gives up the words `· range`, the note and the net count one at a time and whole, so at 40 columns the range pills still show and no total is cut in half.

## 0.2.1 - 2026-11-03

- The tab bar at the top and the key hints at the bottom now sit in rounded boxes of their own, like the panels between them. They used to be bare lines of text above and below the boxed panels; now the whole screen has one look. Each bar takes three rows instead of one, so the panels get four fewer rows.

## 0.2.0 - 2026-11-03

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

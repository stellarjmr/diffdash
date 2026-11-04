mod git;

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Paragraph, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};

use git::{Branch, Branches, Bucket, Commit, FileStat, GraphRow, Log, Repo, Stats, WorkTree, totals};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REFRESH: Duration = Duration::from_secs(2);
const BAR: usize = 24;
/// Number columns of a file list when there is room to spare.
const NUM_W: usize = 8;
/// What one long name can claim. In a file list it is the most a name takes from
/// the bar, so one deep path cannot erase the bar of every row. In the branch list
/// it is the most a name takes from the words in `1 ahead` and the date.
const NAME_GOAL: usize = 40;
/// Cells of subject the commit list keeps before it starts hiding columns.
const SUBJECT_GOAL: usize = 24;
const INDENT: &str = "  ";
const NEW_TAG: &str = " new";
/// Cells before a branch name: a space and the current-branch marker.
const BRANCH_MARK: usize = 3;
/// Branch list: below this many cells of name, marker included, the counts other
/// than `+` and `-` go too.
const NAME_FLOOR: usize = 16;
const RANGES: [(&str, Option<u32>); 4] =
    [("7d", Some(7)), ("30d", Some(30)), ("90d", Some(90)), ("all", None)];
// New tabs go at the end so existing number keys keep their meaning.
const TABS: [&str; 4] = ["Changes", "Log", "Stats", "Branches"];

// Only ANSI palette colors and REVERSED, so the UI follows the terminal theme
// (light or dark).
const ADD: Color = Color::Green;
const DEL: Color = Color::Red;
const DIM: Color = Color::DarkGray;
const ACCENT: Color = Color::Cyan;

fn selected_style() -> Style {
    Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
}

/// Every box in the UI: rounded corners (╭╮╰╯) with a dim border.
fn panel<'a>() -> Block<'a> {
    Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM))
}

/// Powerline half circles (U+E0B6 / U+E0B4) used as the rounded ends of a
/// pill. Ghostty and kitty draw these themselves, so no font fallback.
const PILL_L: &str = "\u{e0b6}";
const PILL_R: &str = "\u{e0b4}";

/// A rounded pill: reversed label between half-circle caps.
///
/// The caps are drawn in the same color the reversed label shows as its
/// background, so they blend into it in any theme. `None` uses the terminal's
/// default foreground color (pure reverse video).
fn pill(label: impl Into<String>, color: Option<Color>) -> [Span<'static>; 3] {
    let cap = match color {
        Some(c) => Style::new().fg(c),
        None => Style::new(),
    }
    .add_modifier(Modifier::BOLD);
    [
        Span::styled(PILL_L, cap),
        Span::styled(label.into(), cap.add_modifier(Modifier::REVERSED)),
        Span::styled(PILL_R, cap),
    ]
}

/// An unselected item that takes the same width as [`pill`], so nothing
/// shifts when the selection moves.
fn pill_slot(label: impl Into<String>, style: Style) -> [Span<'static>; 3] {
    [Span::raw(" "), Span::styled(label.into(), style), Span::raw(" ")]
}

struct Opts {
    path: PathBuf,
    once: bool,
    limit: usize,
    range: usize,
}

fn usage() -> String {
    format!(
        "diffdash {VERSION} - line-count dashboard for a git repository

Usage: diffdash [PATH] [options]

Options:
  -n, --limit N     commits loaded in the Log tab (default 300)
  -r, --range R     initial Stats range: 7d, 30d, 90d, all (default 30d)
      --once        print all four views as plain text and exit
                    (the Log section shows at most 30 commits)
  -h, --help        show this help
  -V, --version     show version

Keys:
  1 2 3 4 / Tab     switch tab (Changes, Log, Stats, Branches)
  j k / arrows      move        g G   top / bottom
  Ctrl-d Ctrl-u     page down / up (also PageDown / PageUp)
  Enter             open the diff of the selected file, commit or branch in less
  t                 cycle Stats range
  r                 refresh now
  q / Esc           quit"
    )
}

fn parse_args() -> Result<Opts, String> {
    let mut o = Opts { path: PathBuf::from("."), once: false, limit: 300, range: 1 };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("diffdash {VERSION}");
                std::process::exit(0);
            }
            "--once" => o.once = true,
            "-n" | "--limit" => {
                let v = args.next().ok_or("--limit needs a number")?;
                o.limit = v.parse().map_err(|_| format!("invalid --limit: {v}"))?;
            }
            "-r" | "--range" => {
                let v = args.next().ok_or("--range needs a value")?;
                o.range = RANGES
                    .iter()
                    .position(|(n, _)| *n == v)
                    .ok_or(format!("invalid --range: {v} (use 7d, 30d, 90d, all)"))?;
            }
            s if s.starts_with('-') => return Err(format!("unknown option: {s}\n\n{}", usage())),
            s => o.path = PathBuf::from(s),
        }
    }
    Ok(o)
}

fn main() {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("diffdash: {e}");
            std::process::exit(2);
        }
    };
    let repo = match Repo::open(&opts.path) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("diffdash: {e}");
            std::process::exit(1);
        }
    };
    let result = if opts.once { print_once(&repo, &opts) } else { run_tui(repo, &opts) };
    if let Err(e) = result {
        eprintln!("diffdash: {e}");
        std::process::exit(1);
    }
}

// ------------------------------------------------------------------ helpers

fn plus(n: u64) -> String {
    format!("+{n}")
}
fn minus(n: u64) -> String {
    format!("-{n}")
}

/// Split up to `width` cells between added/deleted, scaled against `max` churn.
fn bar_split(a: u64, d: u64, max: u64, width: usize) -> (usize, usize) {
    let total = a + d;
    if total == 0 || max == 0 || width == 0 {
        return (0, 0);
    }
    let cells = ((total as f64 / max as f64) * width as f64).ceil() as usize;
    let cells = cells.clamp(1, width);
    let mut ac = ((a as f64 / total as f64) * cells as f64).round() as usize;
    if a > 0 && ac == 0 {
        ac = 1;
    }
    if d > 0 && ac == cells && cells > 1 {
        ac -= 1;
    }
    (ac, cells - ac)
}

/// Box-drawing `━` rather than geometric shapes like `■`/`▰`, which can hit
/// font fallback and render oversized in some terminals.
fn bar_line(a: u64, d: u64, max: u64, width: usize) -> Line<'static> {
    let (ac, dc) = bar_split(a, d, max, width);
    Line::from(vec![
        Span::styled("━".repeat(ac), Style::new().fg(ADD)),
        Span::styled("━".repeat(dc), Style::new().fg(DEL)),
    ])
}

fn bar_text(a: u64, d: u64, max: u64) -> String {
    let (ac, dc) = bar_split(a, d, max, BAR);
    format!("{}{}", "+".repeat(ac), "-".repeat(dc))
}

fn churn(f: &FileStat) -> u64 {
    f.add() + f.del()
}

/// Terminal cells `s` takes up.
fn cells(s: &str) -> usize {
    Span::raw(s).width()
}

/// Left-align `s` to `width` terminal cells (CJK characters take two cells,
/// which `{:<N}` formatting would not account for).
fn pad(s: &str, width: usize) -> String {
    format!("{s}{}", " ".repeat(width.saturating_sub(cells(s))))
}

/// `path` in at most `width` cells, keeping its end. The file name says what
/// changed, and a table cell clips on the right, which would leave only the
/// top directory. Whole directories go first, from the left (`…/auth/login.rs`);
/// the file name is cut only when nothing else is left (`…ogin.rs`).
fn shorten_path(path: &str, width: usize) -> String {
    if cells(path) <= width {
        return path.to_string();
    }
    // git lists an embedded repository as `dir/`. Its name is the part to keep.
    let body = path.trim_end_matches('/');
    for (i, _) in body.match_indices('/') {
        let short = format!("…{}", &path[i..]);
        if cells(&short) <= width {
            return short;
        }
    }
    let file = &path[body.rfind('/').map_or(0, |i| i + 1)..];
    if cells(file) <= width {
        return file.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut used = cells("…");
    let mut start = file.len();
    for (i, c) in file.char_indices().rev() {
        used += cells(c.encode_utf8(&mut [0; 4]));
        if used > width {
            break;
        }
        start = i;
    }
    format!("…{}", &file[start..])
}

/// `f`'s path in at most `width` cells. A rename that does not fit shows only
/// where the file went; the arrow keeps it from reading as a plain edit.
fn fit_path(f: &FileStat, width: usize) -> String {
    let arrow = cells("→ ");
    if f.old_path.is_none() || width <= arrow {
        return shorten_path(&f.path, width);
    }
    let full = f.display_path();
    if cells(&full) <= width {
        return full;
    }
    format!("→ {}", shorten_path(&f.path, width - arrow))
}

fn repo_name(root: &Path) -> String {
    root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

// ---------------------------------------------------------------- --once

fn print_once(repo: &Repo, o: &Opts) -> Result<(), String> {
    let w = repo.worktree()?;
    let log = repo.log(o.limit.min(30))?;
    let (rname, rdays) = RANGES[o.range];
    let stats = repo.stats(rdays)?;
    let br = repo.branches()?;
    let mut out = String::new();
    let all: Vec<&FileStat> = w.staged.iter().chain(&w.unstaged).chain(&w.untracked).collect();
    let max = all.iter().map(|f| churn(f)).max().unwrap_or(0);
    let (n, ta, td) = w.headline();

    out += &format!("diffdash · {} · {}\n\n", repo.root.display(), w.branch);
    out += &format!("== Changes since HEAD  {} {}  ({n} files)\n", plus(ta), minus(td));
    for (title, files) in [("Staged", &w.staged), ("Unstaged", &w.unstaged), ("Untracked", &w.untracked)] {
        if files.is_empty() {
            continue;
        }
        let (a, d) = totals(files);
        out += &format!("{title} ({})  {} {}\n", files.len(), plus(a), minus(d));
        for f in files {
            let nums = if f.is_binary() {
                format!("{:>14}", "bin")
            } else {
                format!("{:>7}{:>7}", plus(f.add()), minus(f.del()))
            };
            let path = pad(&f.display_path(), 50);
            out += &format!("  {path} {nums}  {}\n", bar_text(f.add(), f.del(), max));
        }
    }
    if all.is_empty() {
        out += "  working tree clean\n";
    }

    out += &format!("\n== Log (latest {})\n", log.commits.len());
    for row in &log.rows {
        match row.commit.map(|i| &log.commits[i]) {
            Some(c) => out += &format!("{} {}\n", row.graph, commit_summary(c)),
            None => out += &format!("{}\n", row.graph),
        }
    }

    out += &format!(
        "\n== Stats ({rname}, merges excluded)  {} commits  {} {}  net {:+}\n",
        stats.commits,
        plus(stats.added),
        minus(stats.deleted),
        stats.added as i64 - stats.deleted as i64
    );
    for (title, list, limit) in
        [("By day", &stats.by_day, 14), ("By directory", &stats.by_dir, 10), ("By author", &stats.by_author, 10)]
    {
        out += &format!("{title}\n");
        let max = list.iter().map(|b| b.added + b.deleted).max().unwrap_or(0);
        for b in list.iter().take(limit) {
            out += &format!(
                "  {:<24} {:>4}c {:>8}{:>8}  {}\n",
                b.key,
                b.commits,
                plus(b.added),
                minus(b.deleted),
                bar_text(b.added, b.deleted, max)
            );
        }
    }

    out += &format!("\n== Branches (compared with {} from the merge base)\n", br.head_name);
    let max = br.list.iter().map(branch_churn).max().unwrap_or(0);
    for b in &br.list {
        let mark = if b.current { '*' } else { ' ' };
        let info = match &b.cmp {
            Some(Ok(c)) => {
                let (a, d) = totals(&c.files);
                format!(
                    "{:>4} ahead {:>5} behind {:>4}f {:>7}{:>7}  {}",
                    c.ahead,
                    c.behind,
                    c.files.len(),
                    plus(a),
                    minus(d),
                    bar_text(a, d, max)
                )
            }
            Some(Err(e)) => e.clone(),
            None if b.current => "current".into(),
            None => String::new(),
        };
        let wt = b.worktree.as_ref().map(|p| format!("  [worktree {}]", tilde(p))).unwrap_or_default();
        out += &format!("{mark} {} {} {}{wt}\n", pad(&b.name, 24), pad(&info, 70), b.rel_date);
    }
    io::stdout().write_all(out.as_bytes()).map_err(|e| e.to_string())
}

fn commit_summary(c: &Commit) -> String {
    let (a, d) = totals(&c.files);
    let stat = if c.merge {
        "merge".to_string()
    } else {
        format!("{}f {} {}", c.files.len(), plus(a), minus(d))
    };
    format!("{} {}  [{} · {}]  {}", c.short, c.subject, c.author, c.rel_date, stat)
}

// -------------------------------------------------------------------- TUI

/// Results from background git jobs. Log/Stats carry the generation of the
/// request so a slow, superseded job can never overwrite newer data.
enum Msg {
    Work(Result<WorkTree, String>),
    Log(u64, Result<Log, String>),
    Stats(u64, Result<Stats, String>),
    Branches(u64, Result<Branches, String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Staged,
    Unstaged,
    Untracked,
}

enum WorkRow {
    Header { title: &'static str, count: usize, added: u64, deleted: u64 },
    File { kind: Kind, file: FileStat },
}

struct App {
    repo: Arc<Repo>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    limit: usize,
    tab: usize,
    range: usize,
    work: Option<WorkTree>,
    work_rows: Vec<WorkRow>,
    work_state: TableState,
    log: Option<Log>,
    log_state: TableState,
    stats: Option<Stats>,
    stats_state: TableState,
    branches: Option<Branches>,
    branch_state: TableState,
    /// Background job running, per source (WORK, LOG, STATS, BRANCHES).
    busy: [bool; 4],
    log_gen: u64,
    stats_gen: u64,
    branch_gen: u64,
    last_head: Option<String>,
    last_tips: Option<String>,
    last_work: Instant,
    /// Last error per source (WORK, LOG, STATS, BRANCHES, PAGER); cleared by
    /// that source's next success, so one source never hides another's failure.
    errors: [Option<String>; 5],
    quit: bool,
}

const WORK: usize = 0;
const LOG: usize = 1;
const STATS: usize = 2;
const BRANCHES: usize = 3;
const PAGER: usize = 4;

impl App {
    fn new(repo: Arc<Repo>, o: &Opts) -> App {
        let (tx, rx) = mpsc::channel();
        App {
            repo,
            tx,
            rx,
            limit: o.limit,
            tab: 0,
            range: o.range,
            work: None,
            work_rows: Vec::new(),
            work_state: TableState::default(),
            log: None,
            log_state: TableState::default(),
            stats: None,
            stats_state: TableState::default(),
            branches: None,
            branch_state: TableState::default(),
            busy: [false; 4],
            log_gen: 0,
            stats_gen: 0,
            branch_gen: 0,
            last_head: None,
            last_tips: None,
            last_work: Instant::now(),
            errors: Default::default(),
            quit: false,
        }
    }

    fn spawn_work(&mut self) {
        if self.busy[WORK] {
            return;
        }
        self.busy[WORK] = true;
        self.last_work = Instant::now();
        let (repo, tx) = (self.repo.clone(), self.tx.clone());
        thread::spawn(move || {
            let _ = tx.send(Msg::Work(repo.worktree()));
        });
    }

    fn spawn_log(&mut self) {
        self.busy[LOG] = true;
        self.log_gen += 1;
        let (repo, tx, limit, generation) = (self.repo.clone(), self.tx.clone(), self.limit, self.log_gen);
        thread::spawn(move || {
            let _ = tx.send(Msg::Log(generation, repo.log(limit)));
        });
    }

    fn spawn_stats(&mut self) {
        self.busy[STATS] = true;
        self.stats_gen += 1;
        let (repo, tx, generation) = (self.repo.clone(), self.tx.clone(), self.stats_gen);
        let days = RANGES[self.range].1;
        thread::spawn(move || {
            let _ = tx.send(Msg::Stats(generation, repo.stats(days)));
        });
    }

    fn spawn_branches(&mut self) {
        self.busy[BRANCHES] = true;
        self.branch_gen += 1;
        let (repo, tx, generation) = (self.repo.clone(), self.tx.clone(), self.branch_gen);
        thread::spawn(move || {
            let _ = tx.send(Msg::Branches(generation, repo.branches()));
        });
    }

    fn refresh_all(&mut self) {
        self.spawn_work();
        self.spawn_log();
        self.spawn_stats();
        self.spawn_branches();
    }

    fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Work(r) => {
                self.busy[WORK] = false;
                match r {
                    Ok(w) => {
                        let head_changed = self.last_head.as_ref().is_some_and(|h| *h != w.head);
                        // A branch can move while HEAD stays put, e.g. an agent
                        // committing in another worktree.
                        let tips_changed = self.last_tips.as_ref().is_some_and(|t| *t != w.branch_tips);
                        self.last_head = Some(w.head.clone());
                        self.last_tips = Some(w.branch_tips.clone());
                        self.set_work(w);
                        self.errors[WORK] = None;
                        if head_changed {
                            self.spawn_log();
                            self.spawn_stats();
                        }
                        // Every comparison is against HEAD, so HEAD moving
                        // changes all of them.
                        if head_changed || tips_changed {
                            self.spawn_branches();
                        }
                    }
                    Err(e) => self.errors[WORK] = Some(e),
                }
            }
            Msg::Log(generation, r) => {
                if generation != self.log_gen {
                    return;
                }
                self.busy[LOG] = false;
                match r {
                    Ok(l) => {
                        self.set_log(l);
                        self.errors[LOG] = None;
                    }
                    Err(e) => self.errors[LOG] = Some(e),
                }
            }
            Msg::Stats(generation, r) => {
                // Also drops results for a range the user has already moved away from.
                if generation != self.stats_gen {
                    return;
                }
                self.busy[STATS] = false;
                match r {
                    Ok(s) => {
                        let n = s.by_day.len();
                        self.stats = Some(s);
                        clamp(&mut self.stats_state, n);
                        self.errors[STATS] = None;
                    }
                    Err(e) => self.errors[STATS] = Some(e),
                }
            }
            Msg::Branches(generation, r) => {
                if generation != self.branch_gen {
                    return;
                }
                self.busy[BRANCHES] = false;
                match r {
                    Ok(b) => {
                        self.set_branches(b);
                        self.errors[BRANCHES] = None;
                    }
                    Err(e) => self.errors[BRANCHES] = Some(e),
                }
            }
        }
    }

    fn selected_file(&self) -> Option<(Kind, &FileStat)> {
        match self.work_rows.get(self.work_state.selected()?) {
            Some(WorkRow::File { kind, file }) => Some((*kind, file)),
            _ => None,
        }
    }

    fn set_work(&mut self, w: WorkTree) {
        let keep = self.selected_file().map(|(k, f)| (k, f.path.clone()));
        let mut rows = Vec::new();
        for (kind, title, files) in [
            (Kind::Staged, "Staged", &w.staged),
            (Kind::Unstaged, "Unstaged", &w.unstaged),
            (Kind::Untracked, "Untracked", &w.untracked),
        ] {
            if files.is_empty() {
                continue;
            }
            let (added, deleted) = totals(files);
            rows.push(WorkRow::Header { title, count: files.len(), added, deleted });
            for f in files {
                rows.push(WorkRow::File { kind, file: f.clone() });
            }
        }
        let old_idx = self.work_state.selected().unwrap_or(0);
        self.work_rows = rows;
        self.work = Some(w);
        let found = keep.and_then(|(k, p)| {
            self.work_rows
                .iter()
                .position(|r| matches!(r, WorkRow::File { kind, file } if *kind == k && file.path == p))
        });
        let idx = found.unwrap_or(old_idx.min(self.work_rows.len().saturating_sub(1)));
        self.work_state.select(if self.work_rows.is_empty() { None } else { Some(idx) });
        self.move_work(0);
    }

    fn set_log(&mut self, l: Log) {
        let keep = self.selected_commit().map(|c| c.hash.clone());
        let found = keep.and_then(|h| {
            l.rows.iter().position(|r| r.commit.is_some_and(|i| l.commits[i].hash == h))
        });
        let first = l.rows.iter().position(|r| r.commit.is_some());
        self.log_state.select(found.or(first));
        self.log = Some(l);
    }

    fn selected_commit(&self) -> Option<&Commit> {
        let log = self.log.as_ref()?;
        let i = log.rows.get(self.log_state.selected()?)?.commit?;
        log.commits.get(i)
    }

    /// Keeps the selection on the same branch by name; the list is sorted by
    /// commit date, so a branch that just got a commit moves to the top.
    fn set_branches(&mut self, b: Branches) {
        let keep = self.selected_branch().map(|x| x.name.clone());
        if let Some(i) = keep.and_then(|n| b.list.iter().position(|x| x.name == n)) {
            self.branch_state.select(Some(i));
        }
        clamp(&mut self.branch_state, b.list.len());
        self.branches = Some(b);
    }

    fn selected_branch(&self) -> Option<&Branch> {
        self.branches.as_ref()?.list.get(self.branch_state.selected()?)
    }

    /// Move selection by `delta`, skipping section headers.
    fn move_work(&mut self, delta: isize) {
        let rows = &self.work_rows;
        step(&mut self.work_state, rows.len(), delta, |i| matches!(rows[i], WorkRow::File { .. }));
    }

    fn move_log(&mut self, delta: isize) {
        let Some(log) = &self.log else { return };
        let rows = &log.rows;
        step(&mut self.log_state, rows.len(), delta, |i| rows[i].commit.is_some());
    }

    fn move_sel(&mut self, delta: isize) {
        match self.tab {
            0 => self.move_work(delta),
            1 => self.move_log(delta),
            2 => {
                let n = self.stats.as_ref().map_or(0, |s| s.by_day.len());
                step(&mut self.stats_state, n, delta, |_| true);
            }
            _ => {
                let n = self.branches.as_ref().map_or(0, |b| b.list.len());
                step(&mut self.branch_state, n, delta, |_| true);
            }
        }
    }

    fn open_selected(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let args: Vec<String> = match self.tab {
            0 => {
                let Some((kind, f)) = self.selected_file() else { return Ok(()) };
                let mut a: Vec<String> = match kind {
                    Kind::Staged => vec!["diff".into(), "--cached".into()],
                    Kind::Unstaged => vec!["diff".into()],
                    Kind::Untracked => vec!["diff".into(), "--no-index".into()],
                };
                a.push("--color=always".into());
                a.push("--".into());
                if kind == Kind::Untracked {
                    a.push("/dev/null".into());
                } else {
                    a.extend(f.old_path.clone());
                }
                a.push(f.path.clone());
                a
            }
            1 => {
                let Some(c) = self.selected_commit() else { return Ok(()) };
                vec!["show".into(), "--color=always".into(), "--stat".into(), "--patch".into(), c.hash.clone()]
            }
            3 => {
                // The whole branch since the merge base, the same range the
                // numbers come from. Nothing to page for an empty diff.
                let Some(b) = self.selected_branch() else { return Ok(()) };
                let Some(Ok(c)) = &b.cmp else { return Ok(()) };
                if c.files.is_empty() {
                    return Ok(());
                }
                let mut a: Vec<String> = ["diff", "--color=always", "--stat", "--patch"].map(String::from).into();
                a.extend([c.merge_base.clone(), b.tip.clone(), "--".into()]);
                a
            }
            _ => return Ok(()),
        };
        disable_raw_mode()?;
        execute!(io::stdout(), LeaveAlternateScreen, cursor::Show)?;
        let res = page_git(&self.repo.root, &args);
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
        // Force a full redraw. Not `terminal.clear()`: it queries the cursor
        // position (DSR) and errors out on terminals that never answer it.
        let size = terminal.size()?;
        terminal.resize(Rect::new(0, 0, size.width, size.height))?;
        self.errors[PAGER] = res.err();
        Ok(())
    }

    /// Shown while a tab has no data yet: its error if loading failed, else "loading…".
    fn placeholder(&self, source: usize) -> Paragraph<'static> {
        match &self.errors[source] {
            Some(e) => Paragraph::new(format!(" {e}")).fg(DEL),
            None => Paragraph::new(" loading…").fg(DIM),
        }
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('d') if ctrl => self.move_sel(15),
            KeyCode::Char('u') if ctrl => self.move_sel(-15),
            KeyCode::Char('1') => self.tab = 0,
            KeyCode::Char('2') => self.tab = 1,
            KeyCode::Char('3') => self.tab = 2,
            KeyCode::Char('4') => self.tab = 3,
            KeyCode::Tab => self.tab = (self.tab + 1) % TABS.len(),
            KeyCode::BackTab => self.tab = (self.tab + TABS.len() - 1) % TABS.len(),
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(15),
            KeyCode::PageUp => self.move_sel(-15),
            KeyCode::Char('g') | KeyCode::Home => self.move_sel(-1_000_000),
            KeyCode::Char('G') | KeyCode::End => self.move_sel(1_000_000),
            KeyCode::Enter => self.open_selected(terminal)?,
            KeyCode::Char('r') => self.refresh_all(),
            KeyCode::Char('t') => {
                self.range = (self.range + 1) % RANGES.len();
                self.spawn_stats();
            }
            _ => {}
        }
        Ok(())
    }

    // ------------------------------------------------------------ render

    fn render(&mut self, f: &mut Frame) {
        // The tab bar and the key hints are boxed like the panels between
        // them, so each takes three rows.
        let [top, body, bottom] =
            Layout::vertical([Constraint::Length(3), Constraint::Fill(1), Constraint::Length(3)]).areas(f.area());
        self.render_top(f, top);
        match self.tab {
            0 => self.render_changes(f, body),
            1 => self.render_log(f, body),
            2 => self.render_stats(f, body),
            _ => self.render_branches(f, body),
        }
        self.render_bottom(f, bottom);
    }

    fn render_top(&self, f: &mut Frame, area: Rect) {
        let branch = self.work.as_ref().map_or("…", |w| w.branch.as_str());
        // Leading space so the pill cap does not touch the border.
        let mut spans = vec![Span::raw(" ")];
        spans.extend(pill(" diffdash ", Some(ACCENT)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(repo_name(&self.repo.root), Style::new().bold()));
        spans.push(Span::styled(format!("  {branch}   "), Style::new().fg(Color::Magenta)));
        for (i, t) in TABS.iter().enumerate() {
            let label = format!(" {} {t} ", i + 1);
            if i == self.tab {
                spans.extend(pill(label, None));
            } else {
                spans.extend(pill_slot(label, Style::new().fg(DIM)));
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)).block(panel()), area);
    }

    fn render_bottom(&self, f: &mut Frame, area: Rect) {
        let hint = match self.tab {
            0 => "j/k move · Enter diff · r refresh · 1-4 tabs · q quit",
            1 => "j/k move · Enter show commit · r refresh · 1-4 tabs · q quit",
            2 => "t range · j/k scroll · r refresh · 1-4 tabs · q quit",
            _ => "j/k move · Enter branch diff · r refresh · 1-4 tabs · q quit",
        };
        let mut spans = vec![Span::styled(format!(" {hint}"), Style::new().fg(DIM))];
        // The 2 s working-tree poll is not shown; it would flicker constantly.
        if self.busy[LOG] || self.busy[STATS] || self.busy[BRANCHES] {
            spans.push(Span::styled("  loading…", Style::new().fg(ACCENT)));
        }
        if let Some(e) = self.errors.iter().flatten().next() {
            spans.push(Span::styled(format!("  {e}"), Style::new().fg(DEL)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).block(panel()), area);
    }

    fn render_changes(&mut self, f: &mut Frame, area: Rect) {
        let Some(w) = &self.work else {
            f.render_widget(self.placeholder(WORK), area);
            return;
        };
        let max = w.staged.iter().chain(&w.unstaged).chain(&w.untracked).map(churn).max().unwrap_or(0);
        let (n, ta, td) = w.headline();
        let title = Line::from(vec![
            Span::raw(format!(" Changes since HEAD · {n} files  ")),
            Span::styled(plus(ta), Style::new().fg(ADD).bold()),
            Span::raw(" "),
            Span::styled(minus(td), Style::new().fg(DEL).bold()),
            Span::raw(" "),
        ]);
        let block = panel().title(title);
        if self.work_rows.is_empty() {
            f.render_widget(Paragraph::new(" working tree clean").fg(DIM).block(block), area);
            return;
        }
        let cols = file_cols(
            area.width,
            self.work_rows.iter().map(|r| match r {
                WorkRow::Header { title, count, added, deleted } => {
                    (cells(&format!("{title} ({count})")), *added, *deleted)
                }
                WorkRow::File { kind, file } => {
                    (file_name_width(file, *kind == Kind::Untracked), file.add(), file.del())
                }
            }),
        );
        let rows: Vec<Row> = self
            .work_rows
            .iter()
            .map(|r| match r {
                WorkRow::Header { title, count, added, deleted } => Row::new(vec![
                    Cell::from(Line::from(vec![
                        Span::styled(format!("{title} "), Style::new().fg(Color::Yellow).bold()),
                        Span::styled(format!("({count})"), Style::new().fg(DIM)),
                    ])),
                    Cell::from(Line::from(plus(*added)).right_aligned().fg(ADD).bold()),
                    Cell::from(Line::from(minus(*deleted)).right_aligned().fg(DEL).bold()),
                    Cell::from(""),
                ]),
                WorkRow::File { kind, file } => file_row(file, max, *kind == Kind::Untracked, &cols),
            })
            .collect();
        f.render_stateful_widget(file_table(rows, &cols).block(block), area, &mut self.work_state);
    }

    fn render_log(&mut self, f: &mut Frame, area: Rect) {
        let Some(log) = &self.log else {
            f.render_widget(self.placeholder(LOG), area);
            return;
        };
        let [top, bottom] = Layout::vertical([Constraint::Percentage(62), Constraint::Fill(1)]).areas(area);
        let title = format!(" Log · latest {} commits ", log.commits.len());
        let table = log_table(log, top.width).block(panel().title(title));
        f.render_stateful_widget(table, top, &mut self.log_state);

        let block = panel();
        let Some(c) = self.selected_commit() else {
            f.render_widget(block, bottom);
            return;
        };
        let (a, d) = totals(&c.files);
        let title = Line::from(vec![
            Span::styled(format!(" {} ", c.short), Style::new().fg(Color::Yellow)),
            Span::raw(format!("{} · {} · {} files  ", c.author, c.date, c.files.len())),
            Span::styled(plus(a), Style::new().fg(ADD).bold()),
            Span::raw(" "),
            Span::styled(minus(d), Style::new().fg(DEL).bold()),
            Span::raw(" "),
        ]);
        let block = block.title(title);
        if c.merge {
            let msg = " merge commit: git log reports no per-file stats for merges (Enter shows the commit)";
            f.render_widget(Paragraph::new(msg).fg(DIM).block(block), bottom);
            return;
        }
        render_files(f, bottom, block, &c.files);
    }

    fn render_stats(&mut self, f: &mut Frame, area: Rect) {
        let Some(s) = &self.stats else {
            f.render_widget(self.placeholder(STATS), area);
            return;
        };
        let (rname, _) = RANGES[self.range];
        let mut title = vec![Span::raw(" Stats · range ")];
        for (i, (n, _)) in RANGES.iter().enumerate() {
            let label = format!(" {n} ");
            if i == self.range {
                title.extend(pill(label, Some(ACCENT)));
            } else {
                title.extend(pill_slot(label, Style::new().fg(DIM)));
            }
        }
        title.push(Span::raw(" "));
        let [head, body] = Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).areas(area);
        let net = s.added as i64 - s.deleted as i64;
        let summary = vec![
            Span::raw(" "),
            Span::styled(format!("{} commits", s.commits), Style::new().bold()),
            Span::raw("   "),
            Span::styled(plus(s.added), Style::new().fg(ADD).bold()),
            Span::raw("  "),
            Span::styled(minus(s.deleted), Style::new().fg(DEL).bold()),
            Span::raw("   net "),
            Span::styled(format!("{net:+}"), Style::new().bold()),
            Span::styled(format!("   ({rname}, merges excluded)"), Style::new().fg(DIM)),
        ];
        f.render_widget(
            Paragraph::new(Line::from(summary))
                .block(panel().title(Line::from(title))),
            head,
        );
        let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Fill(1)]).areas(body);
        let [rtop, rbot] = Layout::vertical([Constraint::Percentage(55), Constraint::Fill(1)]).areas(right);
        let boxed = |t: &'static str| panel().title(t);
        let days = bucket_table(&s.by_day, 11, left).block(boxed(" By day "));
        f.render_stateful_widget(days, left, &mut self.stats_state);
        f.render_widget(bucket_table(&s.by_dir, 18, rtop).block(boxed(" By top-level directory ")), rtop);
        f.render_widget(bucket_table(&s.by_author, 18, rbot).block(boxed(" By author ")), rbot);
    }

    fn render_branches(&mut self, f: &mut Frame, area: Rect) {
        let Some(br) = &self.branches else {
            f.render_widget(self.placeholder(BRANCHES), area);
            return;
        };
        let title = format!(" Branches · {} local · compared with {} ", br.list.len(), br.head_name);
        if br.list.is_empty() {
            f.render_widget(Paragraph::new(" no branches yet").fg(DIM).block(panel().title(title)), area);
            return;
        }
        // Branch lists are usually short; the selected branch's files get the rest.
        let list_h = (br.list.len() as u16 + 2).min(area.height / 2).max(3);
        let [top, bottom] = Layout::vertical([Constraint::Length(list_h), Constraint::Fill(1)]).areas(area);
        let table = branch_table(&br.list, top.width).block(panel().title(title));
        f.render_stateful_widget(table, top, &mut self.branch_state);

        let Some(b) = self.selected_branch() else {
            f.render_widget(panel(), bottom);
            return;
        };
        let mut block = panel();
        if let Some(path) = &b.worktree {
            // The note goes first so a long path is what gets cut off.
            let note = format!(" uncommitted changes in its worktree are not counted · {} ", tilde(path));
            block = block.title_bottom(Line::from(note).fg(DIM));
        }
        let name = Span::styled(format!(" {} ", b.name), Style::new().fg(Color::Magenta).bold());
        let c = match &b.cmp {
            Some(Ok(c)) => c,
            other => {
                let (title, text, color) = match other {
                    _ if b.current => (
                        "· current branch ",
                        " the other branches are compared with this one, from where they forked".to_string(),
                        DIM,
                    ),
                    Some(Err(e)) => ("", format!(" {e}"), DEL),
                    _ => ("", " HEAD has no commits yet, so there is nothing to compare with".to_string(), DIM),
                };
                let block = block.title(Line::from(vec![name, Span::raw(title)]));
                f.render_widget(Paragraph::new(text).fg(color).block(block), bottom);
                return;
            }
        };
        let (a, d) = totals(&c.files);
        let title = Line::from(vec![
            name,
            Span::raw(format!(
                "· {} ahead, {} behind · merge base {} · {} files  ",
                c.ahead,
                c.behind,
                &c.merge_base[..c.merge_base.len().min(7)],
                c.files.len()
            )),
            Span::styled(plus(a), Style::new().fg(ADD).bold()),
            Span::raw(" "),
            Span::styled(minus(d), Style::new().fg(DEL).bold()),
            Span::raw(" "),
        ]);
        let block = block.title(title);
        if c.files.is_empty() {
            let msg = if c.ahead == 0 {
                format!(" every commit on this branch is already in {}", br.head_name)
            } else {
                " no file changes since the merge base".to_string()
            };
            f.render_widget(Paragraph::new(msg).fg(DIM).block(block), bottom);
            return;
        }
        render_files(f, bottom, block, &c.files);
    }
}

fn branch_churn(b: &Branch) -> u64 {
    match &b.cmp {
        Some(Ok(c)) => {
            let (a, d) = totals(&c.files);
            a + d
        }
        _ => 0,
    }
}

/// Name cell of a branch row in `width` cells: current-branch marker, name and
/// dim tags. The tags go before any of the name, since the pane below says the
/// same for the selected branch.
fn branch_name(b: &Branch, width: usize) -> Line<'static> {
    let (mark, style) = if b.current {
        ("* ", Style::new().fg(Color::Magenta).bold())
    } else {
        ("  ", Style::new())
    };
    let mut tags = Vec::new();
    if b.worktree.is_some() {
        tags.push(Span::styled(" worktree", Style::new().fg(DIM)));
    }
    if let Some(Err(_)) = &b.cmp {
        tags.push(Span::styled("  no common history", Style::new().fg(DIM)));
    }
    let room = width.saturating_sub(BRANCH_MARK);
    let tags_w: usize = tags.iter().map(Span::width).sum();
    if cells(&b.name) + tags_w > room {
        tags.clear();
    }
    let mut spans = vec![Span::raw(format!(" {mark}")), Span::styled(shorten_path(&b.name, room), style)];
    spans.extend(tags);
    Line::from(spans)
}

/// Column widths of the branch list: name, ahead, behind, files, added, deleted,
/// bar plus one cell of margin, date. `words` spells out `1 ahead`, otherwise
/// the two counts are `↑1` and `↓0`.
struct BranchCols {
    words: bool,
    widths: [usize; 8],
}

/// Fits the branch list into a panel `area_w` cells wide. While names and a whole
/// bar fit, the columns have their full widths. Below that, the padding goes
/// first and the bar takes what the names leave. Then the words in `1 ahead` go,
/// and the date. Only then are names shortened, since they say which branch a row
/// is; a name kept from `NAME_FLOOR` also hides the counts other than `+` and `-`.
fn branch_cols(area_w: u16, list: &[Branch]) -> BranchCols {
    // Name alone and name with its tags; only the name can hide a column.
    let (mut name, mut full, mut date) = (0, 0, 0);
    let (mut ahead, mut behind) = (1, 1);
    // Three cells keep small counts from shifting the columns.
    let (mut files, mut add, mut del) = (3, 3, 3);
    for b in list {
        name = name.max(BRANCH_MARK + cells(&b.name));
        full = full.max(branch_name(b, usize::MAX).width());
        date = date.max(cells(&b.rel_date));
        if let Some(Ok(c)) = &b.cmp {
            let (a, d) = totals(&c.files);
            ahead = ahead.max(c.ahead.to_string().len());
            behind = behind.max(c.behind.to_string().len());
            files = files.max(format!("{}f", c.files.len()).len());
            add = add.max(plus(a).len());
            del = del.max(minus(d).len());
        }
    }
    let date = date.min(14);
    let inner = (area_w as usize).saturating_sub(2);
    let (ahead_w, behind_w) = (ahead + " ahead".len(), behind + " behind".len());
    // Ahead, behind, files, added, deleted, date.
    let roomy = [ahead_w.max(9), behind_w.max(11), files.max(5), add.max(7), del.max(7), 14];
    let goal = name.min(NAME_GOAL);
    // Spelled out, what a layout must leave for names, and its columns.
    let layouts = [
        (true, goal, [ahead_w, behind_w, files, add, del, date]),
        (false, goal, [ahead + 1, behind + 1, files, add, del, date]),
        (false, name.min(NAME_FLOOR), [ahead + 1, behind + 1, files, add, del, 0]),
        (false, 0, [0, 0, 0, add, del, 0]),
    ];
    // The whole bar is 24 cells, a cell of margin and a gap.
    let (words, others) = if room_beside(inner, &roomy) >= full + BAR + 2 {
        (true, roomy)
    } else {
        let (words, _, others) = layouts
            .into_iter()
            .find(|(_, goal, others)| room_beside(inner, others) >= *goal)
            .unwrap_or(layouts[3]);
        (words, others)
    };
    let room = room_beside(inner, &others);
    // A bar gets what names with their tags leave, but not in place of the counts.
    let bar = if others[0] > 0 { room.saturating_sub(full + 2).min(BAR) } else { 0 };
    let bar_col = if bar > 0 { bar + 1 } else { 0 };
    let [ahead, behind, files, add, del, date] = others;
    BranchCols { words, widths: [room_beside(room, &[bar_col]), ahead, behind, files, add, del, bar_col, date] }
}

fn branch_row(b: &Branch, cols: &BranchCols, max: u64) -> Row<'static> {
    let [name_w, .., bar_col, _] = cols.widths;
    let mut row = vec![Cell::from(branch_name(b, name_w))];
    match &b.cmp {
        Some(Ok(c)) => {
            let (a, d) = totals(&c.files);
            // Zero counts are dimmed so branches with work stand out.
            let count = |n: u64, word: &str, arrow: &str| {
                let text = if cols.words { format!("{n} {word}") } else { format!("{arrow}{n}") };
                let line = Line::from(text).right_aligned();
                Cell::from(if n == 0 { line.fg(DIM) } else { line })
            };
            row.extend([
                count(c.ahead, "ahead", "↑"),
                count(c.behind, "behind", "↓"),
                Cell::from(Line::from(format!("{}f", c.files.len())).right_aligned().fg(DIM)),
                Cell::from(Line::from(plus(a)).right_aligned().fg(ADD)),
                Cell::from(Line::from(minus(d)).right_aligned().fg(DEL)),
                Cell::from(bar_line(a, d, max, bar_col.saturating_sub(1))),
            ]);
        }
        _ => row.extend((0..6).map(|_| Cell::from(""))),
    }
    row.push(Cell::from(Line::from(b.rel_date.clone()).fg(DIM)));
    Row::new(shown(&cols.widths, row))
}

fn branch_table(list: &[Branch], area_w: u16) -> Table<'static> {
    let cols = branch_cols(area_w, list);
    let max = list.iter().map(branch_churn).max().unwrap_or(0);
    let rows: Vec<Row> = list.iter().map(|b| branch_row(b, &cols, max)).collect();
    shown_table(rows, &cols.widths)
}

/// `path` with the home directory shown as `~`.
fn tilde(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix(home.as_str()) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// Column widths of a file list. `bar_col` is the bar plus one cell of margin.
struct FileCols {
    name: usize,
    add: usize,
    del: usize,
    bar_col: usize,
}

/// Fits a file list into a panel `area_w` cells wide, given each row's full
/// name width and its added and deleted counts. As room runs out, the padding
/// of the number columns goes first, then the bar, and only then do names get
/// shortened.
fn file_cols(area_w: u16, rows: impl Iterator<Item = (usize, u64, u64)>) -> FileCols {
    // Three cells fit `bin` and keep small counts from shifting the columns.
    let (mut name, mut add, mut del) = (0, 3, 3);
    for (n, a, d) in rows {
        name = name.max(n);
        add = add.max(plus(a).len());
        del = del.max(minus(d).len());
    }
    // Two borders and the three gaps between the columns.
    let inner = (area_w as usize).saturating_sub(2 + 3);
    if inner > name + 2 * NUM_W + BAR {
        add = add.max(NUM_W);
        del = del.max(NUM_W);
    }
    let room = inner.saturating_sub(add + del);
    let bar_col = room.saturating_sub(name.min(NAME_GOAL)).min(BAR + 1);
    FileCols { name: room - bar_col, add, del, bar_col }
}

/// Cells the name column needs to show `f` in full.
fn file_name_width(f: &FileStat, untracked: bool) -> usize {
    INDENT.len() + cells(&f.display_path()) + if untracked { NEW_TAG.len() } else { 0 }
}

fn file_row(f: &FileStat, max: u64, untracked: bool, cols: &FileCols) -> Row<'static> {
    let tag = if untracked { NEW_TAG } else { "" };
    let room = cols.name.saturating_sub(INDENT.len() + tag.len());
    let mut name = vec![Span::raw(INDENT), Span::raw(fit_path(f, room))];
    if untracked {
        name.push(Span::styled(NEW_TAG, Style::new().fg(ADD)));
    }
    if f.is_binary() {
        return Row::new(vec![
            Cell::from(Line::from(name)),
            Cell::from(""),
            Cell::from(Line::from("bin").right_aligned().fg(DIM)),
            Cell::from(""),
        ]);
    }
    Row::new(vec![
        Cell::from(Line::from(name)),
        Cell::from(Line::from(plus(f.add())).right_aligned().fg(ADD)),
        Cell::from(Line::from(minus(f.del())).right_aligned().fg(DEL)),
        Cell::from(bar_line(f.add(), f.del(), max, cols.bar_col.saturating_sub(1))),
    ])
}

fn file_table(rows: Vec<Row<'static>>, cols: &FileCols) -> Table<'static> {
    let widths = [cols.name, cols.add, cols.del, cols.bar_col].map(|w| Constraint::Length(w as u16));
    Table::new(rows, widths).row_highlight_style(selected_style())
}

/// The file list of a commit or branch, largest change first.
fn render_files(frame: &mut Frame, area: Rect, block: Block<'static>, files: &[FileStat]) {
    let mut files: Vec<&FileStat> = files.iter().collect();
    files.sort_by_key(|f| std::cmp::Reverse(churn(f)));
    let max = files.first().map_or(0, |f| churn(f));
    let cols = file_cols(area.width, files.iter().map(|f| (file_name_width(f, false), f.add(), f.del())));
    let rows: Vec<Row> = files.iter().map(|f| file_row(f, max, false, &cols)).collect();
    frame.render_widget(file_table(rows, &cols).block(block), area);
}

/// Room left for a table's main column beside these other columns, each of
/// which also takes a gap. A column of width 0 is hidden and takes neither.
fn room_beside(inner: usize, others: &[usize]) -> usize {
    inner.saturating_sub(others.iter().filter(|w| **w > 0).map(|w| w + 1).sum())
}

/// The cells or constraints of the columns that have a width, so that a hidden
/// column does not take up a gap.
fn shown<T>(widths: &[usize], items: impl IntoIterator<Item = T>) -> Vec<T> {
    widths.iter().zip(items).filter(|(w, _)| **w > 0).map(|(_, item)| item).collect()
}

/// A table of rows built with [`shown`] from these widths.
fn shown_table(rows: Vec<Row<'static>>, widths: &[usize]) -> Table<'static> {
    let constraints = shown(widths, widths.iter().map(|w| Constraint::Length(*w as u16)));
    Table::new(rows, constraints).row_highlight_style(selected_style())
}

/// Column widths of the commit list. `widths` is the commit (graph, hash and
/// subject), author, date, files, added, deleted. `hash` is what the hash takes
/// in the commit column, with its space: 0 when it is hidden.
struct LogCols {
    hash: usize,
    widths: [usize; 6],
}

/// Fits the commit list into a panel `area_w` cells wide. Widths come from all
/// commits, so scrolling does not move a column. While a useful stretch of
/// subject fits, the columns have their full widths. Below that, the padding
/// goes first, then the author, the date and the hash: the pane below still
/// shows all three for the selected commit. A subject that is still too long is
/// clipped on the right, where it matters least.
fn log_cols(area_w: u16, log: &Log) -> LogCols {
    let graph = log.rows.iter().map(|r| cells(&r.graph)).max().unwrap_or(0);
    let (mut hash, mut subject, mut author, mut date) = (0, 0, 0, 0);
    // Three cells keep small counts from shifting the columns.
    let (mut files, mut add, mut del) = (3, 3, 3);
    for c in &log.commits {
        let (a, d) = totals(&c.files);
        hash = hash.max(cells(&c.short) + 1);
        subject = subject.max(cells(&c.subject));
        author = author.max(cells(&c.author));
        date = date.max(cells(&c.rel_date));
        files = files.max(if c.merge { "merge".len() } else { format!("{}f", c.files.len()).len() });
        add = add.max(plus(a).len());
        del = del.max(minus(d).len());
    }
    let (author, date) = (author.min(14), date.min(14));
    let inner = (area_w as usize).saturating_sub(2);
    // The graph takes its own width and a space.
    let need = graph + 1 + subject.min(SUBJECT_GOAL);
    // Hash, then the columns beside the commit column.
    let layouts = [
        [hash, 14, 14, files.max(5), add.max(7), del.max(7)],
        [hash, author, date, files, add, del],
        [hash, 0, date, files, add, del],
        [hash, 0, 0, files, add, del],
        [0, 0, 0, files, add, del],
    ];
    let [hash, others @ ..] =
        layouts.into_iter().find(|[h, others @ ..]| room_beside(inner, others) >= need + h).unwrap_or(layouts[4]);
    let [author, date, files, add, del] = others;
    LogCols { hash, widths: [room_beside(inner, &others), author, date, files, add, del] }
}

fn log_row(r: &GraphRow, log: &Log, cols: &LogCols) -> Row<'static> {
    let graph = Span::styled(r.graph.clone(), Style::new().fg(Color::Blue));
    let Some(c) = r.commit.map(|i| &log.commits[i]) else {
        return Row::new(vec![Cell::from(Line::from(graph))]);
    };
    let (a, d) = totals(&c.files);
    let (files, add, del) = if c.merge {
        (Span::styled("merge", Style::new().fg(DIM)), Span::raw(""), Span::raw(""))
    } else {
        (
            Span::styled(format!("{}f", c.files.len()), Style::new().fg(DIM)),
            Span::styled(plus(a), Style::new().fg(ADD)),
            Span::styled(minus(d), Style::new().fg(DEL)),
        )
    };
    let mut commit = vec![graph, Span::raw(" ")];
    if cols.hash > 0 {
        commit.extend([Span::styled(c.short.clone(), Style::new().fg(Color::Yellow)), Span::raw(" ")]);
    }
    commit.push(Span::raw(c.subject.clone()));
    Row::new(shown(&cols.widths, [
        Cell::from(Line::from(commit)),
        Cell::from(Line::from(c.author.clone()).fg(Color::Magenta)),
        Cell::from(Line::from(c.rel_date.clone()).fg(DIM)),
        Cell::from(Line::from(files).right_aligned()),
        Cell::from(Line::from(add).right_aligned()),
        Cell::from(Line::from(del).right_aligned()),
    ]))
}

fn log_table(log: &Log, area_w: u16) -> Table<'static> {
    let cols = log_cols(area_w, log);
    let rows: Vec<Row> = log.rows.iter().map(|r| log_row(r, log, &cols)).collect();
    shown_table(rows, &cols.widths)
}

/// Table of buckets whose bars are scaled to the space left in `area`, so a
/// narrow pane shrinks every bar instead of clipping only the longest ones.
fn bucket_table(list: &[Bucket], key_width: u16, area: Rect) -> Table<'static> {
    const COUNT_W: u16 = 5;
    const NUM_W: u16 = 8;
    // borders (2) + four column gaps (4) + one cell of right margin.
    let used = 2 + key_width + COUNT_W + 2 * NUM_W + 4 + 1;
    let bar_w = area.width.saturating_sub(used).max(1) as usize;
    let max = list.iter().map(|b| b.added + b.deleted).max().unwrap_or(0);
    let rows: Vec<Row> = list
        .iter()
        .map(|b| {
            Row::new(vec![
                Cell::from(format!(" {}", b.key)),
                Cell::from(Line::from(format!("{}c", b.commits)).right_aligned().fg(DIM)),
                Cell::from(Line::from(plus(b.added)).right_aligned().fg(ADD)),
                Cell::from(Line::from(minus(b.deleted)).right_aligned().fg(DEL)),
                Cell::from(bar_line(b.added, b.deleted, max, bar_w)),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(key_width),
        Constraint::Length(COUNT_W),
        Constraint::Length(NUM_W),
        Constraint::Length(NUM_W),
        Constraint::Fill(1),
    ];
    Table::new(rows, widths).row_highlight_style(selected_style())
}

/// Move a table selection by `delta`, landing only on rows where `ok(i)`.
fn step(state: &mut TableState, len: usize, delta: isize, ok: impl Fn(usize) -> bool) {
    if len == 0 {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0).min(len - 1) as isize;
    let target = (cur + delta).clamp(0, len as isize - 1);
    let dir = if delta < 0 { -1 } else { 1 };
    // Search from target in the travel direction, then fall back the other way.
    for d in [dir, -dir] {
        let mut i = target;
        while (0..len as isize).contains(&i) {
            if ok(i as usize) {
                state.select(Some(i as usize));
                return;
            }
            i += d;
        }
    }
    state.select(None);
}

fn clamp(state: &mut TableState, len: usize) {
    match (state.selected(), len) {
        (_, 0) => state.select(None),
        (None, _) => state.select(Some(0)),
        (Some(i), n) if i >= n => state.select(Some(n - 1)),
        _ => {}
    }
}

/// Pipe `git <args>` into `less -R`.
fn page_git(root: &Path, args: &[String]) -> Result<(), String> {
    let mut git = git::git_cmd(root)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to run git: {e}"))?;
    let out = git.stdout.take().ok_or("no git output")?;
    // -+F overrides a user's LESS=-F, which would make less quit at once on a
    // short diff and look like Enter did nothing.
    let status = Command::new("less").args(["-R", "-+F"]).stdin(out).status();
    let _ = git.wait();
    status.map(|_| ()).map_err(|e| format!("failed to run less: {e}"))
}

fn run_tui(repo: Arc<Repo>, o: &Opts) -> Result<(), String> {
    let mut terminal = ratatui::init();
    let mut app = App::new(repo, o);
    app.refresh_all();
    let res = (|| -> io::Result<()> {
        while !app.quit {
            while let Ok(msg) = app.rx.try_recv() {
                app.handle_msg(msg);
            }
            if app.last_work.elapsed() >= REFRESH {
                app.spawn_work();
            }
            terminal.draw(|f| app.render(f))?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                app.on_key(k.code, k.modifiers, &mut terminal)?;
            }
        }
        Ok(())
    })();
    ratatui::restore();
    res.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::BranchCmp;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn stat(path: &str, added: u64, deleted: u64) -> FileStat {
        FileStat { path: path.into(), old_path: None, added: Some(added), deleted: Some(deleted) }
    }

    /// A file list drawn into a real buffer, one string per screen row. Wide
    /// characters leave a blank cell behind them, so the tests use ASCII paths.
    fn draw_files(width: u16, files: &[FileStat]) -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(width, files.len() as u16 + 2)).unwrap();
        t.draw(|f| {
            let area = f.area();
            render_files(f, area, panel(), files);
        })
        .unwrap();
        let b = t.backend().buffer();
        (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect()).collect()
    }

    const DEEP: &str = "crates/core/src/handlers/auth/login.rs";

    #[test]
    fn path_that_fits_is_untouched() {
        assert_eq!(shorten_path(DEEP, DEEP.len()), DEEP);
        assert_eq!(shorten_path(DEEP, 100), DEEP);
        assert_eq!(shorten_path("README.md", 9), "README.md");
    }

    #[test]
    fn directories_go_first_and_from_the_left() {
        assert_eq!(shorten_path(DEEP, DEEP.len() - 1), "…/core/src/handlers/auth/login.rs");
        assert_eq!(shorten_path(DEEP, 28), "…/src/handlers/auth/login.rs");
        assert_eq!(shorten_path(DEEP, 27), "…/handlers/auth/login.rs");
        assert_eq!(shorten_path(DEEP, 15), "…/auth/login.rs");
        assert_eq!(shorten_path(DEEP, 14), "…/login.rs");
        assert_eq!(shorten_path(DEEP, 10), "…/login.rs");
    }

    #[test]
    fn file_name_is_cut_last() {
        assert_eq!(shorten_path(DEEP, 9), "login.rs");
        assert_eq!(shorten_path(DEEP, 8), "login.rs");
        assert_eq!(shorten_path(DEEP, 7), "…gin.rs");
        assert_eq!(shorten_path(DEEP, 1), "…");
        assert_eq!(shorten_path(DEEP, 0), "");
        assert_eq!(shorten_path("a_long_file_name.rs", 8), "…name.rs");
    }

    #[test]
    fn embedded_repo_keeps_its_name() {
        // git lists these with a trailing slash; `…/` alone would say nothing.
        let path = "vendor/third_party/inner/";
        assert_eq!(shorten_path(path, 20), "…/third_party/inner/");
        assert_eq!(shorten_path(path, 8), "…/inner/");
        assert_eq!(shorten_path(path, 6), "inner/");
        assert_eq!(shorten_path(path, 4), "…er/");
    }

    #[test]
    fn wide_characters_count_two_cells() {
        let path = "实验/笔记/数据分析报告.md";
        assert_eq!(cells(path), 25);
        assert_eq!(shorten_path(path, 25), path);
        assert_eq!(shorten_path(path, 24), "…/笔记/数据分析报告.md");
        assert_eq!(shorten_path(path, 21), "…/数据分析报告.md");
        // The six cells after the `…` hold `.md` and one wide character; half
        // of a second one must not spill over.
        assert_eq!(shorten_path(path, 7), "…告.md");
        for width in 0..30 {
            assert!(cells(&shorten_path(path, width)) <= width, "width {width}");
        }
    }

    #[test]
    fn rename_that_does_not_fit_shows_where_it_went() {
        let f = FileStat {
            path: "src/handlers/new_name.rs".into(),
            old_path: Some("src/old_name.rs".into()),
            added: Some(1),
            deleted: Some(0),
        };
        let full = "src/old_name.rs → src/handlers/new_name.rs";
        assert_eq!(fit_path(&f, cells(full)), full);
        assert_eq!(fit_path(&f, cells(full) - 1), "→ src/handlers/new_name.rs");
        assert_eq!(fit_path(&f, 16), "→ …/new_name.rs");
        assert_eq!(fit_path(&f, 13), "→ new_name.rs");
        for width in 0..50 {
            assert!(cells(&fit_path(&f, width)) <= width, "width {width}");
        }
    }

    #[test]
    fn full_layout_is_kept_while_every_name_fits_it() {
        // Number columns of NUM_W, a whole bar, and the name column gets the rest.
        for width in [106, 120, 200] {
            let cols = file_cols(width, [(60, 12, 3)].into_iter());
            assert_eq!((cols.add, cols.del, cols.bar_col), (NUM_W, NUM_W, BAR + 1), "width {width}");
            assert_eq!(cols.name, width as usize - 5 - 2 * NUM_W - (BAR + 1));
        }
        // Short names keep it down to a much narrower panel, and one cell less
        // is where it stops.
        let cols = file_cols(59, [(13, 12, 3)].into_iter());
        assert_eq!((cols.name, cols.add, cols.del, cols.bar_col), (13, NUM_W, NUM_W, BAR + 1));
        let cols = file_cols(58, [(13, 12, 3)].into_iter());
        assert_eq!((cols.add, cols.del), (3, 3));
    }

    #[test]
    fn number_padding_then_bar_give_way_to_the_names() {
        // Padding goes first: the bar stays whole while the numbers tighten.
        let cols = file_cols(80, [(40, 1234, 5)].into_iter());
        assert_eq!((cols.name, cols.add, cols.del, cols.bar_col), (42, 5, 3, BAR + 1));
        // Then the bar shrinks until the name fits.
        let cols = file_cols(60, [(40, 1234, 5)].into_iter());
        assert_eq!((cols.name, cols.add, cols.del, cols.bar_col), (40, 5, 3, 7));
        // And goes away entirely before any name is shortened.
        let cols = file_cols(40, [(30, 1234, 5)].into_iter());
        assert_eq!((cols.name, cols.add, cols.del, cols.bar_col), (27, 5, 3, 0));
    }

    #[test]
    fn one_very_long_name_does_not_erase_the_bars() {
        // It takes the padding, but the bar only yields up to NAME_GOAL.
        let cols = file_cols(120, [(200, 12, 3)].into_iter());
        assert_eq!((cols.name, cols.add, cols.del, cols.bar_col), (84, 3, 3, BAR + 1));
        let cols = file_cols(60, [(200, 12, 3)].into_iter());
        assert_eq!((cols.name, cols.add, cols.del, cols.bar_col), (NAME_GOAL, 3, 3, 9));
    }

    #[test]
    fn number_columns_fit_their_widest_value() {
        let cols = file_cols(40, [(10, 1, 1), (10, 123456, 1), (10, 1, 1234)].into_iter());
        assert_eq!((cols.add, cols.del), (7, 5));
        // `bin` goes in the deleted column, and huge counts are not clipped.
        let cols = file_cols(40, [(10, 1, 1)].into_iter());
        assert_eq!(cols.del, 3);
        let cols = file_cols(200, [(10, 1_000_000_000, 1)].into_iter());
        assert_eq!(cols.add, 11);
    }

    #[test]
    fn narrow_panel_still_shows_every_file_name() {
        let files = [
            stat("crates/core/src/handlers/auth/session.rs", 1234, 5),
            stat("experiments/2026/results/long_table_name.csv", 12, 0),
            stat("README.md", 4, 0),
        ];
        // 34 is where the longest file name (19 cells) still fits whole.
        for width in 34..=130 {
            let screen = draw_files(width, &files);
            // Rows are sorted by churn; row 0 is the top border.
            for (row, name) in screen[1..].iter().zip(["session.rs", "long_table_name.csv", "README.md"]) {
                assert!(row.contains(name), "width {width}: {row}");
            }
            assert!(screen[1].contains("+1234") && screen[1].contains("-5"), "width {width}: {}", screen[1]);
            // Nothing is shortened or dropped once the panel is wide enough.
            if width >= 96 {
                assert!(screen[1].contains(&files[0].path), "width {width}: {}", screen[1]);
                assert!(screen[1].contains(&"━".repeat(BAR)), "width {width}: {}", screen[1]);
            }
        }
    }

    #[test]
    fn tiny_panels_do_not_panic() {
        let files = [stat("a/b/c.rs", 12345, 678), stat("实验/数据.md", 1, 0)];
        for width in 1..40 {
            draw_files(width, &files);
        }
    }

    /// Any table drawn into a real buffer, one string per screen row.
    fn draw_table(width: u16, height: u16, table: Table<'static>) -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
        t.draw(|f| {
            let area = f.area();
            f.render_widget(table.block(panel()), area);
        })
        .unwrap();
        let b = t.backend().buffer();
        (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect()).collect()
    }

    fn commit(short: &str, subject: &str, author: &str, added: u64) -> Commit {
        Commit {
            hash: format!("{short}000"),
            short: short.into(),
            author: author.into(),
            rel_date: "10 minutes ago".into(),
            date: "2026-11-04".into(),
            merge: false,
            subject: subject.into(),
            files: vec![stat("src/main.rs", added, 2)],
        }
    }

    /// Commits in a straight line, plus a connector row like merges have.
    fn log_of(commits: Vec<Commit>) -> Log {
        let mut rows: Vec<GraphRow> =
            (0..commits.len()).map(|i| GraphRow { graph: "*".into(), commit: Some(i) }).collect();
        rows.push(GraphRow { graph: "|\\".into(), commit: None });
        Log { commits, rows }
    }

    const LONG_SUBJECT: &str = "Fix the narrow layout of a list that goes on and on";

    fn sample_log() -> Log {
        log_of(vec![
            commit("f512c0e", LONG_SUBJECT, "Zhimin Chen", 1234),
            commit("df86c88", "Short one", "claude[bot]", 5),
        ])
    }

    /// What the columns other than the commit column take, gaps included.
    fn beside(widths: &[usize]) -> usize {
        widths[1..].iter().filter(|w| **w > 0).map(|w| w + 1).sum()
    }

    #[test]
    fn commit_list_keeps_its_full_layout_while_the_subject_has_room() {
        // Graph and space (3, the connector row is wider), hash and space (8),
        // 24 of subject, then 47 of columns and 5 gaps: 87 inside the borders.
        let log = sample_log();
        for width in [89, 100, 200] {
            let cols = log_cols(width, &log);
            assert_eq!(cols.hash, 8, "width {width}");
            assert_eq!(cols.widths, [width as usize - 54, 14, 14, 5, 7, 7], "width {width}");
        }
        assert_eq!(log_cols(88, &log).widths[3], 3);
    }

    #[test]
    fn commit_list_gives_up_padding_then_author_date_and_hash() {
        // A column stays while the commit column keeps 35 cells: graph and
        // space (3), hash and space (8), 24 of subject.
        let log = sample_log();
        // The padding goes first: author and date fit their longest value.
        assert_eq!(log_cols(88, &log).widths, [45, 11, 14, 3, 5, 3]);
        // Then the author.
        assert_eq!(log_cols(78, &log).widths, [35, 11, 14, 3, 5, 3]);
        assert_eq!(log_cols(77, &log).widths, [46, 0, 14, 3, 5, 3]);
        // Then the date.
        assert_eq!(log_cols(66, &log).widths, [35, 0, 14, 3, 5, 3]);
        assert_eq!(log_cols(65, &log).widths, [49, 0, 0, 3, 5, 3]);
        // And last the hash.
        let cols = log_cols(51, &log);
        assert_eq!((cols.hash, cols.widths), (8, [35, 0, 0, 3, 5, 3]));
        let cols = log_cols(50, &log);
        assert_eq!((cols.hash, cols.widths), (0, [34, 0, 0, 3, 5, 3]));
    }

    #[test]
    fn commit_list_columns_add_up_to_the_panel_at_every_width() {
        let log = sample_log();
        for width in 20..=200 {
            let cols = log_cols(width, &log);
            assert_eq!(cols.widths[0] + beside(&cols.widths), width as usize - 2, "width {width}");
        }
    }

    #[test]
    fn short_subjects_keep_the_columns_longer() {
        let log = log_of(vec![commit("f512c0e", "Fix", "Zhimin Chen", 5)]);
        // Graph and space (3), hash and space (8), subject (3): the author stays
        // down to where the commit column holds just that.
        assert_eq!(log_cols(55, &log).widths, [14, 11, 14, 3, 3, 3]);
        assert_eq!(log_cols(54, &log).widths[1], 0);
    }

    #[test]
    fn counts_are_never_clipped_in_the_commit_list() {
        let log = log_of(vec![commit("f512c0e", LONG_SUBJECT, "Zhimin Chen", 123_456_789)]);
        // Wider than the roomy 7 cells, at any width.
        assert_eq!(log_cols(200, &log).widths[4], 10);
        assert_eq!(log_cols(50, &log).widths[4], 10);
    }

    #[test]
    fn merge_fits_the_files_column_of_a_narrow_commit_list() {
        let mut merge = commit("f512c0e", "Merge branch 'x'", "Zhimin Chen", 0);
        merge.merge = true;
        merge.files.clear();
        assert_eq!(log_cols(40, &log_of(vec![merge])).widths[3], 5);
    }

    #[test]
    fn commit_list_shows_subject_and_counts_at_every_width() {
        let log = sample_log();
        // 33 is where the 14 cells of subject checked here still fit.
        for width in 33..=130u16 {
            let screen = draw_table(width, 5, log_table(&log, width));
            assert!(screen[1].contains("Fix the narrow"), "width {width}: {}", screen[1]);
            assert!(screen[1].contains("1f"), "width {width}: {}", screen[1]);
            assert!(screen[1].contains("+1234") && screen[1].contains("-2"), "width {width}: {}", screen[1]);
            assert!(screen[2].contains("Short one"), "width {width}: {}", screen[2]);
            // Columns are hidden whole, in this order. None is left half-drawn.
            assert_eq!(screen[1].contains("f512c0e"), width >= 51, "width {width}: {}", screen[1]);
            assert_eq!(screen[1].contains("10 minutes ago"), width >= 66, "width {width}: {}", screen[1]);
            assert_eq!(screen[1].contains("Zhimin Chen"), width >= 78, "width {width}: {}", screen[1]);
        }
    }

    #[test]
    fn tiny_commit_lists_do_not_panic() {
        for width in 1..30 {
            draw_table(width, 5, log_table(&sample_log(), width));
            draw_table(width, 5, log_table(&Log::default(), width));
        }
    }

    fn branch(name: &str, ahead: u64, behind: u64, added: u64) -> Branch {
        Branch {
            name: name.into(),
            tip: "abc".into(),
            current: false,
            rel_date: "3 days ago".into(),
            worktree: None,
            cmp: Some(Ok(BranchCmp {
                merge_base: "def".into(),
                ahead,
                behind,
                files: vec![stat("src/a.rs", added, 2)],
            })),
        }
    }

    fn sample_branches() -> Vec<Branch> {
        vec![
            branch("agent/fix-panes", 2, 0, 1234),
            Branch { current: true, cmp: None, ..branch("main", 0, 0, 0) },
        ]
    }

    #[test]
    fn branch_list_keeps_its_full_layout_while_names_and_a_whole_bar_fit() {
        // The longest name takes 18 cells with its marker. The fixed columns and
        // their gaps take 59, and the bar 25 and a gap: 103 inside the borders.
        let list = sample_branches();
        for width in [105, 120, 200] {
            let cols = branch_cols(width, &list);
            assert!(cols.words, "width {width}");
            assert_eq!(cols.widths, [width as usize - 87, 9, 11, 5, 7, 7, 25, 14], "width {width}");
        }
        assert_eq!(branch_cols(104, &list).widths[1], 7);
    }

    #[test]
    fn branch_list_gives_up_padding_bar_words_and_date_before_names() {
        let list = sample_branches();
        // Padding first, and the bar is still whole. Date: `3 days ago`.
        let cols = branch_cols(104, &list);
        assert!(cols.words);
        assert_eq!(cols.widths, [104 - 2 - 7 - 8 - 3 - 5 - 3 - 25 - 10 - 7, 7, 8, 3, 5, 3, 25, 10]);
        // Then the bar shrinks, and the name column ends up with just the name.
        let cols = branch_cols(70, &list);
        assert!(cols.words);
        assert_eq!(cols.widths, [18, 7, 8, 3, 5, 3, 7, 10]);
        let cols = branch_cols(62, &list);
        assert!(cols.words);
        assert_eq!(cols.widths, [18, 7, 8, 3, 5, 3, 0, 10]);
        // The words go next, leaving `↑2` and `↓0`, and the bar gets what they gave.
        let cols = branch_cols(61, &list);
        assert!(!cols.words);
        assert_eq!(cols.widths, [18, 2, 2, 3, 5, 3, 9, 10]);
        let cols = branch_cols(51, &list);
        assert!(!cols.words);
        assert_eq!(cols.widths, [18, 2, 2, 3, 5, 3, 0, 10]);
        // Then the date, and the bar again.
        assert_eq!(branch_cols(50, &list).widths, [18, 2, 2, 3, 5, 3, 9, 0]);
        assert_eq!(branch_cols(40, &list).widths, [18, 2, 2, 3, 5, 3, 0, 0]);
        // Nothing is left to give, so from here on the name is shortened...
        assert_eq!(branch_cols(39, &list).widths, [17, 2, 2, 3, 5, 3, 0, 0]);
        assert_eq!(branch_cols(38, &list).widths, [16, 2, 2, 3, 5, 3, 0, 0]);
        // ...until it would be too short to read. Only `+` and `-` stay then, and
        // the room goes to the name, not to a bar without the counts around it.
        assert_eq!(branch_cols(37, &list).widths, [25, 0, 0, 0, 5, 3, 0, 0]);
    }

    #[test]
    fn branch_bar_appears_only_when_it_can_draw_something() {
        // The name takes 18. A bar takes its gap and its margin on top.
        let list = sample_branches();
        assert_eq!(branch_cols(64, &list).widths[..7], [20, 7, 8, 3, 5, 3, 0]);
        assert_eq!(branch_cols(65, &list).widths[..7], [18, 7, 8, 3, 5, 3, 2]);
    }

    #[test]
    fn branch_list_columns_add_up_to_the_panel_at_every_width() {
        let list = sample_branches();
        for width in 30..=200 {
            let cols = branch_cols(width, &list);
            assert_eq!(cols.widths[0] + beside(&cols.widths), width as usize - 2, "width {width}");
        }
    }

    #[test]
    fn branch_tags_go_before_the_name() {
        let wt = Branch { worktree: Some("/tmp/wt".into()), ..branch("agent/feature", 1, 0, 5) };
        // Marker (3), name (13), tag (9).
        assert_eq!(branch_name(&wt, 25).to_string(), "   agent/feature worktree");
        assert_eq!(branch_name(&wt, 24).to_string(), "   agent/feature");
        assert_eq!(branch_name(&wt, 16).to_string(), "   agent/feature");
        assert_eq!(branch_name(&wt, 15).to_string(), "   …/feature");
        // The widest row with its tag takes 28 cells, and without it 16. Only the
        // 16 can hide a column: here the date stays although no tag fits.
        let list = vec![wt, Branch { cmp: Some(Err("x".into())), ..branch("orphan", 0, 0, 0) }];
        let cols = branch_cols(47, &list);
        assert_eq!((cols.widths[0], cols.widths[7]), (16, 10));
        // A bar only gets what the tags leave. 42 of the 72 go to the other columns.
        assert_eq!(branch_cols(100, &list).widths[6], 25);
        assert_eq!(branch_cols(72, &list).widths[6], 0);
        assert_eq!(branch_cols(73, &list).widths[6], 2);
    }

    /// 59 cells of name, 62 with the marker.
    const LONG_BRANCH: &str = "agent/some-really-long-branch-name-that-goes-on-for-a-while";

    #[test]
    fn long_branch_name_wins_over_the_bar() {
        let list = vec![branch(LONG_BRANCH, 1, 0, 5)];
        // The bar yields all it has, and the name is shortened only after that.
        let cols = branch_cols(100, &list);
        assert_eq!((cols.widths[0], cols.widths[6]), (58, 0));
        let screen = draw_table(100, 3, branch_table(&list, 100));
        assert!(screen[1].contains("…/some-really-long"), "{}", screen[1]);
        assert!(!screen[1].contains('━'), "{}", screen[1]);
        // A bar shows up only when the whole name has fit.
        let cols = branch_cols(105, &list);
        assert_eq!((cols.widths[0], cols.widths[6]), (63, 0));
        let cols = branch_cols(107, &list);
        assert_eq!((cols.widths[0], cols.widths[6]), (62, 2));
    }

    #[test]
    fn long_branch_name_takes_only_so_much_from_words_and_date() {
        let list = vec![branch(LONG_BRANCH, 1, 0, 5)];
        // Words and date stay as soon as 40 cells are left for it.
        let cols = branch_cols(82, &list);
        assert!(cols.words);
        assert_eq!(cols.widths[0], NAME_GOAL);
        let cols = branch_cols(81, &list);
        assert!(!cols.words);
        assert_eq!(cols.widths[7], 10);
        let cols = branch_cols(71, &list);
        assert_eq!((cols.widths[0], cols.widths[7]), (NAME_GOAL, 10));
        assert_eq!(branch_cols(70, &list).widths[7], 0);
    }

    #[test]
    fn names_never_get_less_room_than_the_fixed_columns_gave_them() {
        // Those took 60 cells with their gaps, and the bar what names with their
        // tags left. Narrow terminals must lose the bar, not the names.
        let wt = Branch { worktree: Some("/tmp/wt".into()), ..branch("agent/feature", 1, 0, 5) };
        for list in [vec![branch(LONG_BRANCH, 1, 0, 5)], vec![wt], sample_branches()] {
            let full = list.iter().map(|b| branch_name(b, usize::MAX).width()).max().unwrap();
            for width in 20..=250 {
                let room = (width as usize).saturating_sub(2 + 60);
                let old = room - room.saturating_sub(full).min(BAR + 1);
                let name = branch_cols(width, &list).widths[0];
                assert!(name >= old, "width {width}: {name} < {old} for {}", list[0].name);
            }
        }
    }

    #[test]
    fn counts_are_never_clipped_in_the_branch_list() {
        let list = vec![branch("data/huge", 123, 45678, 234_567_890)];
        let cols = branch_cols(200, &list);
        assert_eq!(cols.widths[1..6], [9, 12, 5, 10, 7]);
        let cols = branch_cols(50, &list);
        assert!(!cols.words);
        assert_eq!(cols.widths[1..6], [4, 6, 3, 10, 3]);
        assert_eq!(branch_cols(30, &list).widths[4..6], [10, 3]);
    }

    #[test]
    fn branch_list_shows_names_and_counts_at_every_width() {
        let mut list = sample_branches();
        list.push(Branch { cmp: Some(Err("no merge base".into())), ..branch("orphan-history", 0, 0, 0) });
        for width in 30..=130u16 {
            let screen = draw_table(width, 5, branch_table(&list, width));
            let row = &screen[1];
            // A name loses its start, never its end.
            assert!(row.contains("fix-panes"), "width {width}: {row}");
            assert!(screen[3].contains("history"), "width {width}: {}", screen[3]);
            assert!(screen[2].contains("* main"), "width {width}: {}", screen[2]);
            assert!(row.contains("+1234") && row.contains("-2"), "width {width}: {row}");
            // The other counts stay as long as the name stays readable.
            assert_eq!(row.contains("1f"), width >= 38, "width {width}: {row}");
            assert_eq!(row.contains("2 ahead") && row.contains("0 behind"), width >= 62, "width {width}: {row}");
            assert_eq!(row.contains("↑2") && row.contains("↓0"), (38..62).contains(&width), "width {width}: {row}");
            // Columns are hidden whole. None is left half-drawn.
            assert_eq!(row.contains("3 days ago"), width >= 51, "width {width}: {row}");
        }
    }

    #[test]
    fn tiny_branch_lists_do_not_panic() {
        for width in 1..30 {
            draw_table(width, 4, branch_table(&sample_branches(), width));
            draw_table(width, 4, branch_table(&[], width));
        }
    }
}

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
use ratatui::widgets::{Block, BorderType, Cell, Padding, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};

use git::{Branch, BranchCmp, Branches, Bucket, Commit, FileStat, GraphRow, Log, Repo, Stats, WorkTree, totals};

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
/// Top bar: cells between the repo and branch names.
const TOP_GAP: usize = 2;
/// A name cut shorter than this says too little.
const NAME_MIN: usize = 6;
/// Stats: what the key column takes at least while there is room to spare, for
/// days and for names (directories and authors).
const DAY_KEY: usize = 11;
const NAME_KEY: usize = 18;
/// Stats: commit column while there is room to spare.
const COUNT_W: usize = 5;
/// Stats: cells of bar worth keeping the padding of the number columns for.
const BUCKET_BAR: usize = 4;
const RANGES: [(&str, Option<u32>); 4] =
    [("7d", Some(7)), ("30d", Some(30)), ("90d", Some(90)), ("all", None)];
// New tabs go at the end so existing number keys keep their meaning.
const TABS: [&str; 4] = ["Changes", "Log", "Stats", "Branches"];
/// The key hints of each tab, in the order a narrow bar keeps them.
const HINTS: [&[&str]; 4] = [
    &["j/k move", "Enter diff", "r refresh", "1-4 tabs", "q quit"],
    &["j/k move", "Enter show commit", "r refresh", "1-4 tabs", "q quit"],
    &["t range", "j/k scroll", "r refresh", "1-4 tabs", "q quit"],
    &["j/k move", "Enter branch diff", "r refresh", "1-4 tabs", "q quit"],
];

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

/// A message in place of a list. It wraps, so that a narrow pane does not lose its
/// end, and the padding keeps every line one cell from the edge, like a list row.
fn message(text: impl Into<String>, color: Color, block: Block<'static>) -> Paragraph<'static> {
    Paragraph::new(text.into()).fg(color).wrap(Wrap { trim: true }).block(block.padding(Padding::horizontal(1)))
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

/// Repo and branch name in `room` cells. Each keeps what it needs up to half of
/// the room and the other takes the rest. With too little room for two names only
/// the branch stays, because it is the one that changes, and a stub says nothing.
fn fit_names(room: usize, repo: &str, branch: &str) -> (String, String) {
    let (r, b) = (cells(repo), cells(branch));
    if r + TOP_GAP + b <= room {
        return (repo.to_string(), branch.to_string());
    }
    if room < TOP_GAP + 2 * NAME_MIN {
        let keep = b <= room || room >= NAME_MIN;
        return (String::new(), if keep { shorten_path(branch, room) } else { String::new() });
    }
    let room = room - TOP_GAP;
    let r = r.min(room - b.min(room / 2));
    (clip(repo, r), shorten_path(branch, room - r))
}

/// The line in the top bar of a box `area_w` wide. All four tabs always fit, so the
/// active one can always be seen. The badge goes first, because it says the least.
/// Then the tabs show only their numbers: those are the keys, nothing moves when the
/// active tab changes, and the panel below names it. Then the names get shorter.
fn top_line(area_w: u16, repo: &str, branch: &str, tab: usize) -> Line<'static> {
    const BADGE: &str = " diffdash ";
    let label = |i: usize, words: bool| {
        if words { format!(" {} {} ", i + 1, TABS[i]) } else { format!(" {} ", i + 1) }
    };
    let inner = (area_w as usize).saturating_sub(2);
    // Beside the names: the leading space, the badge with its caps and space, the
    // gap before the tabs, and the tabs with the caps of a pill or the spaces of a slot.
    let room = |badge: bool, words: bool| {
        let badge = if badge { cells(BADGE) + 3 } else { 0 };
        let tabs: usize = (0..TABS.len()).map(|i| cells(&label(i, words)) + 2).sum();
        inner.saturating_sub(1 + badge + 3 + tabs)
    };
    let whole = cells(repo) + TOP_GAP + cells(branch);
    let badge = whole <= room(true, true);
    let words = whole <= room(false, true);
    let (repo, branch) = fit_names(room(badge, words), repo, branch);

    // Leading space so the pill cap does not touch the border.
    let mut spans = vec![Span::raw(" ")];
    if badge {
        spans.extend(pill(BADGE, Some(ACCENT)));
        spans.push(Span::raw(" "));
    }
    let gap = if repo.is_empty() { "" } else { "  " };
    spans.push(Span::styled(repo, Style::new().bold()));
    if !branch.is_empty() {
        spans.push(Span::styled(format!("{gap}{branch}   "), Style::new().fg(Color::Magenta)));
    }
    for i in 0..TABS.len() {
        if i == tab {
            spans.extend(pill(label(i, words), None));
        } else {
            spans.extend(pill_slot(label(i, words), Style::new().fg(DIM)));
        }
    }
    Line::from(spans)
}

/// The line in the bottom bar of a box `area_w` wide. What is going on comes before
/// what could be done: key hints go whole, from the end, until `loading…` and the
/// error fit. Then `loading…` goes, and then the end of the error, which says the
/// least: git's own words come first.
fn bottom_line(area_w: u16, tab: usize, loading: bool, error: Option<&str>) -> Line<'static> {
    const LOADING: &str = "loading…";
    let inner = (area_w as usize).saturating_sub(2);
    // The bar has one row, and git's errors can have several lines.
    let error = error.unwrap_or("").replace('\n', " ");
    let wanted = if loading { LOADING } else { "" };
    let status: usize = [wanted, error.as_str()].iter().filter(|s| !s.is_empty()).map(|s| 2 + cells(s)).sum();
    let room = inner.saturating_sub(1 + status);
    let loading = if error.is_empty() || 1 + cells(wanted) + 2 + cells(&error) <= inner { wanted } else { "" };
    let mut hints = String::new();
    for hint in HINTS[tab] {
        let next = if hints.is_empty() { hint.to_string() } else { format!("{hints} · {hint}") };
        if cells(&next) > room {
            break;
        }
        hints = next;
    }
    let error = clip(&error, inner.saturating_sub(1));
    let mut spans = Vec::new();
    for (text, color) in [(hints, DIM), (loading.to_string(), ACCENT), (error, DEL)] {
        if !text.is_empty() {
            spans.push(Span::raw(if spans.is_empty() { " " } else { "  " }));
            spans.push(Span::styled(text, Style::new().fg(color)));
        }
    }
    Line::from(spans)
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
            // No box here, only the padding.
            Some(e) => message(e.clone(), DEL, Block::new()),
            None => message("loading…", DIM, Block::new()),
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
        let line = top_line(area.width, &repo_name(&self.repo.root), branch, self.tab);
        f.render_widget(Paragraph::new(line).block(panel()), area);
    }

    fn render_bottom(&self, f: &mut Frame, area: Rect) {
        // The 2 s working-tree poll is not shown; it would flicker constantly.
        let loading = self.busy[LOG] || self.busy[STATS] || self.busy[BRANCHES];
        let error = self.errors.iter().flatten().next().map(String::as_str);
        f.render_widget(Paragraph::new(bottom_line(area.width, self.tab, loading, error)).block(panel()), area);
    }

    fn render_changes(&mut self, f: &mut Frame, area: Rect) {
        let Some(w) = &self.work else {
            f.render_widget(self.placeholder(WORK), area);
            return;
        };
        let max = w.staged.iter().chain(&w.unstaged).chain(&w.untracked).map(churn).max().unwrap_or(0);
        let (n, ta, td) = w.headline();
        let block = panel().title(changes_title(n, ta, td, area.width));
        if self.work_rows.is_empty() {
            f.render_widget(message("working tree clean", DIM, block), area);
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
        let title = Line::from(format!(" Log · latest {} commits ", log.commits.len()));
        let title = first_fit(top.width, vec![title, Line::from(" Log ")]);
        let table = log_table(log, top.width).block(panel().title(title));
        f.render_stateful_widget(table, top, &mut self.log_state);

        let block = panel();
        let Some(c) = self.selected_commit() else {
            f.render_widget(block, bottom);
            return;
        };
        let block = block.title(commit_title(c, bottom.width));
        if c.merge {
            let msg = "merge commit: git log reports no per-file stats for merges (Enter shows the commit)";
            f.render_widget(message(msg, DIM, block), bottom);
            return;
        }
        render_files(f, bottom, block, &c.files);
    }

    fn render_stats(&mut self, f: &mut Frame, area: Rect) {
        let Some(s) = &self.stats else {
            f.render_widget(self.placeholder(STATS), area);
            return;
        };
        draw_stats(f, area, s, self.range, &mut self.stats_state);
    }

    fn render_branches(&mut self, f: &mut Frame, area: Rect) {
        let Some(br) = &self.branches else {
            f.render_widget(self.placeholder(BRANCHES), area);
            return;
        };
        let title = branches_title(br.list.len(), &br.head_name, area.width);
        if br.list.is_empty() {
            f.render_widget(message("no branches yet", DIM, panel().title(title)), area);
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
            block = block.title_bottom(Line::from(worktree_note(&tilde(path), bottom.width)).fg(DIM));
        }
        // The list above keeps the end of a long name, so this keeps its start.
        let room = (bottom.width as usize).saturating_sub(4);
        let name = Span::styled(format!(" {} ", clip(&b.name, room)), Style::new().fg(Color::Magenta).bold());
        let c = match &b.cmp {
            Some(Ok(c)) => c,
            other => {
                let (title, text, color) = match other {
                    _ if b.current => (
                        "· current branch ",
                        "the other branches are compared with this one, from where they forked".to_string(),
                        DIM,
                    ),
                    Some(Err(e)) => ("", e.clone(), DEL),
                    _ => ("", "HEAD has no commits yet, so there is nothing to compare with".to_string(), DIM),
                };
                let title = vec![Line::from(vec![name.clone(), Span::raw(title)]), Line::from(name)];
                let block = block.title(first_fit(bottom.width, title));
                f.render_widget(message(text, color, block), bottom);
                return;
            }
        };
        let block = block.title(branch_title(name, c, bottom.width));
        if c.files.is_empty() {
            let msg = if c.ahead == 0 {
                format!("every commit on this branch is already in {}", br.head_name)
            } else {
                "no file changes since the merge base".to_string()
            };
            f.render_widget(message(msg, DIM, block), bottom);
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

/// The first of these lines that fits in a panel `area_w` wide. A line the panel
/// clips can end in the middle of a number, and `net +12` reads like a whole count.
fn first_fit(area_w: u16, mut lines: Vec<Line<'static>>) -> Line<'static> {
    let inner = (area_w as usize).saturating_sub(2);
    let i = lines.iter().position(|l| l.width() <= inner).unwrap_or(lines.len() - 1);
    lines.swap_remove(i)
}

/// `+added -deleted ` in a panel title.
fn title_totals(added: u64, deleted: u64) -> Vec<Span<'static>> {
    vec![
        Span::styled(plus(added), Style::new().fg(ADD).bold()),
        Span::raw(" "),
        Span::styled(minus(deleted), Style::new().fg(DEL).bold()),
        Span::raw(" "),
    ]
}

/// The title of the Changes panel. The counts are its headline, so they go last.
fn changes_title(n: usize, added: u64, deleted: u64, area_w: u16) -> Line<'static> {
    let with_totals = |lead: String| Line::from([vec![Span::raw(lead)], title_totals(added, deleted)].concat());
    first_fit(
        area_w,
        vec![
            with_totals(format!(" Changes since HEAD · {n} files  ")),
            with_totals(format!(" Changes · {n} files  ")),
            with_totals(" Changes  ".to_string()),
            Line::from(" Changes "),
        ],
    )
}

/// The title of the pane below the commit list. It is where hash, author and date
/// stay visible when the list hides them, so the counts go first: the selected row
/// shows them too. A long author is cut before a date is.
fn commit_title(c: &Commit, area_w: u16) -> Line<'static> {
    let (a, d) = totals(&c.files);
    let hash = Span::styled(format!(" {} ", c.short), Style::new().fg(Color::Yellow));
    let details = Span::raw(format!("{} · {} · {} files  ", c.author, c.date, c.files.len()));
    let full = [vec![hash.clone(), details], title_totals(a, d)].concat();
    let date = format!(" · {} ", c.date);
    let room = (area_w as usize).saturating_sub(2 + hash.width() + cells(&date));
    let dated = if room >= NAME_MIN.min(cells(&c.author)) {
        format!("{}{date}", clip(&c.author, room))
    } else {
        format!("{} ", c.date)
    };
    first_fit(area_w, vec![Line::from(full), Line::from(vec![hash.clone(), Span::raw(dated)]), Line::from(hash)])
}

/// The title of the Branches list. The other branch names are in the list, and the
/// one everything is compared with goes last.
fn branches_title(n: usize, head: &str, area_w: u16) -> Line<'static> {
    first_fit(
        area_w,
        vec![
            Line::from(format!(" Branches · {n} local · compared with {head} ")),
            Line::from(format!(" Branches · compared with {head} ")),
            Line::from(format!(" Branches · {n} local ")),
            Line::from(" Branches "),
        ],
    )
}

/// The title of a branch's pane, after its `name`. The merge base goes first, as
/// nothing else depends on it, then files and lines, which the list shows too.
fn branch_title(name: Span<'static>, c: &BranchCmp, area_w: u16) -> Line<'static> {
    let (a, d) = totals(&c.files);
    let counts = Span::raw(format!("· {} ahead, {} behind ", c.ahead, c.behind));
    let counts = vec![name.clone(), counts];
    let base = Span::raw(format!("· merge base {} ", &c.merge_base[..c.merge_base.len().min(7)]));
    let files = [vec![Span::raw(format!("· {} files  ", c.files.len()))], title_totals(a, d)].concat();
    first_fit(
        area_w,
        vec![
            Line::from([counts.clone(), vec![base], files.clone()].concat()),
            Line::from([counts.clone(), files].concat()),
            Line::from(counts),
            Line::from(name),
        ],
    )
}

/// What the bottom title of a branch's pane says about its worktree. The note comes
/// first, and the path gets what is left, cut from the left like any path.
fn worktree_note(path: &str, area_w: u16) -> String {
    const NOTE: &str = " uncommitted changes in its worktree are not counted";
    let inner = (area_w as usize).saturating_sub(2);
    // Beside the path: the note, its separator and the closing space.
    let room = inner.saturating_sub(cells(NOTE) + cells(" · ") + 1);
    if room >= NAME_MIN.min(cells(path)) {
        format!("{NOTE} · {} ", shorten_path(path, room))
    } else if cells(NOTE) < inner {
        format!("{NOTE} ")
    } else {
        " uncommitted changes not counted ".to_string()
    }
}

/// The box at the top of the Stats tab. Everything in it goes whole: the words
/// before the range pills, then the note, then the net count.
fn stats_head(s: &Stats, range: usize, area_w: u16) -> Paragraph<'static> {
    let mut pills = Vec::new();
    for (i, (n, _)) in RANGES.iter().enumerate() {
        let label = format!(" {n} ");
        if i == range {
            pills.extend(pill(label, Some(ACCENT)));
        } else {
            pills.extend(pill_slot(label, Style::new().fg(DIM)));
        }
    }
    pills.push(Span::raw(" "));
    let title = |lead: &'static str| Line::from([vec![Span::raw(lead)], pills.clone()].concat());
    let title = first_fit(area_w, vec![title(" Stats · range "), title(" Stats "), title(" ")]);

    let totals = vec![
        Span::raw(" "),
        Span::styled(format!("{} commits", s.commits), Style::new().bold()),
        Span::raw("   "),
        Span::styled(plus(s.added), Style::new().fg(ADD).bold()),
        Span::raw("  "),
        Span::styled(minus(s.deleted), Style::new().fg(DEL).bold()),
    ];
    let net = s.added as i64 - s.deleted as i64;
    let net = vec![Span::raw("   net "), Span::styled(format!("{net:+}"), Style::new().bold())];
    let note = Span::styled(format!("   ({}, merges excluded)", RANGES[range].0), Style::new().fg(DIM));
    let with_net = [totals.clone(), net].concat();
    let summary = first_fit(
        area_w,
        vec![Line::from([with_net.clone(), vec![note]].concat()), Line::from(with_net), Line::from(totals)],
    );
    Paragraph::new(summary).block(panel().title(title))
}

fn draw_stats(f: &mut Frame, area: Rect, s: &Stats, range: usize, state: &mut TableState) {
    let [head, body] = Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).areas(area);
    f.render_widget(stats_head(s, range, area.width), head);
    let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Fill(1)]).areas(body);
    let [rtop, rbot] = Layout::vertical([Constraint::Percentage(55), Constraint::Fill(1)]).areas(right);
    let boxed = |t: &'static str| panel().title(t);
    let days = bucket_table(&s.by_day, left.width, DAY_KEY, fit_day).block(boxed(" By day "));
    f.render_stateful_widget(days, left, state);
    let dirs = bucket_table(&s.by_dir, rtop.width, NAME_KEY, clip);
    let title = first_fit(rtop.width, vec![Line::from(" By top-level directory "), Line::from(" By directory ")]);
    f.render_widget(dirs.block(panel().title(title)), rtop);
    let authors = bucket_table(&s.by_author, rbot.width, NAME_KEY, clip);
    f.render_widget(authors.block(boxed(" By author ")), rbot);
}

/// A day that does not fit loses its year first: `11-01`.
fn fit_day(day: &str, width: usize) -> String {
    if cells(day) <= width {
        return day.to_string();
    }
    clip(day.get(5..).unwrap_or(day), width)
}

/// `s` in at most `width` cells, cut on the right. The start of an author or a
/// top-level directory says the most.
fn clip(s: &str, width: usize) -> String {
    if cells(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut used = cells("…");
    let mut end = 0;
    for (i, c) in s.char_indices() {
        used += cells(c.encode_utf8(&mut [0; 4]));
        if used > width {
            break;
        }
        end = i + c.len_utf8();
    }
    format!("{}…", &s[..end])
}

fn commits(b: &Bucket) -> String {
    format!("{}c", b.commits)
}

/// Column widths of a Stats table: key with its leading space, commits, added,
/// deleted, and the bar with one cell of margin. A narrow pane loses the padding
/// of the number columns, then the bar, then the commit count, and only then key
/// text. `roomy_key` keeps the key column of a roomy table from moving with its
/// data, and a key longer than that takes what it needs from the bar.
fn bucket_cols(area_w: u16, list: &[Bucket], roomy_key: usize) -> [usize; 5] {
    let (mut key, mut count, mut add, mut del) = (0, 0, 0, 0);
    for b in list {
        key = key.max(1 + cells(&b.key));
        count = count.max(commits(b).len());
        add = add.max(plus(b.added).len());
        del = del.max(minus(b.deleted).len());
    }
    let key = key.min(NAME_GOAL);
    let inner = (area_w as usize).saturating_sub(2);
    let roomy = [key.max(roomy_key), count.max(COUNT_W), add.max(NUM_W), del.max(NUM_W)];
    let [key, count, add, del] = if room_beside(inner, &roomy) > BUCKET_BAR {
        roomy
    } else if room_beside(inner, &[count, add, del]) >= key.min(DAY_KEY) {
        [key, count, add, del]
    } else {
        // Room for a full date is worth more than the commit count.
        [key, 0, add, del]
    };
    let beside = room_beside(inner, &[count, add, del]);
    let key = key.min(beside);
    // A bar never shows in place of the commit count, and its margin needs a bar
    // to follow.
    let bar = room_beside(beside, &[key]);
    [key, count, add, del, if count > 0 && bar > 1 { bar } else { 0 }]
}

/// `fit` puts a key in the room of its column.
fn bucket_table(list: &[Bucket], area_w: u16, roomy_key: usize, fit: fn(&str, usize) -> String) -> Table<'static> {
    let widths = bucket_cols(area_w, list, roomy_key);
    let [key, _, _, _, bar] = widths;
    let max = list.iter().map(|b| b.added + b.deleted).max().unwrap_or(0);
    let rows: Vec<Row> = list
        .iter()
        .map(|b| {
            Row::new(shown(
                &widths,
                [
                    Cell::from(format!(" {}", fit(&b.key, key.saturating_sub(1)))),
                    Cell::from(Line::from(commits(b)).right_aligned().fg(DIM)),
                    Cell::from(Line::from(plus(b.added)).right_aligned().fg(ADD)),
                    Cell::from(Line::from(minus(b.deleted)).right_aligned().fg(DEL)),
                    Cell::from(bar_line(b.added, b.deleted, max, bar.saturating_sub(1))),
                ],
            ))
        })
        .collect();
    shown_table(rows, &widths)
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

    fn bucket(key: &str, commits: u64, added: u64, deleted: u64) -> Bucket {
        Bucket { key: key.into(), added, deleted, commits }
    }

    const LONG_AUTHOR: &str = "Christopher Montgomery-Smithson";

    /// Holds a huge count, so a narrow pane cannot make room by tightening alone.
    fn sample_days() -> Vec<Bucket> {
        vec![bucket("2026-11-01", 1, 3, 0), bucket("2026-09-01", 12, 123456, 0), bucket("2024-03-01", 1, 50, 7)]
    }

    fn sample_authors() -> Vec<Bucket> {
        vec![bucket(LONG_AUTHOR, 1, 123456, 0), bucket("Zhimin Chen", 12, 58, 7)]
    }

    fn short_days() -> Vec<Bucket> {
        vec![bucket("2026-11-01", 1, 3, 0), bucket("2026-10-20", 1, 7, 0)]
    }

    fn short_names() -> Vec<Bucket> {
        vec![bucket("src/", 1, 7, 0), bucket("(root)", 1, 3, 0)]
    }

    fn sample_stats() -> Stats {
        Stats {
            commits: 14,
            added: 123509,
            deleted: 7,
            by_day: sample_days(),
            by_dir: short_names(),
            by_author: sample_authors(),
        }
    }

    fn rows_of(t: &Terminal<TestBackend>) -> Vec<String> {
        let b = t.backend().buffer();
        (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect()).collect()
    }

    fn draw_head(width: u16, s: &Stats, range: usize) -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(width, 3)).unwrap();
        t.draw(|f| {
            let area = f.area();
            f.render_widget(stats_head(s, range, width), area);
        })
        .unwrap();
        rows_of(&t)
    }

    #[test]
    fn stats_tables_keep_their_old_layout_while_the_bar_has_room() {
        // The old columns: key, 5, 8, 8 and the rest, with four gaps between them.
        for width in [43, 46, 60, 120] {
            let rest = width as usize - 2 - 11 - 5 - 16 - 4;
            assert_eq!(bucket_cols(width, &short_days(), DAY_KEY), [11, 5, 8, 8, rest], "width {width}");
        }
        for width in [50, 60, 120] {
            let rest = width as usize - 2 - 18 - 5 - 16 - 4;
            assert_eq!(bucket_cols(width, &short_names(), NAME_KEY), [18, 5, 8, 8, rest], "width {width}");
        }
    }

    #[test]
    fn stats_tables_give_up_padding_then_bar_then_commits_then_key() {
        let days = sample_days();
        assert_eq!(bucket_cols(43, &days, DAY_KEY), [11, 5, 8, 8, 5]);
        // The padding goes first, which makes the bar longer.
        assert_eq!(bucket_cols(42, &days, DAY_KEY), [11, 3, 7, 2, 13]);
        // The bar is gone when it would have only its margin.
        assert_eq!(bucket_cols(31, &days, DAY_KEY), [11, 3, 7, 2, 2]);
        assert_eq!(bucket_cols(30, &days, DAY_KEY), [11, 3, 7, 2, 0]);
        // The commit count stays for as long as a whole date fits beside it.
        assert_eq!(bucket_cols(28, &days, DAY_KEY), [11, 3, 7, 2, 0]);
        assert_eq!(bucket_cols(27, &days, DAY_KEY), [11, 0, 7, 2, 0]);
        // Then the key gets shorter. Added and deleted are never cut.
        assert_eq!(bucket_cols(23, &days, DAY_KEY), [10, 0, 7, 2, 0]);
        assert_eq!(bucket_cols(20, &days, DAY_KEY), [7, 0, 7, 2, 0]);
    }

    #[test]
    fn long_name_takes_room_from_the_bar_but_not_without_limit() {
        let authors = sample_authors();
        // It used to be cut at 18 cells at any width.
        assert_eq!(bucket_cols(120, &authors, NAME_KEY), [32, 5, 8, 8, 61]);
        // Padding, then bar: the whole name still fits.
        assert_eq!(bucket_cols(60, &authors, NAME_KEY), [32, 3, 7, 2, 10]);
        assert_eq!(bucket_cols(49, &authors, NAME_KEY), [32, 3, 7, 2, 0]);
        // Then the name is what gets shorter, until a whole date would not fit.
        assert_eq!(bucket_cols(36, &authors, NAME_KEY), [19, 3, 7, 2, 0]);
        assert_eq!(bucket_cols(28, &authors, NAME_KEY), [11, 3, 7, 2, 0]);
        assert_eq!(bucket_cols(27, &authors, NAME_KEY), [14, 0, 7, 2, 0]);
        // One very long name leaves the bar something.
        let huge = [bucket(&"x".repeat(60), 1, 3, 0)];
        assert_eq!(bucket_cols(120, &huge, NAME_KEY)[0], NAME_GOAL);
    }

    #[test]
    fn stats_columns_add_up_to_the_pane_at_every_width() {
        for width in 20..=200u16 {
            let inner = width as usize - 2;
            for (list, roomy) in
                [(sample_days(), DAY_KEY), (sample_authors(), NAME_KEY), (short_days(), DAY_KEY), (short_names(), NAME_KEY)]
            {
                let widths = bucket_cols(width, &list, roomy);
                let used: usize = widths.iter().filter(|w| **w > 0).map(|w| w + 1).sum::<usize>() - 1;
                assert!(used <= inner, "width {width}: {widths:?} takes {used} of {inner}");
            }
        }
    }

    #[test]
    fn days_and_counts_stay_readable_in_a_stats_table_at_every_width() {
        for width in 20..=120u16 {
            let screen = draw_table(width, 5, bucket_table(&sample_days(), width, DAY_KEY, fit_day));
            for (row, day, year, add, del) in [
                (&screen[1], "11-01", "2026-11-01", "+3", "-0"),
                (&screen[2], "09-01", "2026-09-01", "+123456", "-0"),
                (&screen[3], "03-01", "2024-03-01", "+50", "-7"),
            ] {
                // A day is never cut down to `…01`.
                assert!(row.contains(day), "width {width}: {row}");
                assert!(!row.contains('…'), "width {width}: {row}");
                assert_eq!(row.contains(year), width >= 24, "width {width}: {row}");
                assert!(row.contains(add) && row.contains(del), "width {width}: {row}");
            }
            assert_eq!(screen[2].contains("12c"), width >= 28, "width {width}: {}", screen[2]);
        }
    }

    #[test]
    fn names_are_cut_on_the_right_in_a_stats_table() {
        for width in 20..=120u16 {
            let screen = draw_table(width, 4, bucket_table(&sample_authors(), width, NAME_KEY, clip));
            let (first, second) = (&screen[1], &screen[2]);
            assert!(first.contains("Chris"), "width {width}: {first}");
            assert!(second.contains("Zhimi"), "width {width}: {second}");
            assert_eq!(first.contains(LONG_AUTHOR), width >= 49, "width {width}: {first}");
            assert!(first.contains("+123456") && first.contains("-0"), "width {width}: {first}");
            assert!(second.contains("+58") && second.contains("-7"), "width {width}: {second}");
        }
    }

    #[test]
    fn clip_keeps_the_start_and_counts_cells() {
        assert_eq!(clip("Zhimin Chen", 11), "Zhimin Chen");
        assert_eq!(clip("Zhimin Chen", 10), "Zhimin Ch…");
        assert_eq!(clip("Zhimin Chen", 1), "…");
        assert_eq!(clip("Zhimin Chen", 0), "");
        assert_eq!(clip("陈志民", 6), "陈志民");
        assert_eq!(clip("陈志民", 5), "陈志…");
        assert_eq!(clip("陈志民", 4), "陈…");
    }

    #[test]
    fn a_day_loses_its_year_first() {
        assert_eq!(fit_day("2026-11-01", 10), "2026-11-01");
        assert_eq!(fit_day("2026-11-01", 9), "11-01");
        assert_eq!(fit_day("2026-11-01", 5), "11-01");
        assert_eq!(fit_day("2026-11-01", 4), "11-…");
    }

    #[test]
    fn stats_head_drops_things_whole_and_never_cuts_a_number() {
        let s = Stats { commits: 6, added: 123641, deleted: 12, ..Default::default() };
        for width in 31..=120u16 {
            let screen = draw_head(width, &s, 3);
            let (title, summary) = (&screen[0], &screen[1]);
            // Every range pill stays, so the active one can always be seen.
            for range in ["7d", "30d", "90d", "all"] {
                assert!(title.contains(range), "width {width}: {title}");
            }
            assert_eq!(title.contains("Stats · range"), width >= 45, "width {width}: {title}");
            assert_eq!(title.contains("Stats"), width >= 37, "width {width}: {title}");
            assert!(summary.contains("6 commits"), "width {width}: {summary}");
            assert!(summary.contains("+123641") && summary.contains("-12"), "width {width}: {summary}");
            assert_eq!(summary.contains("net"), width >= 41, "width {width}: {summary}");
            assert_eq!(summary.contains("net +123629"), width >= 41, "width {width}: {summary}");
            assert_eq!(summary.contains('('), width >= 66, "width {width}: {summary}");
            assert_eq!(summary.contains("(all, merges excluded)"), width >= 66, "width {width}: {summary}");
        }
    }

    #[test]
    fn selected_day_is_highlighted_across_its_pane() {
        let s = sample_stats();
        for width in [40u16, 50, 60, 80, 120] {
            let mut t = Terminal::new(TestBackend::new(width, 14)).unwrap();
            let mut state = TableState::default().with_selected(Some(1));
            t.draw(|f| {
                let area = f.area();
                draw_stats(f, area, &s, 1, &mut state);
            })
            .unwrap();
            let b = t.backend().buffer();
            // Below the three rows of the head and the pane's top border.
            for x in 1..width / 2 - 1 {
                assert!(b[(x, 5)].modifier.contains(Modifier::REVERSED), "width {width}, x {x}");
            }
            assert!(!b[(1, 4)].modifier.contains(Modifier::REVERSED), "width {width}");
        }
    }

    #[test]
    fn tiny_stats_do_not_panic() {
        let s = sample_stats();
        for width in 1..60u16 {
            for height in 1..12u16 {
                let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut state = TableState::default().with_selected(Some(0));
                t.draw(|f| {
                    let area = f.area();
                    draw_stats(f, area, &s, 1, &mut state);
                })
                .unwrap();
                let empty = Stats::default();
                t.draw(|f| {
                    let area = f.area();
                    draw_stats(f, area, &empty, 1, &mut state);
                })
                .unwrap();
            }
        }
    }

    /// The top bar as it was built before it had to fit.
    fn full_top(repo: &str, branch: &str, tab: usize) -> String {
        let mut text = format!(" {PILL_L} diffdash {PILL_R} {repo}  {branch}   ");
        for (i, t) in TABS.iter().enumerate() {
            let (l, r) = if i == tab { (PILL_L, PILL_R) } else { (" ", " ") };
            text += &format!("{l} {} {t} {r}", i + 1);
        }
        text
    }

    fn top_text(width: u16, repo: &str, branch: &str, tab: usize) -> String {
        top_line(width, repo, branch, tab).iter().map(|s| s.content.as_ref()).collect()
    }

    /// The row inside the top bar, and the text of its reversed cells.
    fn draw_top(width: u16, repo: &str, branch: &str, tab: usize) -> (String, String) {
        let mut t = Terminal::new(TestBackend::new(width, 3)).unwrap();
        t.draw(|f| {
            let area = f.area();
            f.render_widget(Paragraph::new(top_line(width, repo, branch, tab)).block(panel()), area);
        })
        .unwrap();
        let b = t.backend().buffer();
        let row = (0..width).map(|x| b[(x, 1)].symbol()).collect();
        let reversed = (0..width)
            .filter(|x| b[(*x, 1)].modifier.contains(Modifier::REVERSED))
            .map(|x| b[(x, 1)].symbol())
            .collect();
        (row, reversed)
    }

    #[test]
    fn top_bar_stays_as_it_was_while_everything_fits() {
        // The width the rules for this bar have always been checked at.
        for tab in 0..TABS.len() {
            assert_eq!(top_text(80, "diffdash", "main", tab), full_top("diffdash", "main", tab), "tab {tab}");
            assert_eq!(top_text(200, "repo", "agent/feature", tab), full_top("repo", "agent/feature", tab), "tab {tab}");
        }
        assert_eq!(cells(&top_text(80, "diffdash", "main", 0)), 78);
    }

    #[test]
    fn top_bar_gives_up_badge_then_tab_names_then_repo_and_branch() {
        let (repo, branch) = ("demo", "main");
        let has = |width, text: &str| top_text(width, repo, branch, 2).contains(text);
        // The badge goes when names and all the tab names would not fit beside it.
        assert!(has(76, "diffdash") && has(76, "Branches"));
        assert!(!has(75, "diffdash") && has(75, "Branches"));
        // The tab names go next.
        assert!(has(63, "Branches"));
        assert!(!has(62, "diffdash") && !has(62, "Stats") && !has(62, "Branches") && has(62, "demo  main"));
        // Then the names.
        assert!(has(36, "demo  main"));
        assert!(!has(35, "demo") && has(35, "main"));
        assert!(has(30, "main"));
        assert!(!has(29, "main"));
    }

    #[test]
    fn all_tabs_and_the_active_one_show_at_every_width() {
        let names = [("demo", "main"), ("diffdash", "narrow-panes"), ("demo", LONG_BRANCH), (&"r".repeat(70), LONG_BRANCH)];
        for (repo, branch) in names {
            for tab in 0..TABS.len() {
                for width in 23..=200u16 {
                    let (row, reversed) = draw_top(width, repo, branch, tab);
                    for number in ["1", "2", "3", "4"] {
                        assert!(row.contains(number), "width {width} tab {tab}: {row}");
                    }
                    assert!(reversed.contains(&(tab + 1).to_string()), "width {width} tab {tab}: {row}");
                    // Everything fits, so the last cap or slot is inside the border.
                    assert!(cells(&top_text(width, repo, branch, tab)) <= width as usize - 2, "width {width}: {row}");
                }
            }
        }
    }

    #[test]
    fn tabs_do_not_move_when_they_show_only_numbers() {
        let starts: Vec<usize> = (0..TABS.len())
            .map(|tab| {
                let text = top_text(40, "demo", "main", tab);
                text.chars().position(|c| c == '1').unwrap()
            })
            .collect();
        assert!(starts.windows(2).all(|w| w[0] == w[1]), "{starts:?}");
    }

    #[test]
    fn repo_and_branch_share_the_top_bar() {
        // Both whole.
        assert_eq!(fit_names(11, "demo2", "main"), ("demo2".to_string(), "main".to_string()));
        // Neither fits in half: each gets half.
        let long = "agent/fix-the-narrow-panes";
        assert_eq!(fit_names(22, "a-repository-name", long), ("a-reposit…".to_string(), "…row-panes".to_string()));
        // A short name leaves the rest to the other, in either direction.
        assert_eq!(fit_names(22, "demo", long), ("demo".to_string(), shorten_path(long, 16)));
        assert_eq!(fit_names(16, "a-long-repository", "main"), ("a-long-re…".to_string(), "main".to_string()));
        // With too little room for two names, only the branch stays.
        assert_eq!(fit_names(13, "repository", "main"), (String::new(), "main".to_string()));
        assert_eq!(fit_names(6, "repo", long), (String::new(), shorten_path(long, 6)));
        // A stub says nothing.
        assert_eq!(fit_names(5, "repo", long), (String::new(), String::new()));
        assert_eq!(fit_names(4, "repo", "main"), (String::new(), "main".to_string()));
        assert_eq!(fit_names(0, "repo", "main"), (String::new(), String::new()));
    }

    fn draw_message(width: u16, height: u16, text: &str) -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
        t.draw(|f| {
            let area = f.area();
            f.render_widget(message(text, DIM, panel()), area);
        })
        .unwrap();
        rows_of(&t)
    }

    const MERGE_NOTE: &str = "merge commit: git log reports no per-file stats for merges (Enter shows the commit)";

    #[test]
    fn message_that_fits_stays_on_one_line_one_cell_from_the_border() {
        let screen = draw_message(30, 4, "working tree clean");
        assert_eq!(screen[1], format!("│ working tree clean{}│", " ".repeat(9)));
        assert_eq!(screen[2], format!("│{}│", " ".repeat(28)));
    }

    #[test]
    fn long_message_wraps_instead_of_losing_its_end() {
        for width in 14..=100u16 {
            let screen = draw_message(width, 20, MERGE_NOTE);
            let inside: Vec<&str> = screen[1..19].iter().map(|r| &r[r.find('│').unwrap() + '│'.len_utf8()..]).collect();
            let text = inside.join(" ");
            for word in MERGE_NOTE.split(' ') {
                assert!(text.contains(word), "width {width}: {word} is missing from {inside:?}");
            }
            // Every line keeps its distance from both borders.
            for row in inside {
                assert!(row.starts_with(' '), "width {width}: {row:?}");
                assert!(row.ends_with(" │"), "width {width}: {row:?}");
            }
        }
    }

    #[test]
    fn long_error_without_spaces_and_cjk_text_wrap_too() {
        let path = "fatal:/a/very/long/path/without/any/spaces/in/it";
        let joined: String = draw_message(20, 8, path)[1..7]
            .iter()
            .map(|r| r.trim_matches(|c| c == '│' || c == ' '))
            .collect();
        assert_eq!(joined, path);
        let cjk = "这是一条很长的中文提示信息应该换行显示";
        // A wide character leaves a blank cell behind it in the buffer.
        let joined: String = draw_message(14, 8, cjk).join("").chars().filter(|c| !" │╭╮╰╯─".contains(*c)).collect();
        assert_eq!(joined, cjk);
    }

    #[test]
    fn every_line_of_a_multi_line_error_shows() {
        let screen = draw_message(40, 5, "fatal: one\nhint: two");
        assert!(screen[1].contains("fatal: one"), "{}", screen[1]);
        assert!(screen[2].contains("hint: two"), "{}", screen[2]);
    }

    #[test]
    fn tiny_messages_do_not_panic() {
        for width in 1..30 {
            for height in 1..6 {
                draw_message(width, height, MERGE_NOTE);
                draw_message(width, height, "陈志民陈志民");
                draw_message(width, height, "");
            }
        }
    }

    fn draw_title(width: u16, title: Line<'static>) -> String {
        let mut t = Terminal::new(TestBackend::new(width, 3)).unwrap();
        t.draw(|f| {
            let area = f.area();
            f.render_widget(panel().title(title), area);
        })
        .unwrap();
        rows_of(&t).remove(0)
    }

    fn text_of(line: &Line) -> String {
        line.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The counts in a title are all there or not there at all. A cut would make
    /// `+12` read like a whole count.
    fn assert_counts_whole(row: &str, width: u16) {
        let whole = row.contains("+123456") && row.contains("-78901");
        let none = !row.contains('+') && !row.contains(" -");
        assert!(whole || none, "width {width}: {row}");
    }

    fn big_commit(author: &str) -> Commit {
        Commit { files: vec![stat("a.rs", 123456, 78901)], ..commit("f512c0e", "Subject", author, 0) }
    }

    fn big_cmp() -> BranchCmp {
        BranchCmp { merge_base: "abcdef0123456".into(), ahead: 2, behind: 0, files: vec![stat("a.rs", 123456, 78901)] }
    }

    fn pane_name(name: &str) -> Span<'static> {
        Span::raw(format!(" {name} "))
    }

    #[test]
    fn changes_title_gives_up_words_before_counts() {
        let wide = " Changes since HEAD · 12 files  +123456 -78901 ";
        assert_eq!(text_of(&changes_title(12, 123456, 78901, 200)), wide);
        for width in 11..=200u16 {
            let row = draw_title(width, changes_title(12, 123456, 78901, width));
            assert_counts_whole(&row, width);
            assert!(row.contains(" Changes "), "width {width}: {row}");
            assert_eq!(row.contains("since HEAD"), width >= 49, "width {width}: {row}");
            assert_eq!(row.contains("12 files"), width >= 38, "width {width}: {row}");
            assert_eq!(row.contains("+123456"), width >= 27, "width {width}: {row}");
        }
    }

    #[test]
    fn commit_title_gives_up_counts_then_author_before_the_date() {
        let c = big_commit("Zhimin Chen");
        let wide = " f512c0e Zhimin Chen · 2026-11-04 · 1 files  +123456 -78901 ";
        assert_eq!(text_of(&commit_title(&c, 200)), wide);
        for width in 11..=200u16 {
            let row = draw_title(width, commit_title(&c, width));
            assert_counts_whole(&row, width);
            assert!(row.contains(" f512c0e "), "width {width}: {row}");
            assert_eq!(row.contains("+123456"), width >= cells(wide) as u16 + 2, "width {width}: {row}");
            // The date is whole or not there.
            assert_eq!(row.contains("2026-11-04"), width >= 22, "width {width}: {row}");
            assert!(!row.contains("2026") || row.contains("2026-11-04"), "width {width}: {row}");
        }
    }

    #[test]
    fn long_author_is_cut_to_keep_the_date_in_the_commit_title() {
        let c = big_commit(LONG_AUTHOR);
        assert_eq!(text_of(&commit_title(&c, 40)), " f512c0e Christopher Mo… · 2026-11-04 ");
        // Too little room for a useful piece of the name.
        assert_eq!(text_of(&commit_title(&c, 31)), " f512c0e Chris… · 2026-11-04 ");
        assert_eq!(text_of(&commit_title(&c, 30)), " f512c0e 2026-11-04 ");
        assert_eq!(text_of(&commit_title(&c, 21)), " f512c0e ");
        // A short name needs no more than it takes.
        assert_eq!(text_of(&commit_title(&big_commit("Bob"), 28)), " f512c0e Bob · 2026-11-04 ");
    }

    #[test]
    fn branch_title_gives_up_merge_base_then_lines_then_ahead_and_behind() {
        let wide = " feature/x · 2 ahead, 0 behind · merge base abcdef0 · 1 files  +123456 -78901 ";
        let title = |width| branch_title(pane_name("feature/x"), &big_cmp(), width);
        assert_eq!(text_of(&title(200)), wide);
        for width in 13..=200u16 {
            let row = draw_title(width, title(width));
            assert_counts_whole(&row, width);
            assert!(row.contains(" feature/x "), "width {width}: {row}");
            assert_eq!(row.contains("merge base abcdef0"), width >= 80, "width {width}: {row}");
            assert_eq!(row.contains("merge"), width >= 80, "width {width}: {row}");
            assert_eq!(row.contains("1 files"), width >= 59, "width {width}: {row}");
            assert_eq!(row.contains("2 ahead, 0 behind"), width >= 33, "width {width}: {row}");
        }
    }

    #[test]
    fn branches_title_keeps_what_the_list_is_compared_with() {
        let wide = " Branches · 7 local · compared with main ";
        assert_eq!(text_of(&branches_title(7, "main", 200)), wide);
        for width in 12..=200u16 {
            let row = draw_title(width, branches_title(7, "main", width));
            assert!(row.contains(" Branches "), "width {width}: {row}");
            assert_eq!(row.contains("compared with main"), width >= 33, "width {width}: {row}");
            assert_eq!(row.contains("compared"), width >= 33, "width {width}: {row}");
            let local = width >= 43 || (22..33).contains(&width);
            assert_eq!(row.contains("7 local"), local, "width {width}: {row}");
            // A long name is whole or not there; `compared with agen` would mislead.
            let row = draw_title(width, branches_title(7, LONG_BRANCH, width));
            assert_eq!(row.contains("compared with"), row.contains(LONG_BRANCH), "width {width}: {row}");
        }
    }

    #[test]
    fn worktree_note_keeps_the_note_and_the_end_of_the_path() {
        let path = "~/code/worktrees/agent";
        let note = " uncommitted changes in its worktree are not counted";
        assert_eq!(worktree_note(path, 200), format!("{note} · {path} "));
        assert_eq!(worktree_note(path, 70), format!("{note} · …/agent "));
        assert_eq!(worktree_note(path, 64), format!("{note} · agent "));
        assert_eq!(worktree_note(path, 63), format!("{note} "));
        assert_eq!(worktree_note(path, 55), format!("{note} "));
        assert_eq!(worktree_note(path, 54), " uncommitted changes not counted ");
        for width in 35..=120 {
            assert!(cells(&worktree_note(path, width)) <= width as usize - 2, "width {width}");
        }
    }

    #[test]
    fn directory_title_gets_shorter_in_a_narrow_stats_pane() {
        for (width, long) in [(52u16, true), (50, false)] {
            let mut t = Terminal::new(TestBackend::new(width, 12)).unwrap();
            let mut state = TableState::default();
            let s = sample_stats();
            t.draw(|f| {
                let area = f.area();
                draw_stats(f, area, &s, 1, &mut state);
            })
            .unwrap();
            let row = &rows_of(&t)[3];
            assert_eq!(row.contains("By top-level directory"), long, "width {width}: {row}");
            assert_eq!(row.contains("By directory"), !long, "width {width}: {row}");
        }
    }

    fn bottom_text(width: u16, tab: usize, loading: bool, error: Option<&str>) -> String {
        text_of(&bottom_line(width, tab, loading, error))
    }

    const WHY: &str = "fatal: bad object HEAD (git diff --cached --numstat -z HEAD)";

    #[test]
    fn bottom_bar_stays_as_it_was_while_everything_fits() {
        let old = [
            "j/k move · Enter diff · r refresh · 1-4 tabs · q quit",
            "j/k move · Enter show commit · r refresh · 1-4 tabs · q quit",
            "t range · j/k scroll · r refresh · 1-4 tabs · q quit",
            "j/k move · Enter branch diff · r refresh · 1-4 tabs · q quit",
        ];
        for (tab, hint) in old.iter().enumerate() {
            assert_eq!(bottom_text(200, tab, false, None), format!(" {hint}"));
            assert_eq!(bottom_text(200, tab, true, None), format!(" {hint}  loading…"));
            assert_eq!(bottom_text(200, tab, false, Some("boom")), format!(" {hint}  boom"));
            assert_eq!(bottom_text(200, tab, true, Some("boom")), format!(" {hint}  loading…  boom"));
        }
    }

    #[test]
    fn hints_go_whole_and_from_the_end() {
        for (tab, hints) in HINTS.iter().enumerate() {
            for width in 3..=100u16 {
                let line = bottom_text(width, tab, false, None);
                assert!(cells(&line) <= (width as usize).saturating_sub(2), "width {width}: {line}");
                let shown: Vec<&str> = line.trim_start().split(" · ").filter(|h| !h.is_empty()).collect();
                assert_eq!(shown, hints[..shown.len()], "width {width}");
            }
        }
        assert_eq!(bottom_text(60, 1, false, None), " j/k move · Enter show commit · r refresh · 1-4 tabs");
        assert_eq!(bottom_text(40, 1, false, None), " j/k move · Enter show commit");
    }

    #[test]
    fn loading_and_errors_show_before_any_hint() {
        for tab in 0..TABS.len() {
            for width in 14..=100u16 {
                let line = bottom_text(width, tab, false, Some("fatal: boom"));
                assert!(line.ends_with("  fatal: boom") || line == " fatal: boom", "width {width}: {line}");
                assert!(cells(&line) <= width as usize - 2, "width {width}: {line}");
                let line = bottom_text(width, tab, true, None);
                assert!(line.contains("loading…"), "width {width}: {line}");
                assert!(cells(&line) <= width as usize - 2, "width {width}: {line}");
            }
            // Both, then the error alone.
            assert_eq!(bottom_text(24, tab, true, Some("fatal: boom")), " loading…  fatal: boom");
            assert_eq!(bottom_text(23, tab, true, Some("fatal: boom")), " fatal: boom");
        }
    }

    #[test]
    fn long_error_stands_alone_and_keeps_its_start() {
        assert_eq!(bottom_text(40, 0, true, Some(WHY)), " fatal: bad object HEAD (git diff --c…");
        // The whole message takes 60 cells and its leading space one.
        assert_eq!(bottom_text(63, 0, false, Some(WHY)), format!(" {WHY}"));
        assert_eq!(bottom_text(62, 0, false, Some(WHY)), " fatal: bad object HEAD (git diff --cached --numstat -z HEA…");
    }

    #[test]
    fn errors_stay_on_the_one_row_of_the_bar() {
        let line = bottom_line(60, 0, false, Some("fatal: one\nhint: two"));
        assert_eq!(text_of(&line), " j/k move · Enter diff · r refresh  fatal: one hint: two");
        assert!(!text_of(&line).contains('\n'));
    }

    #[test]
    fn tiny_bottom_bars_do_not_panic() {
        for width in 0..24u16 {
            for error in [None, Some("fatal: boom"), Some(WHY), Some("陈志民陈志民")] {
                for loading in [false, true] {
                    let line = bottom_line(width, 3, loading, error);
                    if width > 0 {
                        let mut t = Terminal::new(TestBackend::new(width, 3)).unwrap();
                        t.draw(|f| {
                            let area = f.area();
                            f.render_widget(Paragraph::new(line.clone()).block(panel()), area);
                        })
                        .unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn tiny_top_bars_do_not_panic() {
        for width in 0..23 {
            for tab in 0..TABS.len() {
                draw_top(width.max(1), "demo", LONG_BRANCH, tab);
                top_line(width, "", "", tab);
            }
        }
    }
}

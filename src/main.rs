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

use git::{Branch, Branches, Bucket, Commit, FileStat, Log, Repo, Stats, WorkTree, totals};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REFRESH: Duration = Duration::from_secs(2);
const BAR: usize = 24;
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

/// Left-align `s` to `width` terminal cells (CJK characters take two cells,
/// which `{:<N}` formatting would not account for).
fn pad(s: &str, width: usize) -> String {
    let used = Span::raw(s).width();
    format!("{s}{}", " ".repeat(width.saturating_sub(used)))
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
        let [top, body, bottom] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)]).areas(f.area());
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
        let mut spans = Vec::from(pill(" diffdash ", Some(ACCENT)));
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
        f.render_widget(Paragraph::new(Line::from(spans)), area);
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
        f.render_widget(Paragraph::new(Line::from(spans)), area);
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
                WorkRow::File { kind, file } => file_row(file, max, *kind == Kind::Untracked),
            })
            .collect();
        f.render_stateful_widget(file_table(rows).block(block), area, &mut self.work_state);
    }

    fn render_log(&mut self, f: &mut Frame, area: Rect) {
        let Some(log) = &self.log else {
            f.render_widget(self.placeholder(LOG), area);
            return;
        };
        let [top, bottom] = Layout::vertical([Constraint::Percentage(62), Constraint::Fill(1)]).areas(area);
        let rows: Vec<Row> = log
            .rows
            .iter()
            .map(|r| {
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
                Row::new(vec![
                    Cell::from(Line::from(vec![
                        graph,
                        Span::raw(" "),
                        Span::styled(c.short.clone(), Style::new().fg(Color::Yellow)),
                        Span::raw(" "),
                        Span::raw(c.subject.clone()),
                    ])),
                    Cell::from(Line::from(c.author.clone()).fg(Color::Magenta)),
                    Cell::from(Line::from(c.rel_date.clone()).fg(DIM)),
                    Cell::from(Line::from(files).right_aligned()),
                    Cell::from(Line::from(add).right_aligned()),
                    Cell::from(Line::from(del).right_aligned()),
                ])
            })
            .collect();
        let widths = [
            Constraint::Fill(1),
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Length(5),
            Constraint::Length(7),
            Constraint::Length(7),
        ];
        let title = format!(" Log · latest {} commits ", log.commits.len());
        let table = Table::new(rows, widths)
            .block(panel().title(title))
            .row_highlight_style(selected_style());
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
        let mut files: Vec<&FileStat> = c.files.iter().collect();
        files.sort_by_key(|f| std::cmp::Reverse(churn(f)));
        let max = files.first().map_or(0, |f| churn(f));
        let rows: Vec<Row> = files.iter().map(|f| file_row(f, max, false)).collect();
        f.render_widget(file_table(rows).block(block), bottom);
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
        // Counts and date have fixed widths. The name column fits the longest
        // name and the bar gets what is left, so a narrow terminal shrinks the
        // bar instead of hiding branch names.
        const FIXED: u16 = 9 + 11 + 5 + 7 + 7 + 14 + 7; // six columns, seven gaps
        let names: Vec<Line<'static>> = br.list.iter().map(branch_name).collect();
        let name_w = names.iter().map(Line::width).max().unwrap_or(0) as u16;
        let room = area.width.saturating_sub(2 + FIXED);
        let bar_col = room.saturating_sub(name_w).min(BAR as u16 + 1);
        let bar_w = bar_col.saturating_sub(1) as usize;
        let max = br.list.iter().map(branch_churn).max().unwrap_or(0);
        let rows: Vec<Row> = br.list.iter().zip(names).map(|(b, name)| branch_row(b, name, max, bar_w)).collect();
        let widths = [
            Constraint::Fill(1),
            Constraint::Length(9),
            Constraint::Length(11),
            Constraint::Length(5),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(bar_col),
            Constraint::Length(14),
        ];
        let table = Table::new(rows, widths)
            .block(panel().title(title))
            .row_highlight_style(selected_style());
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
        let mut files: Vec<&FileStat> = c.files.iter().collect();
        files.sort_by_key(|f| std::cmp::Reverse(churn(f)));
        let max = files.first().map_or(0, |f| churn(f));
        let rows: Vec<Row> = files.iter().map(|f| file_row(f, max, false)).collect();
        f.render_widget(file_table(rows).block(block), bottom);
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

/// Name cell of a branch row: current-branch marker, name and dim tags.
fn branch_name(b: &Branch) -> Line<'static> {
    let (mark, style) = if b.current {
        ("* ", Style::new().fg(Color::Magenta).bold())
    } else {
        ("  ", Style::new())
    };
    let mut spans = vec![Span::raw(format!(" {mark}")), Span::styled(b.name.clone(), style)];
    if b.worktree.is_some() {
        spans.push(Span::styled(" worktree", Style::new().fg(DIM)));
    }
    if let Some(Err(_)) = &b.cmp {
        spans.push(Span::styled("  no common history", Style::new().fg(DIM)));
    }
    Line::from(spans)
}

fn branch_row(b: &Branch, name: Line<'static>, max: u64, bar_w: usize) -> Row<'static> {
    let mut cells = vec![Cell::from(name)];
    match &b.cmp {
        Some(Ok(c)) => {
            let (a, d) = totals(&c.files);
            // Zero counts are dimmed so branches with work stand out.
            let count = |n: u64, what: &str| {
                let line = Line::from(format!("{n} {what}")).right_aligned();
                Cell::from(if n == 0 { line.fg(DIM) } else { line })
            };
            cells.extend([
                count(c.ahead, "ahead"),
                count(c.behind, "behind"),
                Cell::from(Line::from(format!("{}f", c.files.len())).right_aligned().fg(DIM)),
                Cell::from(Line::from(plus(a)).right_aligned().fg(ADD)),
                Cell::from(Line::from(minus(d)).right_aligned().fg(DEL)),
                Cell::from(bar_line(a, d, max, bar_w)),
            ]);
        }
        _ => cells.extend((0..6).map(|_| Cell::from(""))),
    }
    cells.push(Cell::from(Line::from(b.rel_date.clone()).fg(DIM)));
    Row::new(cells)
}

/// `path` with the home directory shown as `~`.
fn tilde(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix(home.as_str()) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
        _ => path.to_string(),
    }
}

fn file_row(f: &FileStat, max: u64, untracked: bool) -> Row<'static> {
    let mut name = vec![Span::raw("  "), Span::raw(f.display_path())];
    if untracked {
        name.push(Span::styled(" new", Style::new().fg(ADD)));
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
        Cell::from(bar_line(f.add(), f.del(), max, BAR)),
    ])
}

fn file_table(rows: Vec<Row<'static>>) -> Table<'static> {
    let widths = [
        Constraint::Fill(1),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(BAR as u16 + 1),
    ];
    Table::new(rows, widths).row_highlight_style(selected_style())
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
    let mut git = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "core.quotePath=false", "--no-pager"])
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

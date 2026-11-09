//! Data layer: runs `git` and parses its output into plain structs.
//!
//! All parsing lives in pure functions so it can be unit-tested without a repo.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::SystemTime;

/// Line counts for one file. `added`/`deleted` are `None` for binary files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub path: String,
    pub old_path: Option<String>,
    pub added: Option<u64>,
    pub deleted: Option<u64>,
}

impl FileStat {
    pub fn is_binary(&self) -> bool {
        self.added.is_none() || self.deleted.is_none()
    }
    pub fn add(&self) -> u64 {
        self.added.unwrap_or(0)
    }
    pub fn del(&self) -> u64 {
        self.deleted.unwrap_or(0)
    }
    pub fn display_path(&self) -> String {
        match &self.old_path {
            Some(old) => format!("{old} → {}", self.path),
            None => self.path.clone(),
        }
    }
}

pub fn totals(files: &[FileStat]) -> (u64, u64) {
    files
        .iter()
        .fold((0, 0), |(a, d), f| (a + f.add(), d + f.del()))
}

#[derive(Debug, Clone, Default)]
pub struct WorkTree {
    pub branch: String,
    pub head: String,
    pub staged: Vec<FileStat>,
    pub unstaged: Vec<FileStat>,
    pub untracked: Vec<FileStat>,
    /// Tracked changes from HEAD to the working tree in one diff. Used for the
    /// headline totals so a file both staged and unstaged is not counted twice.
    pub vs_head: Vec<FileStat>,
    /// Every local branch tip plus the current-branch marker. Only compared
    /// between polls, to notice when the Branches tab is stale (for example
    /// an agent committing in another worktree, which leaves HEAD alone).
    pub branch_tips: String,
}

impl WorkTree {
    /// `(files, added, deleted)` changed since HEAD, untracked files included.
    pub fn headline(&self) -> (usize, u64, u64) {
        let (a, d) = totals(&self.vs_head);
        let (ua, ud) = totals(&self.untracked);
        (self.vs_head.len() + self.untracked.len(), a + ua, d + ud)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    pub author: String,
    pub rel_date: String,
    pub date: String,
    pub merge: bool,
    pub subject: String,
    pub files: Vec<FileStat>,
}

/// One rendered row of `git log --graph`: either a commit or a connector line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRow {
    pub graph: String,
    pub commit: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct Log {
    pub commits: Vec<Commit>,
    pub rows: Vec<GraphRow>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bucket {
    pub key: String,
    pub added: u64,
    pub deleted: u64,
    pub commits: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub commits: u64,
    pub added: u64,
    pub deleted: u64,
    /// Newest day first.
    pub by_day: Vec<Bucket>,
    /// Largest churn first.
    pub by_dir: Vec<Bucket>,
    pub by_author: Vec<Bucket>,
}

/// One local branch, compared with HEAD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    /// Full commit hash of the branch tip.
    pub tip: String,
    /// HEAD points at this branch.
    pub current: bool,
    pub rel_date: String,
    /// Path of another worktree that has this branch checked out.
    pub worktree: Option<String>,
    /// `None` for the current branch and before the first commit. `Err` when
    /// this one branch cannot be compared, e.g. it shares no history with HEAD.
    pub cmp: Option<Result<BranchCmp, String>>,
}

/// What a branch changed since it forked from HEAD: the same range as
/// `git diff HEAD...branch`, which is what a pull request shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCmp {
    pub merge_base: String,
    /// Commits on the branch that HEAD does not have.
    pub ahead: u64,
    /// Commits on HEAD that the branch does not have.
    pub behind: u64,
    pub files: Vec<FileStat>,
}

#[derive(Debug, Clone, Default)]
pub struct Branches {
    /// What the branches are compared with: HEAD's branch or `(detached …)`.
    pub head_name: String,
    /// Most recently committed first.
    pub list: Vec<Branch>,
}

/// `git for-each-ref` format read by [`parse_branches`].
const BRANCH_FORMAT: &str =
    "--format=%1e%(HEAD)%1f%(refname:short)%1f%(objectname)%1f%(committerdate:relative)%1f%(worktreepath)";

// ---------------------------------------------------------------- running git

/// A failed git call, with git's own words first. A narrow pane keeps the start of
/// a message, and the arguments can be long enough to fill any row before the reason.
fn failure(args: &[&str], stderr: &str) -> String {
    let cmd = args.join(" ");
    match stderr.trim() {
        "" => format!("git {cmd} failed"),
        why => format!("{why} (git {cmd})"),
    }
}

/// Base command for every git call, so the read-only settings cannot be left
/// out. Both keep diffdash from writing `.git/index`, which would take
/// `.git/index.lock` and could make a concurrent `git commit` (e.g. by an
/// agent) fail:
/// - `--no-optional-locks`: `git status` and friends skip their index refresh.
/// - `diff.autoRefreshIndex=false`: `git diff` still refreshes and rewrites the
///   index for a file with new stat data but unchanged content (a file
///   rewritten as-is), even with `--no-optional-locks`.
pub fn git_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(["-c", "core.quotePath=false", "-c", "diff.autoRefreshIndex=false"])
        .args(["--no-pager", "--no-optional-locks"]);
    cmd
}

/// Cached line count of an untracked file, keyed by path and invalidated by
/// modification time and size.
struct CachedCount {
    mtime: Option<SystemTime>,
    len: u64,
    lines: Option<u64>,
}

pub struct Repo {
    pub root: PathBuf,
    untracked_cache: Mutex<HashMap<String, CachedCount>>,
    /// Branch comparisons keyed by `(HEAD, tip)` commit hashes. Commits never
    /// change, so an entry stays valid until HEAD or the branch moves.
    branch_cache: Mutex<HashMap<(String, String), BranchCmp>>,
}

impl Repo {
    pub fn open(path: &Path) -> Result<Repo, String> {
        let out = git_cmd(path)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .map_err(|e| format!("failed to run git: {e}"))?;
        if !out.status.success() {
            return Err(format!("not a git repository: {}", path.display()));
        }
        let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(Repo::at(PathBuf::from(root)))
    }

    pub(crate) fn at(root: PathBuf) -> Repo {
        Repo {
            root,
            untracked_cache: Mutex::new(HashMap::new()),
            branch_cache: Mutex::new(HashMap::new()),
        }
    }

    fn git(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let out = git_cmd(&self.root)
            .args(args)
            .output()
            .map_err(|e| format!("failed to run git: {e}"))?;
        // `git diff --no-index` exits 1 when files differ; treat 0/1 as success.
        match out.status.code() {
            Some(0) | Some(1) => Ok(out.stdout),
            _ => Err(failure(args, &String::from_utf8_lossy(&out.stderr))),
        }
    }

    fn git_str(&self, args: &[&str]) -> String {
        self.git(args)
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
            .unwrap_or_default()
    }

    /// HEAD's commit hash (empty before the first commit) and its display
    /// name: the branch, or `(detached abc1234)`.
    fn head(&self) -> (String, String) {
        let head = self.git_str(&["rev-parse", "-q", "--verify", "HEAD"]);
        let mut name = self.git_str(&["symbolic-ref", "--short", "-q", "HEAD"]);
        if name.is_empty() {
            name = if head.is_empty() {
                "(no commits)".into()
            } else {
                format!("(detached {})", &head[..head.len().min(7)])
            };
        }
        (head, name)
    }

    pub fn worktree(&self) -> Result<WorkTree, String> {
        let (head, branch) = self.head();
        // Before the first commit there is no HEAD; diff against the empty tree.
        let base = if head.is_empty() {
            self.git_str(&["hash-object", "-t", "tree", "/dev/null"])
        } else {
            "HEAD".to_string()
        };
        let unstaged = parse_numstat_z(&self.git(&["diff", "--numstat", "-z"])?);
        let staged = parse_numstat_z(&self.git(&["diff", "--cached", "--numstat", "-z", &base])?);
        let vs_head = parse_numstat_z(&self.git(&["diff", "--numstat", "-z", &base])?);
        let listed = self.git(&["ls-files", "--others", "--exclude-standard", "-z"])?;
        let untracked = self.untracked_stats(split_z(&listed));
        let branch_tips = self.git_str(&["for-each-ref", "--format=%(HEAD)%(objectname) %(refname)", "refs/heads"]);
        Ok(WorkTree { branch, head, staged, unstaged, untracked, vs_head, branch_tips })
    }

    /// Line counts for untracked files, re-reading a file only when its
    /// mtime or size changed since the last poll.
    fn untracked_stats(&self, paths: Vec<String>) -> Vec<FileStat> {
        let mut cache = self.untracked_cache.lock().unwrap_or_else(|e| e.into_inner());
        let mut fresh = HashMap::with_capacity(paths.len());
        let stats = paths
            .into_iter()
            .map(|p| {
                let full = self.root.join(&p);
                let meta = fs::metadata(&full).ok();
                let (mtime, len) = meta.as_ref().map_or((None, 0), |m| (m.modified().ok(), m.len()));
                let lines = match cache.remove(&p) {
                    Some(c) if c.mtime == mtime && c.len == len && mtime.is_some() => c.lines,
                    _ => count_lines(&full),
                };
                fresh.insert(p.clone(), CachedCount { mtime, len, lines });
                let (added, deleted) = match lines {
                    Some(n) => (Some(n), Some(0)),
                    None => (None, None),
                };
                FileStat { path: p, old_path: None, added, deleted }
            })
            .collect();
        // Entries for files that are no longer untracked are dropped here.
        *cache = fresh;
        stats
    }

    fn has_commits(&self) -> bool {
        !self.git_str(&["rev-parse", "-q", "--verify", "HEAD"]).is_empty()
    }

    pub fn log(&self, limit: usize) -> Result<Log, String> {
        if !self.has_commits() {
            return Ok(Log::default());
        }
        let n = format!("-n{limit}");
        // --graph implies --topo-order, so ask for the same order in both calls.
        let body = self.git(&[
            "log",
            "--topo-order",
            &n,
            "--numstat",
            "--date=short",
            "--format=%x1e%H%x1f%h%x1f%an%x1f%ar%x1f%ad%x1f%P%x1f%s",
        ])?;
        let commits = parse_log(&String::from_utf8_lossy(&body));
        let graph = self.git(&["log", "--graph", "--color=never", &n, "--format=%x1f%H"])?;
        let rows = parse_graph(&String::from_utf8_lossy(&graph), &commits);
        Ok(Log { commits, rows })
    }

    /// `since_days == None` means the whole history.
    pub fn stats(&self, since_days: Option<u32>) -> Result<Stats, String> {
        if !self.has_commits() {
            return Ok(Stats::default());
        }
        let mut args = vec![
            "log".to_string(),
            "--no-merges".into(),
            "--numstat".into(),
            "--date=short".into(),
            // Committer date, so day buckets agree with --since filtering.
            "--format=%x1e%H%x1f%h%x1f%an%x1f%cr%x1f%cd%x1f%P%x1f%s".into(),
        ];
        if let Some(d) = since_days {
            args.push(format!("--since={d}.days.ago"));
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let body = self.git(&refs)?;
        Ok(aggregate(&parse_log(&String::from_utf8_lossy(&body))))
    }

    /// Local branches, most recently committed first, each compared with HEAD.
    pub fn branches(&self) -> Result<Branches, String> {
        let (head, head_name) = self.head();
        let out = self.git(&["for-each-ref", "--sort=-committerdate", BRANCH_FORMAT, "refs/heads"])?;
        let mut list = parse_branches(&String::from_utf8_lossy(&out));
        let mut cache = self.branch_cache.lock().unwrap_or_else(|e| e.into_inner());
        let mut fresh = HashMap::new();
        for b in list.iter_mut().filter(|b| !b.current && !head.is_empty()) {
            let key = (head.clone(), b.tip.clone());
            let cmp = match fresh.get(&key).cloned().or_else(|| cache.remove(&key)) {
                Some(c) => Ok(c),
                None => self.compare(&head, &b.tip),
            };
            // Errors are not cached, so a failure is retried on the next reload.
            if let Ok(c) = &cmp {
                fresh.insert(key, c.clone());
            }
            b.cmp = Some(cmp);
        }
        // Entries for branches or a HEAD that moved on are dropped here.
        *cache = fresh;
        Ok(Branches { head_name, list })
    }

    /// Compare a branch tip with HEAD from their merge base.
    fn compare(&self, head: &str, tip: &str) -> Result<BranchCmp, String> {
        // With no common ancestor `git merge-base` prints nothing and exits 1,
        // which `git()` accepts; detect it here instead of matching git's
        // (possibly translated) error text.
        let merge_base = String::from_utf8_lossy(&self.git(&["merge-base", head, tip])?).trim().to_string();
        if merge_base.is_empty() {
            return Err("no common history with HEAD".into());
        }
        let range = format!("{head}...{tip}");
        let counts = self.git(&["rev-list", "--left-right", "--count", &range])?;
        let (behind, ahead) = parse_left_right(&String::from_utf8_lossy(&counts))
            .ok_or_else(|| format!("unexpected git rev-list output for {range}"))?;
        let files = parse_numstat_z(&self.git(&["diff", "--numstat", "-z", &merge_base, tip, "--"])?);
        Ok(BranchCmp { merge_base, ahead, behind, files })
    }
}

/// Count lines the way git does; `None` for binary or unreadable files.
///
/// Like git, a file is binary if its first 8000 bytes contain a NUL. The file
/// is streamed, so large text files never get loaded into memory at once.
fn count_lines(path: &Path) -> Option<u64> {
    if !fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut file = fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 64 * 1024];
    let (mut lines, mut seen, mut last) = (0u64, 0usize, b'\n');
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        if seen < 8000 && chunk[..n.min(8000 - seen)].contains(&0) {
            return None;
        }
        seen = seen.saturating_add(n);
        lines += chunk.iter().filter(|&&b| b == b'\n').count() as u64;
        last = chunk[n - 1];
    }
    if last != b'\n' {
        lines += 1;
    }
    Some(lines)
}

// -------------------------------------------------------------------- parsing

fn split_z(buf: &[u8]) -> Vec<String> {
    buf.split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

fn parse_count(s: &str) -> Option<u64> {
    s.parse().ok()
}

/// Parse `git diff --numstat -z`.
///
/// Normal record: `A\tD\tpath\0`. Rename/copy: `A\tD\t\0old\0new\0`.
/// Binary files report `-\t-`.
pub fn parse_numstat_z(buf: &[u8]) -> Vec<FileStat> {
    let mut out = Vec::new();
    let mut tokens = buf.split(|&b| b == 0);
    while let Some(tok) = tokens.next() {
        if tok.is_empty() {
            continue;
        }
        let tok = String::from_utf8_lossy(tok);
        let mut parts = tok.splitn(3, '\t');
        let (Some(a), Some(d), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let (path, old_path) = if rest.is_empty() {
            let old = tokens.next().map(|t| String::from_utf8_lossy(t).into_owned());
            let new = tokens.next().map(|t| String::from_utf8_lossy(t).into_owned());
            match (old, new) {
                (Some(o), Some(n)) => (n, Some(o)),
                _ => continue,
            }
        } else {
            (rest.to_string(), None)
        };
        out.push(FileStat { path, old_path, added: parse_count(a), deleted: parse_count(d) });
    }
    out
}

/// Turn a non-`-z` numstat rename path (`src/{a => b}/x.rs`, `a => b`) into
/// `(new_path, Some(old_path))`.
pub fn split_rename(p: &str) -> (String, Option<String>) {
    if let (Some(open), Some(close)) = (p.find('{'), p.rfind('}'))
        && open < close
        && let Some((o, n)) = p[open + 1..close].split_once(" => ")
    {
        let (pre, post) = (&p[..open], &p[close + 1..]);
        let join = |mid: &str| format!("{pre}{mid}{post}").replace("//", "/");
        return (join(n), Some(join(o)));
    }
    match p.split_once(" => ") {
        Some((o, n)) => (n.to_string(), Some(o.to_string())),
        None => (p.to_string(), None),
    }
}

/// Parse `git log --numstat --format=%x1e%H%x1f%h%x1f%an%x1f%ar%x1f%ad%x1f%P%x1f%s`.
///
/// Merge commits carry no numstat lines by default; they get an empty file list.
pub fn parse_log(text: &str) -> Vec<Commit> {
    let mut out = Vec::new();
    for chunk in text.split('\x1e') {
        let mut lines = chunk.lines();
        let Some(header) = lines.next() else { continue };
        let f: Vec<&str> = header.splitn(7, '\x1f').collect();
        if f.len() < 7 {
            continue;
        }
        let files = lines
            .filter_map(|l| {
                let mut p = l.splitn(3, '\t');
                let (a, d, path) = (p.next()?, p.next()?, p.next()?);
                let (path, old_path) = split_rename(path);
                Some(FileStat { path, old_path, added: parse_count(a), deleted: parse_count(d) })
            })
            .collect();
        out.push(Commit {
            hash: f[0].into(),
            short: f[1].into(),
            author: f[2].into(),
            rel_date: f[3].into(),
            date: f[4].into(),
            merge: f[5].split_whitespace().count() > 1,
            subject: f[6].into(),
            files,
        });
    }
    out
}

/// Parse `git log --graph --format=%x1f%H`, linking rows to `commits` by hash.
pub fn parse_graph(text: &str, commits: &[Commit]) -> Vec<GraphRow> {
    let index: HashMap<&str, usize> =
        commits.iter().enumerate().map(|(i, c)| (c.hash.as_str(), i)).collect();
    text.lines()
        .map(|line| match line.split_once('\x1f') {
            Some((graph, hash)) => GraphRow {
                graph: graph.trim_end().to_string(),
                commit: index.get(hash.trim()).copied(),
            },
            None => GraphRow { graph: line.trim_end().to_string(), commit: None },
        })
        .collect()
}

/// Parse `git for-each-ref` output written with [`BRANCH_FORMAT`].
pub fn parse_branches(text: &str) -> Vec<Branch> {
    text.split('\x1e')
        .filter_map(|record| {
            let f: Vec<&str> = record.trim_end_matches('\n').splitn(5, '\x1f').collect();
            if f.len() < 5 {
                return None;
            }
            let current = f[0] == "*";
            Some(Branch {
                name: f[1].into(),
                tip: f[2].into(),
                current,
                rel_date: f[3].into(),
                // The current branch is checked out here. Any other branch with
                // a path is checked out in another worktree.
                worktree: (!current && !f[4].is_empty()).then(|| f[4].to_string()),
                cmp: None,
            })
        })
        .collect()
}

/// Parse `git rev-list --left-right --count A...B` (`left\tright`).
pub fn parse_left_right(text: &str) -> Option<(u64, u64)> {
    let mut it = text.split_whitespace();
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn top_dir(path: &str) -> String {
    match path.split_once('/') {
        Some((dir, _)) => format!("{dir}/"),
        None => "(root)".into(),
    }
}

fn bump(map: &mut HashMap<String, Bucket>, key: String, a: u64, d: u64, commits: u64) {
    let b = map.entry(key.clone()).or_insert_with(|| Bucket { key, ..Default::default() });
    b.added += a;
    b.deleted += d;
    b.commits += commits;
}

pub fn aggregate(commits: &[Commit]) -> Stats {
    let mut s = Stats::default();
    let (mut day, mut dir, mut author) = (HashMap::new(), HashMap::new(), HashMap::new());
    for c in commits {
        let (a, d) = totals(&c.files);
        s.commits += 1;
        s.added += a;
        s.deleted += d;
        bump(&mut day, c.date.clone(), a, d, 1);
        bump(&mut author, c.author.clone(), a, d, 1);
        let mut dirs: HashMap<String, (u64, u64)> = HashMap::new();
        for f in &c.files {
            let e = dirs.entry(top_dir(&f.path)).or_default();
            e.0 += f.add();
            e.1 += f.del();
        }
        for (k, (a, d)) in dirs {
            bump(&mut dir, k, a, d, 1);
        }
    }
    let churn_desc = |v: &mut Vec<Bucket>| {
        v.sort_by(|x, y| (y.added + y.deleted).cmp(&(x.added + x.deleted)).then(x.key.cmp(&y.key)))
    };
    s.by_day = day.into_values().collect();
    s.by_day.sort_by(|x, y| y.key.cmp(&x.key));
    s.by_dir = dir.into_values().collect();
    churn_desc(&mut s.by_dir);
    s.by_author = author.into_values().collect();
    churn_desc(&mut s.by_author);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs(path: &str, a: Option<u64>, d: Option<u64>) -> FileStat {
        FileStat { path: path.into(), old_path: None, added: a, deleted: d }
    }

    #[test]
    fn numstat_plain_binary_and_spaces() {
        let buf = b"3\t1\tsrc/main.rs\0-\t-\timg/logo.png\x0010\t0\tdir with space/a b.txt\0";
        let got = parse_numstat_z(buf);
        assert_eq!(
            got,
            vec![
                fs("src/main.rs", Some(3), Some(1)),
                fs("img/logo.png", None, None),
                fs("dir with space/a b.txt", Some(10), Some(0)),
            ]
        );
        assert!(got[1].is_binary());
    }

    #[test]
    fn numstat_rename_record() {
        let buf = b"2\t5\t\0old/name.rs\0new/name.rs\0" as &[u8];
        let got = parse_numstat_z(buf);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "new/name.rs");
        assert_eq!(got[0].old_path.as_deref(), Some("old/name.rs"));
        assert_eq!((got[0].add(), got[0].del()), (2, 5));
    }

    #[test]
    fn rename_path_forms() {
        assert_eq!(
            split_rename("src/{a => b}/x.rs"),
            ("src/b/x.rs".into(), Some("src/a/x.rs".into()))
        );
        assert_eq!(split_rename("src/{ => sub}/x.rs"), ("src/sub/x.rs".into(), Some("src/x.rs".into())));
        assert_eq!(split_rename("a.txt => b.txt"), ("b.txt".into(), Some("a.txt".into())));
        assert_eq!(split_rename("plain.rs"), ("plain.rs".into(), None));
    }

    #[test]
    fn log_with_merge_and_files() {
        let text = "\x1eaaa\x1fa\x1fAlice\x1f2 hours ago\x1f2026-11-02\x1fp1 p2\x1fMerge branch x\n\
                    \x1ebbb\x1fb\x1fBob\x1f1 day ago\x1f2026-11-01\x1fp1\x1fAdd solver\n\n\
                    10\t2\tsrc/solver.rs\n-\t-\tdata.bin\n";
        let log = parse_log(text);
        assert_eq!(log.len(), 2);
        assert!(log[0].merge);
        assert!(log[0].files.is_empty());
        assert!(!log[1].merge);
        assert_eq!(log[1].files.len(), 2);
        assert_eq!(totals(&log[1].files), (10, 2));
        assert_eq!(log[1].subject, "Add solver");
    }

    #[test]
    fn graph_rows_link_to_commits() {
        let commits = parse_log(
            "\x1eaaa\x1fa\x1fA\x1fnow\x1f2026-11-02\x1fp1 p2\x1fm\n\x1ebbb\x1fb\x1fB\x1fnow\x1f2026-11-02\x1fp\x1fc\n",
        );
        let rows = parse_graph("*   \x1faaa\n|\\  \n| * \x1fbbb\n", &commits);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].commit, Some(0));
        assert_eq!(rows[1].commit, None);
        assert_eq!(rows[1].graph, "|\\");
        assert_eq!(rows[2].commit, Some(1));
        assert_eq!(rows[2].graph, "| *");
    }

    #[test]
    fn aggregate_buckets() {
        let commits = parse_log(
            "\x1ea\x1fa\x1fAlice\x1fx\x1f2026-11-02\x1fp\x1fs\n\n5\t1\tsrc/a.rs\n2\t0\tREADME.md\n\
             \x1eb\x1fb\x1fBob\x1fx\x1f2026-11-01\x1fp\x1fs\n\n1\t9\tsrc/b.rs\n",
        );
        let s = aggregate(&commits);
        assert_eq!((s.commits, s.added, s.deleted), (2, 8, 10));
        assert_eq!(s.by_day[0].key, "2026-11-02");
        assert_eq!(s.by_dir[0], Bucket { key: "src/".into(), added: 6, deleted: 10, commits: 2 });
        assert_eq!(s.by_author[0].key, "Bob");
    }

    #[test]
    fn branch_records() {
        let text = "\x1e*\x1fmain\x1faaa\x1f2 hours ago\x1f/work/repo\n\
                    \x1e \x1fagent/x\x1fbbb\x1f5 minutes ago\x1f/work/repo-wt\n\
                    \x1e \x1fold\x1fccc\x1f3 weeks ago\x1f\n";
        let got = parse_branches(text);
        assert_eq!(got.len(), 3);
        assert!(got[0].current);
        assert_eq!((got[0].name.as_str(), got[0].tip.as_str()), ("main", "aaa"));
        // The current branch's own checkout is not "another worktree".
        assert_eq!(got[0].worktree, None);
        assert!(!got[1].current);
        assert_eq!(got[1].worktree.as_deref(), Some("/work/repo-wt"));
        assert_eq!(got[1].rel_date, "5 minutes ago");
        assert_eq!(got[2].worktree, None);
        assert!(got.iter().all(|b| b.cmp.is_none()));
    }

    #[test]
    fn left_right_counts() {
        assert_eq!(parse_left_right("1\t3\n"), Some((1, 3)));
        assert_eq!(parse_left_right(""), None);
        assert_eq!(parse_left_right("x\t1"), None);
    }

    /// Run git for test setup, isolated from the user's config (signing, hooks).
    fn setup_git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn branches_compare_from_merge_base() {
        let dir = std::env::temp_dir().join(format!("diffdash-branches-{}", std::process::id()));
        let wt = dir.with_extension("wt");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&wt);
        fs::create_dir_all(&dir).unwrap();
        let commit = |files: &[(&str, &str)], msg: &str| {
            for (name, body) in files {
                fs::write(dir.join(name), body).unwrap();
            }
            setup_git(&dir, &["add", "-A"]);
            setup_git(&dir, &["commit", "-qm", msg]);
        };
        setup_git(&dir, &["init", "-q", "-b", "main"]);
        commit(&[("f.txt", "a\nb\nc\n")], "base");
        setup_git(&dir, &["switch", "-qc", "feature"]);
        commit(&[("f.txt", "a\nb\nc\nd\ne\n"), ("new.rs", "1\n2\n3\n4\n")], "feature work");
        setup_git(&dir, &["switch", "-q", "main"]);
        commit(&[("m.txt", "m\n")], "main moves on");
        setup_git(&dir, &["branch", "old", "HEAD~1"]);
        setup_git(&dir, &["switch", "-q", "--orphan", "lonely"]);
        commit(&[("z.txt", "z\n")], "unrelated");
        setup_git(&dir, &["switch", "-qf", "main"]);
        setup_git(&dir, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "agent"]);

        let repo = Repo::open(&dir).unwrap();
        let br = repo.branches().unwrap();
        assert_eq!(br.head_name, "main");
        assert_eq!(br.list.len(), 5);
        let get = |name: &str| br.list.iter().find(|b| b.name == name).unwrap();
        let main = get("main");
        assert!(main.current && main.cmp.is_none() && main.worktree.is_none());
        // feature forked before main's last commit: 1 ahead, 1 behind, and only
        // its own changes count (m.txt from main is not part of the diff).
        let Some(Ok(c)) = &get("feature").cmp else { panic!("feature: {:?}", get("feature").cmp) };
        assert_eq!((c.ahead, c.behind), (1, 1));
        assert_eq!(c.files.len(), 2);
        assert_eq!(totals(&c.files), (6, 0));
        // A branch already contained in HEAD has nothing ahead and no changes.
        let Some(Ok(c)) = &get("old").cmp else { panic!("old: {:?}", get("old").cmp) };
        assert_eq!((c.ahead, c.behind, c.files.len()), (0, 1, 0));
        // An unrelated branch reports its own error without failing the rest.
        assert!(matches!(&get("lonely").cmp, Some(Err(e)) if e.contains("no common history")));
        // A branch checked out in another worktree is marked.
        let agent = get("agent");
        assert!(agent.worktree.is_some());
        assert!(matches!(&agent.cmp, Some(Ok(c)) if c.ahead == 0 && c.behind == 0));

        // The second load comes from the (HEAD, tip) cache; errors are not cached.
        assert_eq!(repo.branch_cache.lock().unwrap().len(), 3);
        let again = repo.branches().unwrap();
        let cmps = |b: &Branches| b.list.iter().map(|x| (x.name.clone(), x.cmp.clone())).collect::<Vec<_>>();
        let (mut first, mut second) = (cmps(&br), cmps(&again));
        first.sort_by(|a, b| a.0.cmp(&b.0));
        second.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(first, second);

        fs::remove_dir_all(&wt).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failure_says_why_first() {
        assert_eq!(failure(&["log", "-n1"], "fatal: bad object HEAD\n"), "fatal: bad object HEAD (git log -n1)");
        assert_eq!(failure(&["log", "-n1"], "  \n"), "git log -n1 failed");
    }

    #[test]
    fn a_broken_repo_reports_why_before_the_command() {
        let dir = std::env::temp_dir().join(format!("diffdash-broken-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        setup_git(&dir, &["init", "-q", "-b", "main"]);
        setup_git(&dir, &["commit", "-q", "--allow-empty", "-m", "init"]);
        fs::write(dir.join(".git/refs/heads/main"), "0000000000000000000000000000000000000001\n").unwrap();

        let repo = Repo::open(&dir).unwrap();
        // git's own words can be translated, so this checks only where they are.
        let e = repo.worktree().err().unwrap();
        assert!(!e.starts_with("git "), "{e}");
        assert!(e.contains(" (git diff ") && e.ends_with(')'), "{e}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn poll_never_rewrites_index() {
        let dir = std::env::temp_dir().join(format!("diffdash-index-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        setup_git(&dir, &["init", "-q", "-b", "main"]);
        fs::write(dir.join("a.txt"), "same\n").unwrap();
        setup_git(&dir, &["add", "-A"]);
        setup_git(&dir, &["commit", "-qm", "init"]);
        // Same content, new mtime: what an agent or formatter rewriting a file
        // unchanged leaves behind. `git diff` refreshes the index for such a
        // file unless told not to, and --no-optional-locks alone does not stop it.
        let file = fs::File::options().write(true).open(dir.join("a.txt")).unwrap();
        file.set_modified(SystemTime::now() + std::time::Duration::from_secs(3600)).unwrap();
        drop(file);
        let index = dir.join(".git/index");
        let before = fs::read(&index).unwrap();

        let repo = Repo::open(&dir).unwrap();
        let w = repo.worktree().unwrap();
        assert!(w.unstaged.is_empty() && w.vs_head.is_empty(), "same content is not a change");
        assert!(fs::read(&index).unwrap() == before, "polling rewrote .git/index (took index.lock)");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn untracked_line_count() {
        let dir = std::env::temp_dir().join(format!("diffdash-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let text = dir.join("a.txt");
        fs::write(&text, "one\ntwo\nthree").unwrap();
        assert_eq!(count_lines(&text), Some(3));
        fs::write(&text, "one\ntwo\n").unwrap();
        assert_eq!(count_lines(&text), Some(2));
        fs::write(&text, "").unwrap();
        assert_eq!(count_lines(&text), Some(0));
        let bin = dir.join("b.bin");
        fs::write(&bin, [1u8, 0, 2, 3]).unwrap();
        assert_eq!(count_lines(&bin), None);
        // A NUL after the first 8000 bytes does not make the file binary.
        let late = dir.join("late.txt");
        let mut body = "x\n".repeat(5000).into_bytes();
        body.push(0);
        fs::write(&late, &body).unwrap();
        assert_eq!(count_lines(&late), Some(5001));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn untracked_cache_invalidates_on_change() {
        let dir = std::env::temp_dir().join(format!("diffdash-cache-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let repo = Repo::at(dir.clone());
        fs::write(dir.join("a.txt"), "1\n2\n3\n").unwrap();
        let first = repo.untracked_stats(vec!["a.txt".into()]);
        assert_eq!(first[0].added, Some(3));
        fs::write(dir.join("a.txt"), "1\n2\n3\n4\n5\n").unwrap();
        let second = repo.untracked_stats(vec!["a.txt".into()]);
        assert_eq!(second[0].added, Some(5));
        // Files that stop being untracked are evicted from the cache.
        repo.untracked_stats(vec![]);
        assert!(repo.untracked_cache.lock().unwrap().is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}

//! The diff behind Enter. git paints a whole changed line red or green, so a
//! changed comment looks like changed code. diffdash asks git for plain text and
//! paints it itself: git's default colors, plus comments faint, in the line's own
//! red or green on a changed line, so the change still shows, and in the plain
//! color on an unchanged line. less shows the result, as before.
//!
//! Palette colors and attributes only, since the user switches between light and
//! dark themes. The user picked faint from a side by side in their Ghostty, over
//! gray comments (palette 8 or 0) and over italic. Their Ghostty shows no italic
//! (it asks 0xProto for a Semibold Italic it does not have), and no bold either,
//! since it draws regular text in Bold.

use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::git;
use crate::syntax::{self, Lang, Lexer};

// Each code starts with a reset, so nothing carries over from the span before.
const PLAIN: &[u8] = b"\x1b[m";
const META: &[u8] = b"\x1b[0;1m";
const FRAG: &[u8] = b"\x1b[0;36m";
const COMMIT: &[u8] = b"\x1b[0;33m";
const OLD: &[u8] = b"\x1b[0;31m";
const NEW: &[u8] = b"\x1b[0;32m";
const OLD_NOTE: &[u8] = b"\x1b[0;31;2m";
const NEW_NOTE: &[u8] = b"\x1b[0;32;2m";
/// A comment on an unchanged line: faint, in the plain color.
const NOTE: &[u8] = b"\x1b[0;2m";
/// Blanks at the end of an added line, on red as git shows them.
const BLANKS: &[u8] = b"\x1b[0;41m";

/// Shows `git <args>` in `less -R -+F`, painted. `args` must ask git for plain
/// text (`--no-color`, which beats `color.ui = always`).
pub fn page(root: &Path, args: &[String]) -> Result<(), String> {
    let mut git = git::git_cmd(root)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to run git: {e}"))?;
    let diff = git.stdout.take().ok_or("no git output")?;
    // -+F overrides a user's LESS=-F, which would make less quit at once on a
    // short diff and look like Enter did nothing.
    let mut less = match Command::new("less").args(["-R", "-+F"]).stdin(Stdio::piped()).spawn() {
        Ok(less) => less,
        Err(e) => {
            drop(diff);
            let _ = git.wait();
            return Err(format!("failed to run less: {e}"));
        }
    };
    if let Some(pipe) = less.stdin.take() {
        // A write fails once less has quit, when q comes before the end of the
        // diff. That is how reading stops early, not an error.
        let _ = paint_all(BufReader::new(diff), BufWriter::new(pipe));
    }
    // Both pipes are closed now, so less sees the end of the diff, and git, if it
    // was not done, stops at its next write.
    let status = less.wait();
    let _ = git.wait();
    status.map(|_| ()).map_err(|e| format!("failed to run less: {e}"))
}

/// Paints `input` into `out`, line by line, until the input ends or a write fails.
pub fn paint_all(mut input: impl BufRead, mut out: impl Write) -> io::Result<()> {
    let mut painter = Painter::new();
    let (mut line, mut painted) = (Vec::new(), Vec::new());
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            return out.flush();
        }
        painted.clear();
        painter.line(line.strip_suffix(b"\n").unwrap_or(&line), &mut painted);
        painted.push(b'\n');
        out.write_all(&painted)?;
    }
}

/// Paints the output of `git diff` or `git show`, one line at a time.
pub struct Painter {
    part: Part,
    lang: Option<&'static Lang>,
    /// Marker columns before the text of a hunk line: 1, or one per parent in the
    /// combined diff of a merge.
    marks: usize,
    /// One lexer per side of a hunk: the old file (unchanged and `-` lines) and the
    /// new one (unchanged and `+` lines), since an added `/*` opens a comment in the
    /// new file only.
    old: Lexer,
    new: Lexer,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    /// Before the first `diff `: the commit and the `--stat` of `git show`.
    Preamble,
    /// From `diff ` to the first `@@`.
    Header,
    Hunk,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Change {
    Old,
    New,
    Same,
}

impl Painter {
    pub fn new() -> Painter {
        Painter { part: Part::Preamble, lang: None, marks: 1, old: Lexer::default(), new: Lexer::default() }
    }

    /// Appends `line`, a line of git's output without its newline, painted.
    pub fn line(&mut self, line: &[u8], out: &mut Vec<u8>) {
        if line.starts_with(b"diff ") {
            self.part = Part::Header;
            self.lang = None;
            return whole(out, META, line);
        }
        match self.part {
            Part::Preamble => preamble(line, out),
            Part::Header | Part::Hunk if line.starts_with(b"@@") => self.hunk_header(line, out),
            Part::Header => {
                // Only here, between `diff ` and the first `@@`: inside a hunk, a
                // removed Lua or SQL comment `-- x` reads `--- x`.
                if let Some(path) = line.strip_prefix(b"--- ").or_else(|| line.strip_prefix(b"+++ "))
                    && let Some(lang) = lang_of(path)
                {
                    self.lang = Some(lang);
                }
                whole(out, META, line)
            }
            // Text lines start with a marker; `diff ` and `@@` lines never do.
            Part::Hunk => match line.first() {
                None | Some(b' ' | b'+' | b'-') => self.text(line, out),
                // `\ No newline at end of file`
                Some(b'\\') => out.extend_from_slice(line),
                _ => {
                    self.part = Part::Preamble;
                    preamble(line, out)
                }
            },
        }
    }

    fn hunk_header(&mut self, line: &[u8], out: &mut Vec<u8>) {
        self.part = Part::Hunk;
        let ats = line.iter().take_while(|&&b| b == b'@').count();
        self.marks = ats.saturating_sub(1).max(1);
        self.old = Lexer::default();
        self.new = Lexer::default();
        // `@@ -1,4 +1,5 @@` in cyan and the function after it plain, as git has them.
        let fence = &line[..ats];
        let end = line[ats..].windows(ats).position(|w| w == fence).map_or(line.len(), |k| ats + k + ats);
        whole(out, FRAG, &line[..end]);
        out.extend_from_slice(&line[end..]);
    }

    /// A line of a hunk: its markers, then the text, with the comments faint.
    fn text(&mut self, line: &[u8], out: &mut Vec<u8>) {
        let (marks, text) = line.split_at(self.marks.min(line.len()));
        let change = if marks.contains(&b'-') {
            Change::Old
        } else if marks.contains(&b'+') {
            Change::New
        } else {
            Change::Same
        };
        let notes = match self.lang {
            None => Vec::new(),
            Some(lang) => match change {
                Change::Old => self.old.comments(lang, text),
                Change::New => self.new.comments(lang, text),
                // Shown as the new file reads it; the old file moves on as well.
                Change::Same => {
                    self.old.comments(lang, text);
                    self.new.comments(lang, text)
                }
            },
        };
        let (code, note) = match change {
            Change::Old => (OLD, OLD_NOTE),
            Change::New => (NEW, NEW_NOTE),
            Change::Same => (PLAIN, NOTE),
        };
        let mut spans = Vec::new();
        let mut at = 0;
        for r in notes {
            spans.push((at..r.start, code));
            at = r.end;
            spans.push((r, note));
        }
        spans.push((at..text.len(), code));
        let blanks = match change {
            Change::New => text.len() - text.iter().rev().take_while(|b| matches!(b, b' ' | b'\t')).count(),
            _ => text.len(),
        };
        let mut pen = Pen { out, style: PLAIN };
        pen.put(code, marks);
        for (r, style) in spans {
            let cut = blanks.clamp(r.start, r.end);
            pen.put(style, &text[r.start..cut]);
            pen.put(BLANKS, &text[cut..r.end]);
        }
        pen.end();
    }
}

/// A line before the first `diff `: the commit line in yellow and the graph of
/// `--stat` (` src/main.rs | 12 +++++------`) in green and red, as git has them. A
/// commit message is indented by four, so no line of it is taken for a stat line.
fn preamble(line: &[u8], out: &mut Vec<u8>) {
    if line.starts_with(b"commit ") {
        return whole(out, COMMIT, line);
    }
    if line.starts_with(b" ")
        && !line.starts_with(b"  ")
        && let Some(bar) = line.windows(3).rposition(|w| w == b" | ")
    {
        let num = bar + 3 + line[bar + 3..].iter().take_while(|&&b| b == b' ').count();
        let digits = line[num..].iter().take_while(|b| b.is_ascii_digit()).count();
        if digits > 0 {
            out.extend_from_slice(&line[..num + digits]);
            let mut pen = Pen { out, style: PLAIN };
            for b in line[num + digits..].chunks(1) {
                pen.put(
                    match b[0] {
                        b'+' => NEW,
                        b'-' => OLD,
                        _ => PLAIN,
                    },
                    b,
                );
            }
            return pen.end();
        }
    }
    out.extend_from_slice(line);
}

/// The language of the file in `--- a/src/x.rs` or `+++ b/src/x.rs`, by its name.
fn lang_of(path: &[u8]) -> Option<&'static Lang> {
    // git ends a name that holds a blank with a tab, and quotes an odd one.
    let path = path.split(|&b| b == b'\t').next().unwrap_or(path);
    let path = path.strip_prefix(b"\"").and_then(|p| p.strip_suffix(b"\"")).unwrap_or(path);
    if path == b"/dev/null" {
        return None;
    }
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    syntax::lang_for(std::str::from_utf8(name).ok()?)
}

fn whole(out: &mut Vec<u8>, style: &[u8], line: &[u8]) {
    out.extend_from_slice(style);
    out.extend_from_slice(line);
    out.extend_from_slice(PLAIN);
}

/// Writes spans of text, with a style code only where the style changes.
struct Pen<'a> {
    out: &'a mut Vec<u8>,
    style: &'static [u8],
}

impl Pen<'_> {
    fn put(&mut self, style: &'static [u8], text: &[u8]) {
        if text.is_empty() {
            return;
        }
        if style != self.style {
            self.out.extend_from_slice(style);
            self.style = style;
        }
        self.out.extend_from_slice(text);
    }

    fn end(self) {
        if self.style != PLAIN {
            self.out.extend_from_slice(PLAIN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each line of `input`, painted, with ESC shown as `~` so a failure reads.
    fn painted(input: &str) -> Vec<String> {
        let mut out = Vec::new();
        paint_all(input.as_bytes(), &mut out).unwrap();
        String::from_utf8(out).unwrap().replace('\x1b', "~").lines().map(String::from).collect()
    }

    const SHOW: &str = "commit 0123abcd
Author: T <t@example.com>
Date:   Mon Nov 16 10:00:00 2026 +0800

    Fix the sum | for real

 src/main.rs | 4 ++--
 1 file changed, 2 insertions(+), 2 deletions(-)

diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,4 @@ fn main() {
     // context comment
-    let x = 1; // old
+    // new comment
+    let x = 2;\x20\x20
     println!(\"// not a comment\");
\\ No newline at end of file
";

    #[test]
    fn paints_like_git_with_comments_faint() {
        let got = painted(SHOW);
        let want = [
            "~[0;33mcommit 0123abcd~[m",
            "Author: T <t@example.com>",
            "Date:   Mon Nov 16 10:00:00 2026 +0800",
            "",
            "    Fix the sum | for real",
            "",
            " src/main.rs | 4 ~[0;32m++~[0;31m--~[m",
            " 1 file changed, 2 insertions(+), 2 deletions(-)",
            "",
            "~[0;1mdiff --git a/src/main.rs b/src/main.rs~[m",
            "~[0;1mindex 1111111..2222222 100644~[m",
            "~[0;1m--- a/src/main.rs~[m",
            "~[0;1m+++ b/src/main.rs~[m",
            "~[0;36m@@ -1,4 +1,4 @@~[m fn main() {",
            "     ~[0;2m// context comment~[m",
            "~[0;31m-    let x = 1; ~[0;31;2m// old~[m",
            "~[0;32m+    ~[0;32;2m// new comment~[m",
            "~[0;32m+    let x = 2;~[0;41m  ~[m",
            "     println!(\"// not a comment\");",
            "\\ No newline at end of file",
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn a_file_it_does_not_know_keeps_gits_colors() {
        let got = painted("diff --git a/x.xyz b/x.xyz\n--- a/x.xyz\n+++ b/x.xyz\n@@ -1 +1 @@\n-// a\n+// b\n");
        assert_eq!(got[4..], ["~[0;31m-// a~[m", "~[0;32m+// b~[m"]);
    }

    #[test]
    fn a_removed_lua_comment_is_no_file_header() {
        let got = painted("diff --git a/a.lua b/a.lua\n--- a/a.lua\n+++ b/a.lua\n@@ -1,2 +1 @@\n--- old note\n x = 1\n");
        assert_eq!(got[4..], ["~[0;31m-~[0;31;2m-- old note~[m", " x = 1"]);
    }

    #[test]
    fn each_side_keeps_its_own_comment_state() {
        let head = "diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n";
        // An added `/*` makes the unchanged lines after it a comment in the new file.
        let got = painted(&format!("{head}@@ -1,2 +1,4 @@\n+/* opened\n int a;\n+*/\n int b;\n"));
        assert_eq!(got[4..], ["~[0;32m+~[0;32;2m/* opened~[m", " ~[0;2mint a;~[m", "~[0;32m+~[0;32;2m*/~[m", " int b;"]);
        // A removed one does not: unchanged lines show as the new file reads them.
        let got = painted(&format!("{head}@@ -1,3 +1,1 @@\n-/* gone\n int a;\n-*/\n"));
        assert_eq!(got[4..], ["~[0;31m-~[0;31;2m/* gone~[m", " int a;", "~[0;31m-~[0;31;2m*/~[m"]);
    }

    #[test]
    fn each_hunk_starts_fresh() {
        let got = painted(
            "diff --git a/a.c b/a.c\n--- a/a.c\n+++ b/a.c\n@@ -1 +1,2 @@\n+/* never closed\n x;\n@@ -9 +10 @@\n y;\n",
        );
        assert_eq!(got[7], " y;");
    }

    #[test]
    fn a_hunk_may_start_inside_a_doc_block() {
        let hunk = "@@ -3,3 +3,3 @@\n * Returns the sum.\n */\n function f() {}\n";
        let got = painted(&format!("diff --git a/a.js b/a.js\n--- a/a.js\n+++ b/a.js\n{hunk}"));
        assert_eq!(got[4..], [" ~[0;2m* Returns the sum.~[m", " ~[0;2m*/~[m", " function f() {}"]);
        // Not in Rust, where rustfmt starts a continued line with `* b`.
        let got = painted("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n     * b;\n");
        assert_eq!(got[4], "     * b;");
    }

    #[test]
    fn a_hunk_that_starts_inside_a_docstring_keeps_its_code() {
        let got = painted(
            "diff --git a/a.py b/a.py\n--- a/a.py\n+++ b/a.py\n@@ -4,3 +4,4 @@\n Returns the sum.\n \"\"\"\n x = 1\n+y = 2\n",
        );
        assert_eq!(got[4..], [" Returns the sum.", " ~[0;2m\"\"\"~[m", " x = 1", "~[0;32m+y = 2~[m"]);
    }

    #[test]
    fn a_merge_has_a_marker_column_per_parent() {
        let got = painted(
            "diff --cc a.rs\nindex 1,2..3\n--- a/a.rs\n+++ b/a.rs\n@@@ -1,2 -1,2 +1,2 @@@ fn f() {\n\
             - let a = 1; // ours\n -let b = 1; // theirs\n++// both\n  x();\n",
        );
        assert_eq!(got[4], "~[0;36m@@@ -1,2 -1,2 +1,2 @@@~[m fn f() {");
        assert_eq!(
            got[5..],
            [
                "~[0;31m- let a = 1; ~[0;31;2m// ours~[m",
                "~[0;31m -let b = 1; ~[0;31;2m// theirs~[m",
                "~[0;32m++~[0;32;2m// both~[m",
                "  x();",
            ]
        );
    }

    #[test]
    fn names_with_blanks_and_quotes_find_their_language() {
        let lang = |p: &[u8]| lang_of(p).map(|l| l.name);
        assert_eq!(lang(b"b/my dir/a b.py\t"), Some("Python"));
        assert_eq!(lang(b"\"b/we\\\"ird.rs\""), Some("Rust"));
        assert_eq!(lang(b"/dev/null"), None);
        assert_eq!(lang(b"w/src/main.rs"), Some("Rust"));
    }

    #[test]
    fn bytes_that_are_not_utf8_pass_through() {
        let mut out = Vec::new();
        paint_all(&b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n+caf\xe9 // c\n"[..], &mut out).unwrap();
        assert!(out.ends_with(b"\x1b[0;32m+caf\xe9 \x1b[0;32;2m// c\x1b[m\n"), "{:?}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn real_git_output_is_painted() {
        use crate::git::tests::setup_git;
        let dir = std::env::temp_dir().join(format!("diffdash-pager-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        setup_git(&dir, &["init", "-q", "-b", "main"]);
        // `--no-color` must win over a config that asks for color everywhere.
        setup_git(&dir, &["config", "color.ui", "always"]);
        std::fs::write(dir.join("calc.py"), "def add(a, b):\n    return a + b\n").unwrap();
        setup_git(&dir, &["add", "-A"]);
        setup_git(&dir, &["commit", "-qm", "add"]);
        std::fs::write(dir.join("calc.py"), "def add(a, b):\n    # Plain sum.\n    return a + b\n").unwrap();
        setup_git(&dir, &["commit", "-qam", "explain"]);
        let run = |args: &[&str]| git::git_cmd(&dir).args(args).output().unwrap().stdout;

        let raw = run(&["show", "--no-color", "--stat", "--patch", "HEAD"]);
        assert!(!raw.contains(&0x1b), "git colored its output");
        let mut out = Vec::new();
        paint_all(&raw[..], &mut out).unwrap();
        let text = String::from_utf8(out).unwrap().replace('\x1b', "~");
        assert!(text.starts_with("~[0;33mcommit "), "{text}");
        assert!(text.contains(" calc.py | 1 ~[0;32m+~[m\n"), "{text}");
        assert!(text.contains("~[0;1mdiff --git a/calc.py b/calc.py~[m\n"), "{text}");
        assert!(text.contains("~[0;32m+    ~[0;32;2m# Plain sum.~[m\n"), "{text}");
        assert!(text.contains("\n     return a + b\n"), "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

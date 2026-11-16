//! Where the comments are in a line of source code, so that the diff behind Enter
//! can show them apart from code: git paints a whole changed line red or green,
//! comment or not.
//!
//! A small lexer per language instead of a highlighting crate, since diffdash
//! depends on ratatui only. It knows each language's comments and strings, the
//! strings only so that `"http://x"` or `"#"` is not taken for a comment. It works
//! on bytes: every delimiter is ASCII, so no UTF-8 character is ever cut, and text
//! that is not UTF-8 goes through as it is.

use std::ops::Range;

/// A string: what opens and closes it, whether it can go on past the end of the
/// line, and whether a backslash escapes the byte after it.
#[derive(Debug)]
pub struct Quote {
    open: &'static [u8],
    close: &'static [u8],
    multi: bool,
    escape: bool,
}

const fn quote(open: &'static [u8], close: &'static [u8], multi: bool, escape: bool) -> Quote {
    Quote { open, close, multi, escape }
}

/// The comments and strings of one language.
#[derive(Debug)]
pub struct Lang {
    #[cfg_attr(not(test), allow(dead_code))]
    pub name: &'static str,
    /// Comments that run to the end of the line.
    line: &'static [&'static [u8]],
    /// Comments as (open, close). Tried before `line`, so Lua's `--[[` wins over `--`.
    block: &'static [(&'static [u8], &'static [u8])],
    /// Strings, a longer opening before one it starts with (`"""` before `"`).
    strings: &'static [Quote],
    /// A line comment counts only at the start of the line or after a blank, as `#`
    /// does in shell and YAML, where `$#` and `a#b` are no comments.
    spaced: bool,
    /// `/** … */` blocks are common, so a hunk that opens on ` * ` lines is taken to
    /// start inside one. Not for Rust, where rustfmt starts continued lines with `* `.
    star_blocks: bool,
    /// Rust: `'` opens a char literal only as `'x'` or `'\…'`, else it starts a
    /// lifetime or a label; `r"…"`, `r#"…"#` and `br"…"` are raw strings.
    rust: bool,
    /// A triple-quoted string that opens a line is a docstring, shown as a comment.
    doc_strings: bool,
    /// Fixed-form Fortran: `c`, `C`, `*` or `!` in the first column makes a comment line.
    fixed_form: bool,
}

const BASE: Lang = Lang {
    name: "",
    line: &[],
    block: &[],
    strings: &[],
    spaced: false,
    star_blocks: false,
    rust: false,
    doc_strings: false,
    fixed_form: false,
};

const SLASH: &[&[u8]] = &[b"//"];
const HASH_MARK: &[&[u8]] = &[b"#"];
const DASHES: &[&[u8]] = &[b"--"];
const C_BLOCK: &[(&[u8], &[u8])] = &[(b"/*", b"*/")];
/// `"…"` and `'…'` on one line, with backslash escapes.
const QUOTES: &[Quote] = &[quote(b"\"", b"\"", false, true), quote(b"'", b"'", false, true)];

const RUST: Lang = Lang {
    name: "Rust",
    line: SLASH,
    block: C_BLOCK,
    strings: &[quote(b"\"", b"\"", true, true)],
    rust: true,
    ..BASE
};
const C_LIKE: Lang = Lang { name: "C", line: SLASH, block: C_BLOCK, strings: QUOTES, star_blocks: true, ..BASE };
const JAVA_LIKE: Lang = Lang {
    name: "Java",
    line: SLASH,
    block: C_BLOCK,
    strings: &[
        quote(b"\"\"\"", b"\"\"\"", true, true),
        quote(b"'''", b"'''", true, true),
        quote(b"\"", b"\"", false, true),
        quote(b"'", b"'", false, true),
    ],
    star_blocks: true,
    ..BASE
};
const CSHARP: Lang = Lang {
    name: "C#",
    line: SLASH,
    block: C_BLOCK,
    strings: &[
        quote(b"\"\"\"", b"\"\"\"", true, false),
        quote(b"@\"", b"\"", true, false),
        quote(b"\"", b"\"", false, true),
        quote(b"'", b"'", false, true),
    ],
    star_blocks: true,
    ..BASE
};
const JS: Lang = Lang {
    name: "JavaScript",
    line: SLASH,
    block: C_BLOCK,
    strings: &[quote(b"`", b"`", true, true), quote(b"\"", b"\"", false, true), quote(b"'", b"'", false, true)],
    star_blocks: true,
    ..BASE
};
const GO: Lang = Lang {
    name: "Go",
    line: SLASH,
    block: C_BLOCK,
    strings: &[quote(b"`", b"`", true, false), quote(b"\"", b"\"", false, true), quote(b"'", b"'", false, true)],
    star_blocks: true,
    ..BASE
};
const PHP: Lang = Lang {
    name: "PHP",
    line: SLASH,
    block: C_BLOCK,
    strings: &[quote(b"\"", b"\"", true, true), quote(b"'", b"'", true, true)],
    star_blocks: true,
    ..BASE
};
/// No line comments: `url(http://x)` needs no quotes in CSS. No `star_blocks`
/// either, in CSS or SCSS: `* {` is the universal selector.
const CSS: Lang = Lang { name: "CSS", block: C_BLOCK, strings: QUOTES, ..BASE };
const SCSS: Lang = Lang { name: "SCSS", line: SLASH, block: C_BLOCK, strings: QUOTES, ..BASE };
const ZIG: Lang = Lang { name: "Zig", line: SLASH, strings: QUOTES, ..BASE };
const PYTHON: Lang = Lang {
    name: "Python",
    line: HASH_MARK,
    strings: &[
        quote(b"\"\"\"", b"\"\"\"", true, true),
        quote(b"'''", b"'''", true, true),
        quote(b"\"", b"\"", false, true),
        quote(b"'", b"'", false, true),
    ],
    doc_strings: true,
    ..BASE
};
const SHELL: Lang = Lang {
    name: "Shell",
    line: HASH_MARK,
    strings: &[quote(b"\"", b"\"", true, true), quote(b"'", b"'", true, false)],
    spaced: true,
    ..BASE
};
const RUBY: Lang = Lang {
    name: "Ruby",
    line: HASH_MARK,
    strings: &[quote(b"\"", b"\"", true, true), quote(b"'", b"'", true, true)],
    ..BASE
};
/// Spaced, for `$#array`.
const PERL: Lang = Lang {
    name: "Perl",
    line: HASH_MARK,
    strings: &[quote(b"\"", b"\"", true, true), quote(b"'", b"'", true, true)],
    spaced: true,
    ..BASE
};
const R: Lang = Lang {
    name: "R",
    line: HASH_MARK,
    strings: &[quote(b"\"", b"\"", true, true), quote(b"'", b"'", true, true)],
    ..BASE
};
/// `#` comments in files of many kinds: Makefile, Dockerfile, .gitignore, .env,
/// requirements.txt, Justfile.
const HASH: Lang = Lang { name: "#", line: HASH_MARK, strings: QUOTES, spaced: true, ..BASE };
const CMAKE: Lang = Lang {
    name: "CMake",
    line: HASH_MARK,
    block: &[(b"#[[", b"]]")],
    strings: &[quote(b"\"", b"\"", true, true)],
    ..BASE
};
const NIX: Lang = Lang {
    name: "Nix",
    line: HASH_MARK,
    block: C_BLOCK,
    strings: &[quote(b"''", b"''", true, false), quote(b"\"", b"\"", true, true)],
    spaced: true,
    ..BASE
};
const TOML: Lang = Lang {
    name: "TOML",
    line: HASH_MARK,
    strings: &[
        quote(b"\"\"\"", b"\"\"\"", true, true),
        quote(b"'''", b"'''", true, false),
        quote(b"\"", b"\"", false, true),
        quote(b"'", b"'", false, false),
    ],
    ..BASE
};
const YAML: Lang = Lang {
    name: "YAML",
    line: HASH_MARK,
    strings: &[quote(b"\"", b"\"", false, true), quote(b"'", b"'", false, false)],
    spaced: true,
    ..BASE
};
const INI: Lang = Lang {
    name: "INI",
    line: &[b"#", b";"],
    strings: &[quote(b"\"", b"\"", false, true)],
    spaced: true,
    ..BASE
};
const GRAPHQL: Lang = Lang {
    name: "GraphQL",
    line: HASH_MARK,
    strings: &[quote(b"\"\"\"", b"\"\"\"", true, true), quote(b"\"", b"\"", false, true)],
    ..BASE
};
const ELIXIR: Lang = Lang {
    name: "Elixir",
    line: HASH_MARK,
    strings: &[
        quote(b"\"\"\"", b"\"\"\"", true, true),
        quote(b"'''", b"'''", true, true),
        quote(b"\"", b"\"", true, true),
        quote(b"'", b"'", true, true),
    ],
    ..BASE
};
const JULIA: Lang = Lang {
    name: "Julia",
    line: HASH_MARK,
    block: &[(b"#=", b"=#")],
    strings: &[quote(b"\"\"\"", b"\"\"\"", true, true), quote(b"\"", b"\"", true, true)],
    ..BASE
};
/// The backtick escapes in PowerShell; a backslash is a plain character (`"C:\"`).
const POWERSHELL: Lang = Lang {
    name: "PowerShell",
    line: HASH_MARK,
    block: &[(b"<#", b"#>")],
    strings: &[quote(b"\"", b"\"", true, false), quote(b"'", b"'", true, false)],
    spaced: true,
    ..BASE
};
const LUA: Lang = Lang {
    name: "Lua",
    line: DASHES,
    block: &[(b"--[[", b"]]")],
    strings: &[quote(b"[[", b"]]", true, false), quote(b"\"", b"\"", false, true), quote(b"'", b"'", false, true)],
    ..BASE
};
/// One line per string, so a quote escaped some other way than SQL's `''` cannot
/// swallow the lines after it.
const SQL: Lang = Lang {
    name: "SQL",
    line: DASHES,
    block: C_BLOCK,
    strings: &[quote(b"'", b"'", false, false), quote(b"\"", b"\"", false, false)],
    ..BASE
};
const HASKELL: Lang = Lang {
    name: "Haskell",
    line: DASHES,
    block: &[(b"{-", b"-}")],
    strings: &[quote(b"\"", b"\"", false, true)],
    ..BASE
};
const ADA: Lang = Lang { name: "Ada", line: DASHES, strings: &[quote(b"\"", b"\"", false, false)], ..BASE };
const LISP: Lang = Lang {
    name: "Lisp",
    line: &[b";"],
    block: &[(b"#|", b"|#")],
    strings: &[quote(b"\"", b"\"", true, true)],
    ..BASE
};
const ERLANG: Lang = Lang { name: "Erlang", line: &[b"%"], strings: &[quote(b"\"", b"\"", false, true)], ..BASE };
/// No strings: an apostrophe in prose would open one.
const TEX: Lang = Lang { name: "TeX", line: &[b"%"], ..BASE };
const FORTRAN: Lang = Lang {
    name: "Fortran",
    line: &[b"!"],
    strings: &[quote(b"\"", b"\"", false, false), quote(b"'", b"'", false, false)],
    ..BASE
};
const FORTRAN_FIXED: Lang = Lang { name: "Fortran (fixed form)", fixed_form: true, ..FORTRAN };
/// VASP's INCAR.
const INCAR: Lang = Lang { name: "INCAR", line: &[b"#", b"!"], ..BASE };
/// Also Markdown, Vue and Svelte. No strings: an apostrophe in prose would open one.
const HTML: Lang = Lang { name: "HTML", block: &[(b"<!--", b"-->")], ..BASE };
const OCAML: Lang = Lang {
    name: "OCaml",
    block: &[(b"(*", b"*)")],
    strings: &[quote(b"\"", b"\"", true, true)],
    ..BASE
};
const FSHARP: Lang = Lang {
    name: "F#",
    line: SLASH,
    block: &[(b"(*", b"*)")],
    strings: &[quote(b"\"\"\"", b"\"\"\"", true, false), quote(b"\"", b"\"", false, true)],
    ..BASE
};
const PASCAL: Lang = Lang {
    name: "Pascal",
    line: SLASH,
    block: &[(b"{", b"}"), (b"(*", b"*)")],
    strings: &[quote(b"'", b"'", false, false)],
    ..BASE
};
const HCL: Lang = Lang {
    name: "HCL",
    line: &[b"#", b"//"],
    block: C_BLOCK,
    strings: &[quote(b"\"", b"\"", false, true)],
    ..BASE
};

/// The language of a file, from its name (`Makefile`, `.zshrc`) or extension, in
/// any case. `None` for a file it does not know, whose diff keeps git's colors.
pub fn lang_for(name: &str) -> Option<&'static Lang> {
    let by_name = match name {
        "Makefile" | "makefile" | "GNUmakefile" | "Dockerfile" | "Containerfile" | "Justfile" | "justfile"
        | "CODEOWNERS" | "requirements.txt" | ".gitignore" | ".gitattributes" | ".dockerignore" | ".npmignore"
        | ".env" => Some(&HASH),
        "CMakeLists.txt" => Some(&CMAKE),
        "Gemfile" | "Rakefile" | "Podfile" | "Brewfile" | "Vagrantfile" | "Fastfile" => Some(&RUBY),
        ".bashrc" | ".bash_profile" | ".bash_logout" | ".zshrc" | ".zshenv" | ".zprofile" | ".zlogin" | ".zlogout"
        | ".profile" | ".envrc" | "PKGBUILD" => Some(&SHELL),
        ".editorconfig" | ".gitconfig" | ".gitmodules" | ".npmrc" | ".inputrc" => Some(&INI),
        "INCAR" => Some(&INCAR),
        _ if name.starts_with("Dockerfile.") || name.starts_with(".env.") => Some(&HASH),
        _ => None,
    };
    if by_name.is_some() {
        return by_name;
    }
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => &RUST,
        "c" | "h" | "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" | "ipp" | "inl" | "ino" | "cu"
        | "cuh" | "mm" | "glsl" | "vert" | "frag" | "geom" | "comp" | "hlsl" | "metal" | "proto" | "sol" | "sv"
        | "svh" | "json" | "jsonc" | "json5" => &C_LIKE,
        "java" | "kt" | "kts" | "scala" | "sc" | "groovy" | "gradle" | "dart" | "swift" => &JAVA_LIKE,
        "cs" | "csx" => &CSHARP,
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => &JS,
        "go" => &GO,
        "php" => &PHP,
        "css" => &CSS,
        "scss" | "sass" | "less" | "styl" => &SCSS,
        "zig" => &ZIG,
        "py" | "pyi" | "pyw" | "pyx" | "pxd" => &PYTHON,
        "sh" | "bash" | "zsh" | "ksh" | "fish" | "bats" => &SHELL,
        "rb" | "rake" | "gemspec" | "ru" | "podspec" => &RUBY,
        "pl" | "pm" => &PERL,
        "r" => &R,
        "mk" | "mak" | "dockerfile" | "just" | "awk" | "tcl" | "gitignore" => &HASH,
        "cmake" => &CMAKE,
        "nix" => &NIX,
        "toml" => &TOML,
        "yml" | "yaml" => &YAML,
        "ini" | "cfg" | "conf" | "properties" | "desktop" | "service" | "ghostty" | "gitconfig" => &INI,
        "graphql" | "gql" => &GRAPHQL,
        "ex" | "exs" => &ELIXIR,
        "jl" => &JULIA,
        "ps1" | "psm1" | "psd1" => &POWERSHELL,
        "lua" => &LUA,
        "sql" => &SQL,
        "hs" | "elm" => &HASKELL,
        "adb" | "ads" | "vhd" | "vhdl" => &ADA,
        "lisp" | "lsp" | "el" | "clj" | "cljs" | "cljc" | "edn" | "scm" | "ss" | "rkt" | "fnl" => &LISP,
        "erl" | "hrl" => &ERLANG,
        "tex" | "sty" | "cls" | "ltx" | "dtx" | "bib" => &TEX,
        "f90" | "f95" | "f03" | "f08" => &FORTRAN,
        "f" | "for" | "ftn" | "f77" => &FORTRAN_FIXED,
        "html" | "htm" | "xhtml" | "xml" | "svg" | "xsl" | "xslt" | "xsd" | "plist" | "vue" | "svelte" | "md"
        | "markdown" | "mdx" | "csproj" | "fsproj" | "vcxproj" => &HTML,
        "ml" | "mli" | "mll" | "mly" => &OCAML,
        "fs" | "fsi" | "fsx" => &FSHARP,
        "pas" | "dpr" | "lpr" => &PASCAL,
        "tf" | "tfvars" | "hcl" => &HCL,
        _ => return None,
    })
}

/// Where the lexer is at the end of a line, for the next one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Code,
    /// In the block comment `block[i]`.
    Block(u8),
    /// In the string `strings[i]`; `doc` for a docstring.
    Str { i: u8, doc: bool },
    /// In a Rust raw string, closed by `"` and this many `#`.
    Raw(u8),
}

impl Lang {
    /// The state inside a `/* … */` block, for a hunk taken to start in one (see
    /// `star_blocks`). `None` for a language that does not guess.
    fn star_block(&self) -> Option<State> {
        if !self.star_blocks {
            return None;
        }
        self.block.iter().position(|(open, _)| *open == b"/*").map(|i| State::Block(i as u8))
    }
}

/// Reads the lines of one side of a hunk in order. A hunk can start anywhere in a
/// file, even inside a comment or a docstring, which this has to guess.
#[derive(Clone, Copy, Debug, Default)]
pub struct Lexer {
    state: State,
    /// A line with text was read.
    started: bool,
    /// That line ended in `:` outside comments, as `def f():` does before its docstring.
    colon: bool,
}

/// What the lines before tell about a triple quote that opens a line (see `code`).
#[derive(Clone, Copy)]
struct Doc {
    colon: bool,
    first: bool,
}

impl Lexer {
    /// The byte ranges of `line` that are comments or docstrings.
    pub fn comments(&mut self, lang: &Lang, line: &[u8]) -> Vec<Range<usize>> {
        let blank = line.iter().all(u8::is_ascii_whitespace);
        let first = !self.started && !blank;
        // A hunk that starts on ` * ` lines starts inside a `/** … */` block.
        let t = line.trim_ascii_start();
        if first
            && self.state == State::Code
            && (t == b"*" || t.starts_with(b"* ") || t.starts_with(b"*/"))
            && let Some(inside) = lang.star_block()
        {
            self.state = inside;
        }
        let out = comments(lang, line, &mut self.state, Doc { colon: self.colon, first });
        if !blank {
            self.started = true;
            let code_end = match out.last() {
                Some(r) if r.end == line.len() => r.start,
                _ => line.len(),
            };
            self.colon = self.state == State::Code && line[..code_end].trim_ascii_end().ends_with(b":");
        }
        out
    }
}

/// The byte ranges of `line` that are comments, or docstrings. `state` comes in
/// from the line before and goes on to the next.
fn comments(lang: &Lang, line: &[u8], state: &mut State, doc: Doc) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if lang.fixed_form && *state == State::Code && line.first().is_some_and(|b| b"cC*!".contains(b)) {
        push(&mut out, 0..line.len());
        return out;
    }
    let mut i = 0;
    while i < line.len() {
        match *state {
            State::Block(k) => {
                let close = lang.block[k as usize].1;
                let end = find(line, i, close).map(|j| j + close.len());
                push(&mut out, i..end.unwrap_or(line.len()));
                if end.is_some() {
                    *state = State::Code;
                }
                i = end.unwrap_or(line.len());
            }
            State::Str { i: k, doc } => {
                let q = &lang.strings[k as usize];
                let end = close_quote(line, i, q);
                if doc {
                    push(&mut out, i..end.unwrap_or(line.len()));
                }
                if end.is_some() || !q.multi {
                    *state = State::Code;
                }
                i = end.unwrap_or(line.len());
            }
            State::Raw(hashes) => {
                let end = close_raw(line, i, hashes as usize);
                if end.is_some() {
                    *state = State::Code;
                }
                i = end.unwrap_or(line.len());
            }
            State::Code => i = code(lang, line, i, state, doc, &mut out),
        }
    }
    // A string that cannot span lines ends with its line, closed or not.
    if let State::Str { i: k, .. } = *state
        && !lang.strings[k as usize].multi
    {
        *state = State::Code;
    }
    out
}

/// One step in code at `i`: a comment or string that opens there, or one byte.
/// Returns where to go on.
fn code(lang: &Lang, line: &[u8], i: usize, state: &mut State, doc: Doc, out: &mut Vec<Range<usize>>) -> usize {
    let rest = &line[i..];
    // A backslash makes the next character plain: `\%` in TeX, `\#` and `\"` in shell.
    if i > 0 && line[i - 1] == b'\\' {
        return i + 1;
    }
    if let Some(k) = lang.block.iter().position(|(open, _)| rest.starts_with(open)) {
        let open = lang.block[k].0;
        push(out, i..i + open.len());
        *state = State::Block(k as u8);
        return i + open.len();
    }
    if lang.line.iter().any(|m| rest.starts_with(m)) && line_comment_at(lang, line, i) {
        push(out, i..line.len());
        return line.len();
    }
    if lang.rust
        && let Some(next) = rust_literal(line, i, state)
    {
        return next;
    }
    if let Some(k) = lang.strings.iter().position(|q| rest.starts_with(q.open)) {
        let q = &lang.strings[k];
        let after = i + q.open.len();
        if lang.doc_strings && q.multi && line[..i].iter().all(u8::is_ascii_whitespace) {
            // A triple quote that opens a line starts, ends or holds a whole
            // docstring. A hunk can start inside one, and its closing quotes taken
            // for an opening would turn the code after them into a comment. So it
            // opens one only after a line ending in `:` (`def f():`) or as the
            // hunk's first line with text after it; else it closes one. A missed
            // docstring reads as code, which is the lesser harm.
            if let Some(end) = close_quote(line, after, q) {
                push(out, i..end);
                return end;
            }
            push(out, i..after);
            if doc.colon || doc.first && !line[after..].trim_ascii().is_empty() {
                *state = State::Str { i: k as u8, doc: true };
            }
            return after;
        }
        *state = State::Str { i: k as u8, doc: false };
        return after;
    }
    i + 1
}

fn line_comment_at(lang: &Lang, line: &[u8], i: usize) -> bool {
    let before = i.checked_sub(1).map(|j| line[j]);
    if lang.spaced && !matches!(before, None | Some(b' ' | b'\t')) {
        return false;
    }
    // `url(http://x)` in SCSS: a scheme, not a comment.
    !(line[i..].starts_with(b"//") && before == Some(b':'))
}

/// A Rust char literal, lifetime or raw string at `i`: where to go on, or `None`
/// for none of them.
fn rust_literal(line: &[u8], i: usize, state: &mut State) -> Option<usize> {
    if line[i] == b'\'' {
        return Some(char_end(line, i));
    }
    let word_start = i == 0 || !is_word(line[i - 1]);
    if !word_start || !matches!(line[i], b'r' | b'b' | b'c') {
        return None;
    }
    let mut j = i;
    if line[j] != b'r' {
        j += 1;
        if line.get(j) != Some(&b'r') {
            return None;
        }
    }
    j += 1;
    let hashes = line[j..].iter().take_while(|&&b| b == b'#').count();
    if line.get(j + hashes) != Some(&b'"') {
        return None;
    }
    *state = State::Raw(hashes.min(u8::MAX as usize) as u8);
    Some(j + hashes + 1)
}

/// Past a char literal (`'x'`, `'字'`, `'\n'`, `'\u{1F600}'`) at `i`, or past the
/// quote alone when it starts a lifetime or a label (`'a`, `'outer:`).
fn char_end(line: &[u8], i: usize) -> usize {
    let rest = &line[i + 1..];
    match rest.first() {
        Some(b'\\') => rest.iter().skip(2).take(10).position(|&b| b == b'\'').map_or(i + 1, |k| i + k + 4),
        Some(&b) => {
            let n = utf8_len(b);
            if rest.get(n) == Some(&b'\'') { i + n + 2 } else { i + 1 }
        }
        None => i + 1,
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn find(line: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    line.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|k| from + k)
}

/// Past the end of a string that is open at `j`, or `None` if it goes on past the line.
fn close_quote(line: &[u8], mut j: usize, q: &Quote) -> Option<usize> {
    while j < line.len() {
        if q.escape && line[j] == b'\\' {
            j += 2;
        } else if line[j..].starts_with(q.close) {
            return Some(j + q.close.len());
        } else {
            j += 1;
        }
    }
    None
}

/// Past the `"` and `hashes` × `#` that close a Rust raw string.
fn close_raw(line: &[u8], mut j: usize, hashes: usize) -> Option<usize> {
    while j < line.len() {
        if line[j] == b'"' && line.get(j + 1..j + 1 + hashes).is_some_and(|h| h.iter().all(|&b| b == b'#')) {
            return Some(j + 1 + hashes);
        }
        j += 1;
    }
    None
}

/// Adds `r`, merged into the range before it when the two touch.
fn push(out: &mut Vec<Range<usize>>, r: Range<usize>) {
    if r.is_empty() {
        return;
    }
    match out.last_mut() {
        Some(last) if last.end == r.start => last.end = r.end,
        _ => out.push(r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The comment text of each line, read in order as one side of a hunk.
    fn noted(name: &str, lines: &[&str]) -> Vec<Vec<String>> {
        let lang = lang_for(name).unwrap_or_else(|| panic!("no language for {name}"));
        let mut lexer = Lexer::default();
        lines
            .iter()
            .map(|l| lexer.comments(lang, l.as_bytes()).into_iter().map(|r| l[r].to_string()).collect())
            .collect()
    }

    /// The comment text of one line.
    fn note(name: &str, line: &str) -> Vec<String> {
        noted(name, &[line]).remove(0)
    }

    #[test]
    fn languages_come_from_names_and_extensions() {
        let name = |f: &str| lang_for(f).map(|l| l.name);
        assert_eq!(name("main.rs"), Some("Rust"));
        assert_eq!(name("App.TSX"), Some("JavaScript"));
        assert_eq!(name("analysis.py"), Some("Python"));
        assert_eq!(name("Makefile"), Some("#"));
        assert_eq!(name("Dockerfile.dev"), Some("#"));
        assert_eq!(name(".env.local"), Some("#"));
        assert_eq!(name(".zshrc"), Some("Shell"));
        assert_eq!(name("config.ghostty"), Some("INI"));
        assert_eq!(name("INCAR"), Some("INCAR"));
        assert_eq!(name("solver.F90"), Some("Fortran"));
        assert_eq!(name("old.f"), Some("Fortran (fixed form)"));
        assert_eq!(name("README.md"), Some("HTML"));
        // Unknown, ambiguous (`.m` is Objective-C or MATLAB) or no extension at all.
        for unknown in ["data.xyz", "model.m", "README", "LICENSE", ".prettierrc", "archive.tar.gz"] {
            assert_eq!(name(unknown), None, "{unknown}");
        }
    }

    #[test]
    fn a_comment_marker_in_a_string_is_no_comment() {
        assert_eq!(note("a.rs", r#"let url = "http://x"; // note"#), ["// note"]);
        assert_eq!(note("a.rs", r#"let s = "a \" // b"; // c"#), ["// c"]);
        assert_eq!(note("a.py", r##"x = "#"  # real"##), ["# real"]);
        assert_eq!(note("a.js", "const s = `// no ${x}`; // yes"), ["// yes"]);
        assert_eq!(note("a.go", "s := `a // b` // c"), ["// c"]);
        assert_eq!(note("a.sql", "select '--' -- c"), ["-- c"]);
        assert_eq!(note("a.lua", "s = [[ -- not ]] -- yes"), ["-- yes"]);
        assert_eq!(note("a.f90", "print *, 'it''s ! not' ! c"), ["! c"]);
        assert_eq!(note("a.c", r#"char q = '"'; // c"#), ["// c"]);
    }

    #[test]
    fn rust_lifetimes_attributes_and_raw_strings() {
        assert_eq!(note("a.rs", "#[derive(Debug)]"), Vec::<String>::new());
        assert_eq!(note("a.rs", r#"fn f<'a>(x: &'a str) -> char { '"' } // c"#), ["// c"]);
        assert_eq!(note("a.rs", r"let c = '\''; let d = '\u{1F600}'; // c"), ["// c"]);
        assert_eq!(note("a.rs", "'outer: loop { break 'outer; } // c"), ["// c"]);
        assert_eq!(note("a.rs", "let z = '字'; // c"), ["// c"]);
        assert_eq!(note("a.rs", r##"let r = r#"a // "b" "#; // c"##), ["// c"]);
        assert_eq!(note("a.rs", r#"let p = r"C:\"; // c"#), ["// c"]);
        assert_eq!(note("a.rs", r#"let b = br"x" ; for x in y {} // c"#), ["// c"]);
        assert_eq!(note("a.rs", "/// doc"), ["/// doc"]);
        // Rust strings go on over lines.
        let lines = noted("a.rs", &[r#"let s = "abc"#, r#"// not a comment";"#, "// yes"]);
        assert_eq!(lines, [vec![], vec![], vec!["// yes".to_string()]]);
    }

    #[test]
    fn block_comments_go_on_over_lines() {
        let lines = noted("a.c", &["int x; /* start", "still */ int y; // end", "int z;"]);
        assert_eq!(lines, [vec!["/* start"], vec!["still */", "// end"], vec![]]);
        let lines = noted("a.lua", &["--[[ block", "]] x = 1 -- c"]);
        assert_eq!(lines, [vec!["--[[ block"], vec!["]]", "-- c"]]);
        let lines = noted("index.html", &["<p>don't</p> <!-- a", "b --> <b>"]);
        assert_eq!(lines, [vec!["<!-- a"], vec!["b -->"]]);
        // A string that cannot span lines ends with its line, so the next is lexed fresh.
        assert_eq!(noted("a.c", &[r#"char *s = "abc"#, "// c"]), [vec![], vec!["// c".to_string()]]);
    }

    #[test]
    fn a_marker_that_is_not_a_comment() {
        // Shell and YAML want a blank before `#`.
        assert_eq!(note("a.sh", r##"echo $# "#" a#b \# # c"##), ["# c"]);
        assert_eq!(note("a.sh", "# full line"), ["# full line"]);
        assert_eq!(note("a.yaml", "url: http://a#b # c"), ["# c"]);
        assert_eq!(note("a.toml", "k = 'a#b' # c"), ["# c"]);
        // An unquoted URL: no line comments in CSS, and `://` is no comment in SCSS.
        assert_eq!(note("a.css", "a { b: url(http://x/y.png) } /* c */"), ["/* c */"]);
        assert_eq!(note("a.scss", "b: url(http://x/y.png); // c"), ["// c"]);
        assert_eq!(note("a.tex", r"50\% done % c"), ["% c"]);
        assert_eq!(note("INCAR", "ENCUT = 520 ! eV"), ["! eV"]);
    }

    #[test]
    fn python_docstrings_read_as_comments() {
        // Text after the quotes on the hunk's first line: an opening.
        let lines = noted("a.py", &[r#"    """Doc with # hash"#, r#"    more """"#, "x = 1  # c", r#"s = """not doc # x""""#]);
        assert_eq!(lines, [vec![r#""""Doc with # hash"#], vec![r#"    more """"#], vec!["# c"], vec![]]);
        // After `def f():`, bare quotes open one too.
        let lines = noted("a.py", &["def f():", r#"    """"#, "    Sum # of both.", r#"    """"#, "    return a"]);
        assert_eq!(lines, [vec![], vec![r#"""""#], vec!["    Sum # of both."], vec![r#"    """"#], vec![]]);
        // A docstring on one line, here one of an attribute, after no `:`.
        assert_eq!(noted("a.py", &["x = 1", r#""""Doc for x.""""#]), [vec![], vec![r#""""Doc for x.""""#]]);
    }

    #[test]
    fn a_hunk_that_starts_inside_a_docstring_keeps_its_code() {
        // The bare quotes close the docstring that began above the hunk. Taken for
        // an opening, they would turn every line after them into a comment.
        let lines = noted("a.py", &["    Returns the sum.", r#"    """"#, "    x = 1", "    y = 2  # c"]);
        assert_eq!(lines, [vec![], vec![r#"""""#], vec![], vec!["# c"]]);
        // Even when the line before ends with a colon inside the docstring.
        let lines = noted("a.py", &["    Args:", "        a: one.", r#"    """"#, "    return a"]);
        assert_eq!(lines, [vec![], vec![], vec![r#"""""#], vec![]]);
    }

    #[test]
    fn fixed_form_fortran_comments_in_column_one() {
        assert_eq!(note("a.f", "C     comment"), ["C     comment"]);
        assert_eq!(note("a.f", "*     comment"), ["*     comment"]);
        assert_eq!(note("a.f", "      CALL FOO ! c"), ["! c"]);
        // Free form has no column rule.
        assert_eq!(note("a.f90", "call foo"), Vec::<String>::new());
    }

    #[test]
    fn star_blocks_are_guessed_only_where_doc_blocks_are_common() {
        assert!(lang_for("a.js").unwrap().star_block().is_some());
        assert!(lang_for("a.java").unwrap().star_block().is_some());
        // rustfmt starts continued lines with `* b`, and `* {` is CSS's universal selector.
        for name in ["a.rs", "a.py", "a.css", "a.scss"] {
            assert!(lang_for(name).unwrap().star_block().is_none(), "{name}");
        }
        assert_eq!(noted("a.js", &[" * Returns the sum.", " */", "function f() {}"]), [vec![" * Returns the sum."], vec![" */"], vec![]]);
        assert_eq!(noted("a.css", &["* { margin: 0; }", "a { b: c; }"]), [Vec::<&str>::new(), vec![]]);
    }
}

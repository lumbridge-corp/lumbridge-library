//! Code-aware lexical terms. Postgres' `simple` parser keeps `StreamableHttpService` and
//! `crates/ignore/src/gitignore.rs` as single tokens, so the indexer stores a normalised text per
//! chunk (`wh.chunk_terms`): symbol name / parent words (weight A), path segments and signature
//! identifiers (C), words of compound identifiers in the content (D), each with a crude stem.
//! Queries go through the same splitting and stemming.

use std::collections::HashSet;

/// Cap on the words of one chunk's search text (A weight) and identifier text (C weight).
const MAX_SEARCH_WORDS: usize = 160;
const MAX_IDENT_WORDS: usize = 300;
/// OR terms per query.
pub const MAX_QUERY_TERMS: usize = 32;

/// Split an identifier or path into lowercase words: `/ . _ - :` and camel case boundaries
/// (`StreamableHttpService` → streamable, http, service; `HTTPServer2` → http, server2).
pub fn split_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in s.split(|c: char| !c.is_alphanumeric()) {
        if part.is_empty() {
            continue;
        }
        let chars: Vec<char> = part.chars().collect();
        let mut cur = String::new();
        for i in 0..chars.len() {
            let c = chars[i];
            if i > 0 && !cur.is_empty() {
                let prev = chars[i - 1];
                let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
                let boundary = (c.is_uppercase() && (prev.is_lowercase() || prev.is_ascii_digit()))
                    || (c.is_uppercase() && prev.is_uppercase() && next_lower);
                if boundary {
                    out.push(std::mem::take(&mut cur).to_lowercase());
                }
            }
            cur.push(c);
        }
        if !cur.is_empty() {
            out.push(cur.to_lowercase());
        }
    }
    out
}

/// A crude, symmetric suffix stripper (applied to both indexed words and query terms):
/// matching / matched / matches → match, stealing → steal, parse / parsing → pars.
pub fn stem(w: &str) -> String {
    let n = w.chars().count();
    if n <= 4 || !w.is_ascii() {
        return w.to_string();
    }
    let mut s = w;
    for suf in ["ing", "ed"] {
        if let Some(r) = s.strip_suffix(suf)
            && r.len() >= 3
        {
            s = r;
            // stopped → stopp → stop
            let b = s.as_bytes();
            if b.len() >= 4 && b[b.len() - 1] == b[b.len() - 2] && !matches!(b[b.len() - 1], b'l' | b's' | b'z') {
                s = &s[..s.len() - 1];
            }
            return trim_e(s).to_string();
        }
    }
    if let Some(r) = ["ches", "shes", "sses", "xes"].iter().find_map(|suf| s.strip_suffix(suf).map(|r| (r, suf))) {
        let (r, suf) = r;
        return format!("{r}{}", &suf[..suf.len() - 2]);
    }
    if let Some(r) = s.strip_suffix("ies")
        && r.len() >= 3
    {
        return format!("{r}y");
    }
    if let Some(r) = s.strip_suffix('s')
        && !r.ends_with('s')
        && !r.ends_with('u')
        && !r.ends_with('i')
    {
        s = r;
    }
    trim_e(s).to_string()
}

fn trim_e(s: &str) -> &str {
    if s.len() > 4 && s.ends_with('e') { &s[..s.len() - 1] } else { s }
}

/// Words that say nothing about code location (English glue + language keywords).
pub fn is_stopword(w: &str) -> bool {
    const STOP: &[&str] = &[
        "a", "an", "the", "and", "or", "of", "to", "in", "on", "for", "with", "by", "at", "from", "as", "is", "are",
        "was", "be", "it", "its", "this", "that", "these", "those", "how", "what", "where", "which", "who", "why",
        "when", "does", "do", "did", "can", "could", "should", "would", "will", "into", "via", "use", "used", "using",
        "uses", "find", "show", "me", "my", "we", "our", "you", "your", "there", "here", "about", "between", "get",
        "implementation", "implemented", "implement", "implements", "code", "work", "works", "working", "handle",
        "handled", "handles", "handling", "pub", "fn", "let", "mut", "self", "impl", "where", "crate", "super",
        "mod", "use", "struct", "enum", "trait", "type", "const", "static", "async", "await", "dyn", "ref", "return",
        "def", "func", "function", "class", "var", "rs", "src", "lib", "some", "none", "ok", "err", "true", "false",
        "if", "else", "not", "no", "yes", "all", "any", "one",
    ];
    STOP.contains(&w)
}

fn is_keyword(w: &str) -> bool {
    matches!(
        w,
        "pub" | "fn" | "let" | "mut" | "self" | "impl" | "where" | "crate" | "super" | "mod" | "struct" | "enum"
            | "trait" | "type" | "const" | "static" | "async" | "await" | "dyn" | "ref" | "return" | "unsafe"
            | "extern" | "def" | "func" | "function" | "class" | "var" | "export" | "public" | "private"
            | "protected" | "void" | "new" | "in" | "for" | "as" | "str" | "u8" | "u16" | "u32" | "u64" | "usize"
            | "i32" | "i64" | "isize" | "bool" | "option" | "result" | "string" | "vec" | "box" | "arc" | "t"
    )
}

/// Accumulates distinct words (and their stems) up to a cap.
struct Words {
    seen: HashSet<String>,
    out: Vec<String>,
    cap: usize,
}

impl Words {
    fn new(cap: usize) -> Self {
        Self { seen: HashSet::new(), out: Vec::new(), cap }
    }
    fn push(&mut self, w: &str) {
        if self.out.len() >= self.cap || w.is_empty() || w.len() > 60 {
            return;
        }
        if self.seen.insert(w.to_string()) {
            self.out.push(w.to_string());
        }
        let s = stem(w);
        if s != w && self.out.len() < self.cap && self.seen.insert(s.clone()) {
            self.out.push(s);
        }
    }
    /// The identifier itself (lowercased, as the `simple` parser would see it) plus its words.
    fn ident(&mut self, ident: &str, skip_keywords: bool) {
        let parts = split_words(ident);
        if parts.len() > 1 {
            let whole: String = ident.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
            self.push(&whole);
        }
        for p in parts {
            if skip_keywords && is_keyword(&p) {
                continue;
            }
            self.push(&p);
        }
    }
    fn text(self) -> String {
        self.out.join(" ")
    }
}

/// Identifiers in free text (`[A-Za-z_][A-Za-z0-9_]*`).
fn identifiers(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| w.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_'))
}

/// The A-weight text of a chunk: words of the symbol's name and parent (`StreamableHttpService`
/// → streamablehttpservice streamable http service), with stems.
pub fn search_text(sym_name: Option<&str>, parent: Option<&str>) -> String {
    let mut w = Words::new(MAX_SEARCH_WORDS);
    for n in [sym_name, parent].into_iter().flatten() {
        w.ident(n, false);
    }
    w.text()
}

/// The C-weight text of a chunk: path segments split into words, plus the identifiers of the
/// symbol's signature (keywords dropped).
pub fn path_text(path: &str, signature: Option<&str>) -> String {
    let mut w = Words::new(MAX_SEARCH_WORDS);
    for seg in path.split('/') {
        w.ident(seg, false);
    }
    if let Some(sig) = signature {
        for id in identifiers(sig) {
            w.ident(id, true);
        }
    }
    w.text()
}

/// The D-weight text of a chunk: words of compound identifiers in its content (`camelCase`,
/// `PascalCase`, `snake_case`), which the `simple` parser keeps whole or splits without stems.
pub fn ident_text(content: &str) -> String {
    let mut w = Words::new(MAX_IDENT_WORDS);
    for id in identifiers(content) {
        if id.len() < 4 || id.len() > 80 {
            continue;
        }
        let parts = split_words(id);
        if parts.len() < 2 {
            continue;
        }
        for p in parts {
            if p.len() >= 3 && !is_keyword(&p) {
                w.push(&p);
            }
        }
    }
    w.text()
}

/// One query term: the word as typed (lowercased) and its stem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub word: String,
    pub stem: String,
}

impl Term {
    /// tsquery fragment: `(word | stem)` (terms are [a-z0-9] only, so always valid).
    pub fn tsquery(&self, weights: &str) -> String {
        let w = if weights.is_empty() { String::new() } else { format!(":{weights}") };
        if self.stem == self.word { format!("{}{w}", self.word) } else { format!("({}{w} | {}{w})", self.word, self.stem) }
    }
}

/// Distinct search terms of a question: identifiers and paths split into words, stopwords and
/// one-letter words dropped, compound identifiers also kept whole. At most [`MAX_QUERY_TERMS`].
pub fn query_terms(q: &str, drop: &HashSet<String>) -> Vec<Term> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut add = |w: String, out: &mut Vec<Term>| {
        if out.len() >= MAX_QUERY_TERMS || w.chars().count() < 2 || w.len() > 60 || is_stopword(&w) || drop.contains(&w) {
            return;
        }
        if !w.chars().all(char::is_alphanumeric) {
            return;
        }
        if seen.insert(w.clone()) {
            let stem = stem(&w);
            out.push(Term { word: w, stem });
        }
    };
    for tok in q.split_whitespace() {
        let tok = tok.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_'));
        if tok.is_empty() {
            continue;
        }
        let parts = split_words(tok);
        if parts.len() > 1 && identifiers(tok).count() == 1 && !tok.contains('-') {
            // `StreamableHttpService` / `read_to_end`: the whole identifier too.
            add(tok.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase(), &mut out);
        }
        for p in parts {
            add(p, &mut out);
        }
    }
    out
}

/// Identifier-looking tokens of a question, for the symbol leg: `(name, prefix_ok)`. Compound or
/// code-shaped tokens (`StreamableHttpService`, `read_to_end`, `query!`, `tokio::spawn`) are
/// matched exactly and by prefix; plain words exactly (case-insensitive), and by prefix when long.
pub fn symbol_candidates(q: &str, drop: &HashSet<String>) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let push = |n: String, prefix: bool, out: &mut Vec<(String, bool)>| {
        if n.chars().count() < 3 || out.len() >= 16 {
            return;
        }
        let l = n.to_lowercase();
        if is_stopword(&l) || drop.contains(&l) {
            return;
        }
        if let Some(e) = out.iter_mut().find(|e| e.0 == l) {
            e.1 |= prefix;
        } else {
            out.push((l, prefix));
        }
    };
    for tok in q.split_whitespace() {
        let tok = tok.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '!'));
        let tok = tok.trim_end_matches(['!', '(', ')']);
        if tok.is_empty() {
            continue;
        }
        // `tokio::spawn`, `Gitignore::matched`: the last segment.
        let last = tok.rsplit("::").next().unwrap_or(tok).trim_matches(|c: char| !(c.is_alphanumeric() || c == '_'));
        if last.is_empty() || !last.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') {
            continue;
        }
        if !last.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let code_shaped = tok.contains("::")
            || last.contains('_')
            || last.chars().skip(1).any(|c| c.is_uppercase())
            || last.chars().any(|c| c.is_ascii_digit());
        let long = last.chars().count() >= 6;
        push(last.to_string(), code_shaped || long, &mut out);
        if !code_shaped {
            let s = stem(&last.to_lowercase());
            if s != last.to_lowercase() && s.len() >= 4 {
                push(s, true, &mut out);
            }
        }
    }
    out
}

/// Why a chunk is probably not what a question wants. Returns a score multiplier (1.0 = fine):
/// tests, examples, benches, docs / README / CHANGELOG and manifests unless asked for, chunks
/// under 3 lines, and chunks that are only imports / module declarations.
pub fn noise_factor(path: &str, line_start: i32, line_end: i32, content: &str, wants: &Wants) -> f32 {
    let mut f = 1.0f32;
    match path_kind(path) {
        PathKind::Test if !wants.tests => f *= 0.5,
        PathKind::Example if !wants.examples => f *= 0.55,
        PathKind::Bench if !wants.benches => f *= 0.5,
        PathKind::Doc if !wants.docs => f *= 0.5,
        PathKind::Config if !wants.config => f *= 0.6,
        _ => {}
    }
    if line_end - line_start + 1 < 3 {
        f *= 0.6;
    }
    if !wants.imports && imports_only(content) {
        f *= 0.5;
    }
    f
}

/// A chunk made of `use` / `import` / `mod` / `#include` lines (plus comments and attributes),
/// including the continuation lines of a multi-line `use a::{...}` / `import {...}`.
pub fn imports_only(content: &str) -> bool {
    let (mut code, mut imports) = (0usize, 0usize);
    let mut in_block = false;
    for l in content.lines() {
        let t = l.trim();
        if t.is_empty() || t.starts_with("//") || (t.starts_with('#') && !t.starts_with("#include")) || t.starts_with("/*") || t.starts_with('*') {
            continue;
        }
        code += 1;
        if in_block {
            imports += 1;
            if t.contains('}') || t.contains(')') {
                in_block = false;
            }
            continue;
        }
        let t = t.strip_prefix("pub ").or_else(|| t.strip_prefix("pub(crate) ")).unwrap_or(t);
        let starts = ["use ", "mod ", "import ", "from ", "#include", "extern crate ", "package ", "export * from", "export {"];
        if starts.iter().any(|p| t.starts_with(p)) {
            imports += 1;
            in_block = (t.contains('{') && !t.contains('}')) || (t.starts_with("from ") && t.ends_with('('));
        }
    }
    code > 0 && imports * 10 >= code * 8
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Source,
    Test,
    Example,
    Bench,
    Doc,
    /// Manifests and config (toml / yaml / json / lock).
    Config,
}

/// Coarse role of a file by its path.
pub fn path_kind(path: &str) -> PathKind {
    let l = path.to_ascii_lowercase();
    let mut dirs: Vec<&str> = l.split('/').collect();
    let file = dirs.pop().unwrap_or("");
    let stem = file.split('.').next().unwrap_or(file);
    if dirs.iter().any(|d| matches!(*d, "tests" | "test" | "testing" | "__tests__" | "spec" | "specs" | "test-utils" | "test_utils" | "testsuite"))
        || stem.ends_with("_test")
        || stem.ends_with("_tests")
        || stem.starts_with("test_")
        || stem == "tests"
        || stem == "test"
        || file.contains(".test.")
        || file.contains(".spec.")
    {
        return PathKind::Test;
    }
    if dirs.iter().any(|d| matches!(*d, "benches" | "bench" | "benchmarks" | "benchmark")) {
        return PathKind::Bench;
    }
    if dirs.iter().any(|d| matches!(*d, "examples" | "example" | "demo" | "demos" | "samples")) {
        return PathKind::Example;
    }
    let doc_file = file.ends_with(".md")
        || file.ends_with(".markdown")
        || file.ends_with(".mdx")
        || file.ends_with(".rst")
        || file.ends_with(".txt")
        || file.ends_with(".adoc")
        || stem == "readme"
        || stem == "changelog"
        || stem == "changes"
        || stem == "history";
    if doc_file || dirs.iter().any(|d| matches!(*d, "doc" | "docs" | "documentation")) {
        return PathKind::Doc;
    }
    if [".toml", ".yaml", ".yml", ".json", ".lock", ".ini", ".cfg"].iter().any(|e| file.ends_with(e)) {
        return PathKind::Config;
    }
    PathKind::Source
}

/// Which usually-noisy kinds of file a question explicitly asks for.
#[derive(Debug, Clone, Copy, Default)]
pub struct Wants {
    pub tests: bool,
    pub examples: bool,
    pub benches: bool,
    pub docs: bool,
    pub config: bool,
    pub imports: bool,
}

impl Wants {
    pub fn from_query(q: &str) -> Self {
        let mut w = Wants::default();
        for t in split_words(q) {
            match t.as_str() {
                "test" | "tests" | "testing" | "unittest" | "spec" | "specs" => w.tests = true,
                "example" | "examples" | "demo" | "demos" | "sample" | "usage" | "tutorial" => w.examples = true,
                "bench" | "benches" | "benchmark" | "benchmarks" => w.benches = true,
                "doc" | "docs" | "documentation" | "readme" | "changelog" | "guide" | "markdown" => w.docs = true,
                "cargo" | "toml" | "manifest" | "dependency" | "dependencies" | "feature" | "features" | "config"
                | "configuration" | "yaml" | "json" | "package" | "workspace" => w.config = true,
                "import" | "imports" | "reexport" | "reexports" | "use" | "module" | "modules" => w.imports = true,
                _ => {}
            }
        }
        w
    }
}

/// Cut `s` to at most `max` chars on a char boundary.
pub fn cap_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_identifiers_and_paths() {
        assert_eq!(split_words("StreamableHttpService"), ["streamable", "http", "service"]);
        assert_eq!(split_words("HTTPServer"), ["http", "server"]);
        assert_eq!(split_words("read_to_end"), ["read", "to", "end"]);
        assert_eq!(split_words("crates/ignore/src/gitignore.rs"), ["crates", "ignore", "src", "gitignore", "rs"]);
        assert_eq!(split_words("sqlx-macros-core"), ["sqlx", "macros", "core"]);
        assert_eq!(split_words("utf8Decoder"), ["utf8", "decoder"]);
    }

    #[test]
    fn stems_are_symmetric() {
        assert_eq!(stem("matching"), "match");
        assert_eq!(stem("matched"), "match");
        assert_eq!(stem("matches"), "match");
        assert_eq!(stem("stealing"), "steal");
        assert_eq!(stem("parsing"), stem("parse"));
        assert_eq!(stem("stopped"), "stop");
        assert_eq!(stem("bytes"), "byte");
        assert_eq!(stem("entries"), "entry");
        assert_eq!(stem("status"), "status");
        assert_eq!(stem("http"), "http");
    }

    #[test]
    fn chunk_texts_have_path_and_symbol_words() {
        let p = path_text(
            "crates/rmcp/src/transport/streamable_http_server/tower.rs",
            Some("pub struct StreamableHttpService<S, M = LocalSessionManager>"),
        );
        for w in ["rmcp", "transport", "streamable", "http", "server", "tower", "session", "manager"] {
            assert!(p.split(' ').any(|x| x == w), "{w} missing from {p}");
        }
        assert!(!p.split(' ').any(|x| x == "pub"));
        let t = search_text(Some("StreamableHttpService"), None);
        assert_eq!(t, "streamablehttpservice streamablehttpservic streamable streamabl http service servic");
        let t = search_text(Some("matched"), Some("Gitignore"));
        assert!(t.contains("gitignore") && t.contains("match"), "{t}");
        let i = ident_text("let svc = StreamableHttpService::new(cfg); read_to_end(x)");
        assert!(i.contains("streamable") && i.contains("service") && i.contains("read"), "{i}");
    }

    #[test]
    fn query_terms_and_symbols() {
        let none = HashSet::new();
        let t: Vec<String> = query_terms("How does ripgrep handle gitignore matching?", &none).into_iter().map(|t| t.word).collect();
        assert_eq!(t, ["ripgrep", "gitignore", "matching"]);
        let drop: HashSet<String> = ["ripgrep".to_string()].into();
        let t = query_terms("ripgrep gitignore matching", &drop);
        assert_eq!(t[1].tsquery(""), "(matching | match)");
        assert_eq!(t[0].tsquery("A"), "(gitignore:A | gitignor:A)");
        let t: Vec<String> = query_terms("StreamableHttpService in crates/rmcp/src", &none).into_iter().map(|t| t.word).collect();
        assert_eq!(t, ["streamablehttpservice", "streamable", "http", "service", "crates", "rmcp"]);
        let many = (0..100).map(|i| format!("word{i}")).collect::<Vec<_>>().join(" ");
        assert_eq!(query_terms(&many, &none).len(), MAX_QUERY_TERMS);
        let s = symbol_candidates("sqlx query! macro expansion via tokio::spawn and StreamableHttpService", &none);
        assert!(s.contains(&("query".into(), false)), "{s:?}");
        assert!(s.contains(&("spawn".into(), true)), "{s:?}");
        assert!(s.contains(&("streamablehttpservice".into(), true)), "{s:?}");
        assert!(s.contains(&("expansion".into(), true)), "{s:?}");
        let s = symbol_candidates("tokio work stealing", &none);
        assert!(s.contains(&("steal".into(), true)), "{s:?}");
    }

    #[test]
    fn noise() {
        let w = Wants::default();
        assert_eq!(path_kind("crates/ignore/src/gitignore.rs"), PathKind::Source);
        assert_eq!(path_kind("tests/tests.rs"), PathKind::Test);
        assert_eq!(path_kind("src/foo_test.go"), PathKind::Test);
        assert_eq!(path_kind("examples/simple.rs"), PathKind::Example);
        assert_eq!(path_kind("benches/bench.rs"), PathKind::Bench);
        assert_eq!(path_kind("CHANGELOG.md"), PathKind::Doc);
        assert_eq!(path_kind("docs/guide/intro.rs"), PathKind::Doc);
        let body = "fn f() {\n    g();\n}";
        assert_eq!(noise_factor("src/lib.rs", 1, 10, body, &w), 1.0);
        assert!(noise_factor("src/lib.rs", 4, 5, body, &w) < 1.0);
        assert!(noise_factor("tests/a.rs", 1, 10, body, &w) < 1.0);
        assert!(noise_factor("Cargo.toml", 1, 10, "[package]\nname = \"x\"", &w) < 1.0);
        let wx = Wants::from_query("show me an example of a server");
        assert!(wx.examples && !wx.tests);
        assert_eq!(noise_factor("examples/a.rs", 1, 10, body, &wx), 1.0);
        assert!(imports_only("use std::fmt;\nuse crate::{\n    a,\n    b,\n};\npub mod tower;\n"));
        assert!(!imports_only("pub struct Config {\n    pub a: bool,\n    pub b: u32,\n    c: String,\n}\n"));
        assert!(!imports_only("use std::fmt;\nfn main() {\n    println!(\"x\");\n    let y = 1;\n}\n"));
        assert_eq!(cap_chars("héllo", 2), "hé");
    }
}

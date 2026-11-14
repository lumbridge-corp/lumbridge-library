//! tree-sitter symbols, references and whole-block chunks (probe-style).
//!
//! One chunk per function / method / impl / class. Blocks over [`MAX_BLOCK_TOKENS`] are split into
//! their child symbols, or on line boundaries with a small overlap. Lines no symbol covers (imports,
//! top-level statements) are chunked by line windows. Markdown is chunked by headings.

use super::lang::Lang;
use crate::rank::approx_tokens;
use std::collections::{HashMap, HashSet};
use tree_sitter::{Node, Parser};

pub const MAX_BLOCK_TOKENS: usize = 1500;
pub const LEFTOVER_TOKENS: usize = 600;
pub const DOC_TOKENS: usize = 900;
pub const OVERLAP_LINES: usize = 3;
const MAX_REFS_PER_FILE: usize = 3000;
const SIG_MAX: usize = 300;
const DOC_MAX: usize = 1000;

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: &'static str,
    pub parent: Option<String>,
    pub parent_idx: Option<usize>,
    pub signature: Option<String>,
    pub doc: Option<String>,
    /// 1-based, inclusive.
    pub line_start: usize,
    pub line_end: usize,
    /// First line of the chunk (doc comments and attributes included).
    pub chunk_start: usize,
}

#[derive(Debug, Clone)]
pub struct Ref {
    pub name: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub line_start: usize,
    pub line_end: usize,
    pub content: String,
    pub symbol_idx: Option<usize>,
}

#[derive(Debug, Default)]
pub struct Parsed {
    pub symbols: Vec<Symbol>,
    pub refs: Vec<Ref>,
    pub chunks: Vec<Chunk>,
    pub lines: usize,
}

/// Parsers are created lazily, one per grammar, and reused across files.
#[derive(Default)]
pub struct Parsers {
    map: HashMap<&'static str, Parser>,
}

fn language(lang: Lang) -> Option<tree_sitter::Language> {
    Some(match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
        Lang::C => tree_sitter_c::LANGUAGE.into(),
        Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Lang::Bash => tree_sitter_bash::LANGUAGE.into(),
        Lang::Java => tree_sitter_java::LANGUAGE.into(),
        _ => return None,
    })
}

impl Parsers {
    pub fn parse_file(&mut self, lang: Lang, text: &str) -> Parsed {
        let lines = Lines::new(text);
        let mut parsed = match lang {
            Lang::Markdown => Parsed { chunks: markdown_chunks(&lines), ..Default::default() },
            Lang::Text | Lang::Other(_) => {
                let t = if matches!(lang, Lang::Text) { DOC_TOKENS } else { LEFTOVER_TOKENS + 200 };
                Parsed { chunks: windows(&lines, 1, lines.count(), t, 0, None), ..Default::default() }
            }
            _ => self.parse_code(lang, text, &lines),
        };
        parsed.lines = lines.count();
        parsed
    }

    fn parse_code(&mut self, lang: Lang, text: &str, lines: &Lines) -> Parsed {
        let Some(language) = language(lang) else { return Parsed::default() };
        let parser = self.map.entry(lang.name()).or_insert_with(|| {
            let mut p = Parser::new();
            p.set_language(&language).expect("grammar ABI compatible with tree-sitter");
            p
        });
        let Some(tree) = parser.parse(text, None) else {
            return Parsed { chunks: windows(lines, 1, lines.count(), LEFTOVER_TOKENS, 0, None), ..Default::default() };
        };
        let mut w = Walker { lang, src: text.as_bytes(), symbols: Vec::new(), refs: Vec::new(), seen_refs: HashSet::new() };
        w.walk(tree.root_node());
        let chunks = plan_chunks(&w.symbols, lines);
        Parsed { symbols: w.symbols, refs: w.refs, chunks, lines: 0 }
    }
}

// ───────────────────────────── lines ─────────────────────────────

pub struct Lines<'a> {
    text: &'a str,
    /// Byte offset of the start of each line, plus text.len() at the end.
    starts: Vec<usize>,
}

impl<'a> Lines<'a> {
    pub fn new(text: &'a str) -> Self {
        let mut starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' && i + 1 < text.len() {
                starts.push(i + 1);
            }
        }
        starts.push(text.len());
        if text.is_empty() {
            starts = vec![0, 0];
        }
        Self { text, starts }
    }
    pub fn count(&self) -> usize {
        if self.text.is_empty() { 0 } else { self.starts.len() - 1 }
    }
    /// Lines lo..=hi (1-based) without the final newline.
    pub fn slice(&self, lo: usize, hi: usize) -> &'a str {
        let n = self.count();
        if n == 0 || lo > hi || lo > n {
            return "";
        }
        let hi = hi.min(n);
        let s = &self.text[self.starts[lo - 1]..self.starts[hi]];
        s.strip_suffix('\n').unwrap_or(s)
    }
    pub fn line(&self, i: usize) -> &'a str {
        self.slice(i, i)
    }
}

// ───────────────────────────── walker ─────────────────────────────

struct Walker<'s> {
    lang: Lang,
    src: &'s [u8],
    symbols: Vec<Symbol>,
    refs: Vec<Ref>,
    seen_refs: HashSet<(String, usize)>,
}

struct Def<'t> {
    kind: &'static str,
    name: String,
    /// Node whose range is the symbol (export / decorator / template wrapper included).
    outer: Node<'t>,
    /// Where the signature starts.
    sig_start: usize,
    /// Where the signature ends (start of the body), if known.
    body: Option<Node<'t>>,
    /// Override for the parent name (Go receivers, C++ `Foo::bar`, Rust impl type).
    parent: Option<String>,
}

fn is_comment(kind: &str) -> bool {
    matches!(kind, "comment" | "line_comment" | "block_comment")
}

/// Symbol kinds whose bodies may hold further symbols.
fn holds_symbols(kind: &str) -> bool {
    !matches!(kind, "function" | "method" | "macro" | "const" | "static" | "type")
}

impl<'s> Walker<'s> {
    fn text(&self, n: Node) -> &'s str {
        n.utf8_text(self.src).unwrap_or("")
    }

    fn field_text(&self, n: Node, f: &str) -> Option<String> {
        n.child_by_field_name(f).map(|c| self.text(c).to_string())
    }

    fn walk(&mut self, root: Node) {
        // Explicit stack: deep ASTs must not overflow the async task's stack.
        let mut stack: Vec<(Node, Option<usize>, bool)> = vec![(root, None, true)];
        while let Some((node, parent_sym, may_define)) = stack.pop() {
            self.collect_ref(node);
            let mut cur_parent = parent_sym;
            let mut child_may_define = may_define;
            // Nameless definitions (anonymous C++ structs, odd declarators) are useless for lookup.
            if may_define && let Some(def) = self.classify(node, parent_sym).filter(|d| !d.name.trim().is_empty()) {
                let idx = self.push_symbol(def, parent_sym);
                cur_parent = Some(idx);
                child_may_define = holds_symbols(self.symbols[idx].kind);
            }
            let mut cursor = node.walk();
            let children: Vec<Node> = node.named_children(&mut cursor).collect();
            for c in children.into_iter().rev() {
                stack.push((c, cur_parent, child_may_define));
            }
        }
        self.symbols.sort_by_key(|s| (s.line_start, std::cmp::Reverse(s.line_end)));
        // parent_idx were assigned before sorting; rebuild them from the parent chain order.
        self.fix_parent_indices();
    }

    fn fix_parent_indices(&mut self) {
        // Re-derive parent_idx by containment: nearest preceding symbol that encloses this one and
        // is a container.
        let n = self.symbols.len();
        let mut open: Vec<usize> = Vec::new();
        for i in 0..n {
            let (ls, le) = (self.symbols[i].line_start, self.symbols[i].line_end);
            while let Some(&top) = open.last() {
                if self.symbols[top].line_end >= le && self.symbols[top].line_start <= ls && top != i {
                    break;
                }
                open.pop();
            }
            let had_parent = self.symbols[i].parent_idx.is_some();
            self.symbols[i].parent_idx = if had_parent { open.last().copied() } else { None };
            if holds_symbols(self.symbols[i].kind) {
                open.push(i);
            }
        }
    }

    fn push_symbol(&mut self, d: Def, parent_sym: Option<usize>) -> usize {
        let parent = d.parent.clone().or_else(|| parent_sym.map(|p| self.symbols[p].name.clone()));
        let sig_end = d.body.map(|b| b.start_byte()).unwrap_or(d.outer.end_byte());
        let raw_sig = std::str::from_utf8(&self.src[d.sig_start.min(sig_end)..sig_end]).unwrap_or("");
        let signature = clean_signature(raw_sig, d.body.is_none());
        let (doc, doc_start) = self.doc_for(d.outer, d.body);
        let line_start = d.outer.start_position().row + 1;
        let mut line_end = d.outer.end_position().row + 1;
        if d.outer.end_position().column == 0 && line_end > line_start {
            line_end -= 1;
        }
        let chunk_start = doc_start.map_or(line_start, |s| s.min(line_start));
        self.symbols.push(Symbol {
            name: d.name,
            kind: d.kind,
            parent,
            parent_idx: parent_sym,
            signature,
            doc,
            line_start,
            line_end,
            chunk_start,
        });
        self.symbols.len() - 1
    }

    /// Doc comment text and the first line it (or attributes) start on.
    fn doc_for(&self, outer: Node, body: Option<Node>) -> (Option<String>, Option<usize>) {
        // Python docstring: first statement in the body.
        if matches!(self.lang, Lang::Python)
            && let Some(b) = body
            && let Some(first) = b.named_child(0)
            && first.kind() == "expression_statement"
            && let Some(s) = first.named_child(0)
            && s.kind() == "string"
        {
            let doc = clean_doc(self.text(s), self.lang);
            let start = self.preceding_block_start(outer).map(|(_, l)| l);
            return (doc, start);
        }
        match self.preceding_block_start(outer) {
            Some((texts, start)) => (clean_doc(&texts, self.lang), Some(start)),
            None => (None, None),
        }
    }

    /// Contiguous comments / attributes directly above `outer`: (joined comment text, first line).
    fn preceding_block_start(&self, outer: Node) -> Option<(String, usize)> {
        let mut texts: Vec<&str> = Vec::new();
        let mut start: Option<usize> = None;
        let mut cur_row = outer.start_position().row;
        let mut sib = outer.prev_named_sibling();
        while let Some(s) = sib {
            let end_row = s.end_position().row;
            let end_row = if s.end_position().column == 0 && end_row > s.start_position().row { end_row - 1 } else { end_row };
            if end_row + 1 < cur_row {
                break;
            }
            let k = s.kind();
            if is_comment(k) {
                let t = self.text(s);
                if matches!(self.lang, Lang::Rust) && !(t.starts_with("///") && !t.starts_with("////") || t.starts_with("/**")) {
                    break;
                }
                let low = t.to_ascii_lowercase();
                if low.contains("copyright") || low.contains("spdx-license") {
                    break;
                }
                texts.push(t.trim_end());
            } else if !matches!(k, "attribute_item" | "decorator" | "annotation" | "marker_annotation") {
                break;
            }
            start = Some(s.start_position().row + 1);
            cur_row = s.start_position().row;
            sib = s.prev_named_sibling();
        }
        let start = start?;
        texts.reverse();
        Some((texts.join("\n"), start))
    }

    fn classify<'t>(&self, n: Node<'t>, parent_sym: Option<usize>) -> Option<Def<'t>> {
        let parent_kind = parent_sym.map(|p| self.symbols[p].kind);
        let in_type = matches!(parent_kind, Some("impl" | "trait" | "class" | "interface" | "struct" | "enum"));
        let k = n.kind();
        let mk = |kind: &'static str, name: String, body: Option<Node<'t>>| Def {
            kind,
            name,
            outer: n,
            sig_start: n.start_byte(),
            body,
            parent: None,
        };
        let name = |f: &str| self.field_text(n, f);
        let body = n.child_by_field_name("body");
        match self.lang {
            Lang::Rust => {
                let kind = match k {
                    "function_item" | "function_signature_item" => {
                        if in_type { "method" } else { "function" }
                    }
                    "struct_item" | "union_item" => "struct",
                    "enum_item" => "enum",
                    "trait_item" => "trait",
                    "impl_item" => {
                        let ty = name("type")?;
                        let ty = strip_generics(&ty).to_string();
                        return Some(mk("impl", ty, body));
                    }
                    "const_item" => "const",
                    "static_item" => "static",
                    "macro_definition" => "macro",
                    "mod_item" => "module",
                    "type_item" => "type",
                    _ => return None,
                };
                Some(mk(kind, name("name")?, body))
            }
            Lang::Python => {
                let kind = match k {
                    "function_definition" => {
                        if parent_kind == Some("class") { "method" } else { "function" }
                    }
                    "class_definition" => "class",
                    _ => return None,
                };
                let mut d = mk(kind, name("name")?, body);
                if let Some(p) = n.parent()
                    && p.kind() == "decorated_definition"
                {
                    d.outer = p;
                }
                Some(d)
            }
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
                let mut d = match k {
                    "function_declaration" | "generator_function_declaration" => mk("function", name("name")?, body),
                    "class_declaration" | "abstract_class_declaration" => mk("class", name("name")?, body),
                    "method_definition" => mk("method", name("name")?, body),
                    "interface_declaration" => mk("interface", name("name")?, body),
                    "type_alias_declaration" => mk("type", name("name")?, None),
                    "enum_declaration" => mk("enum", name("name")?, body),
                    "internal_module" | "module" => mk("module", name("name")?, body),
                    "lexical_declaration" | "variable_declaration" => {
                        if in_type || matches!(parent_kind, Some("function" | "method")) {
                            return None;
                        }
                        let mut c = n.walk();
                        let decl = n.named_children(&mut c).find(|d| {
                            d.kind() == "variable_declarator"
                                && d.child_by_field_name("value").is_some_and(|v| {
                                    matches!(v.kind(), "arrow_function" | "function_expression" | "function" | "generator_function")
                                })
                        })?;
                        let fname = self.field_text(decl, "name")?;
                        let fbody = decl.child_by_field_name("value").and_then(|v| v.child_by_field_name("body"));
                        mk("function", fname, fbody)
                    }
                    _ => return None,
                };
                if let Some(p) = n.parent()
                    && p.kind() == "export_statement"
                {
                    d.outer = p;
                    d.sig_start = p.start_byte();
                }
                Some(d)
            }
            Lang::Go => match k {
                "function_declaration" => Some(mk("function", name("name")?, body)),
                "method_declaration" => {
                    let mut d = mk("method", name("name")?, body);
                    d.parent = n.child_by_field_name("receiver").and_then(|r| {
                        let t = self.text(r);
                        // "(s *Server)" → "Server"
                        let inner = t.trim_matches(|c| c == '(' || c == ')');
                        let ty = inner.split_whitespace().last()?;
                        Some(strip_generics(ty.trim_start_matches('*')).to_string())
                    });
                    Some(d)
                }
                "type_spec" | "type_alias" => {
                    let ty = n.child_by_field_name("type");
                    let kind = match ty.map(|t| t.kind()) {
                        Some("struct_type") => "struct",
                        Some("interface_type") => "interface",
                        _ => "type",
                    };
                    let mut d = mk(kind, name("name")?, None);
                    if let Some(p) = n.parent()
                        && p.kind() == "type_declaration"
                        && p.named_child_count() == 1
                    {
                        d.outer = p;
                        d.sig_start = p.start_byte();
                    }
                    Some(d)
                }
                _ => None,
            },
            Lang::C | Lang::Cpp => {
                let mut d = match k {
                    "function_definition" => {
                        let decl = n.child_by_field_name("declarator")?;
                        let (fname, scope) = self.c_declarator_name(decl)?;
                        let kind = if in_type || scope.is_some() { "method" } else { "function" };
                        let mut d = mk(kind, fname, body);
                        d.parent = scope;
                        d
                    }
                    "struct_specifier" | "union_specifier" | "enum_specifier" | "class_specifier" => {
                        body?;
                        if n.parent().is_some_and(|p| p.kind() == "type_definition") {
                            return None; // handled by the typedef
                        }
                        let kind = match k {
                            "enum_specifier" => "enum",
                            "class_specifier" => "class",
                            _ => "struct",
                        };
                        let mut d = mk(kind, name("name")?, body);
                        // `struct foo { ... };` lives in a declaration; include the semicolon line.
                        if let Some(p) = n.parent()
                            && p.kind() == "declaration"
                            && p.named_child_count() == 1
                        {
                            d.outer = p;
                        }
                        d
                    }
                    "type_definition" => {
                        let ty = n.child_by_field_name("type");
                        let tname = name("declarator").map(|s| s.trim_start_matches('*').to_string())?;
                        match ty {
                            Some(t) if t.child_by_field_name("body").is_some() => {
                                let kind = if t.kind() == "enum_specifier" { "enum" } else { "struct" };
                                let nm = self.field_text(t, "name").unwrap_or(tname);
                                mk(kind, nm, t.child_by_field_name("body"))
                            }
                            _ => mk("type", tname, None),
                        }
                    }
                    "namespace_definition" => mk("module", name("name")?, body),
                    "preproc_function_def" => mk("macro", name("name")?, n.child_by_field_name("value")),
                    _ => return None,
                };
                if let Some(p) = n.parent()
                    && p.kind() == "template_declaration"
                {
                    d.outer = p;
                    d.sig_start = p.start_byte();
                }
                Some(d)
            }
            Lang::Bash => match k {
                "function_definition" => Some(mk("function", name("name")?, body)),
                _ => None,
            },
            Lang::Java => {
                let kind = match k {
                    "class_declaration" | "record_declaration" => "class",
                    "interface_declaration" | "annotation_type_declaration" => "interface",
                    "enum_declaration" => "enum",
                    "method_declaration" | "constructor_declaration" | "compact_constructor_declaration" => "method",
                    _ => return None,
                };
                Some(mk(kind, name("name")?, body))
            }
            _ => None,
        }
    }

    /// C/C++ declarator chain → (name, qualifying scope).
    fn c_declarator_name(&self, mut d: Node) -> Option<(String, Option<String>)> {
        for _ in 0..8 {
            match d.kind() {
                "identifier" | "field_identifier" | "destructor_name" | "operator_name" | "type_identifier" => {
                    return Some((self.text(d).to_string(), None));
                }
                "qualified_identifier" => {
                    let scope = self.field_text(d, "scope");
                    let name = d.child_by_field_name("name")?;
                    let (n, _) = self.c_declarator_name(name)?;
                    return Some((n, scope.map(|s| strip_generics(&s).to_string())));
                }
                _ => d = d.child_by_field_name("declarator")?,
            }
        }
        None
    }

    fn collect_ref(&mut self, n: Node) {
        if self.refs.len() >= MAX_REFS_PER_FILE {
            return;
        }
        let k = n.kind();
        let target = match k {
            "call_expression" | "call" => n.child_by_field_name("function"),
            "macro_invocation" => n.child_by_field_name("macro"),
            "method_invocation" => n.child_by_field_name("name"),
            "new_expression" => n.child_by_field_name("constructor"),
            "object_creation_expression" => n.child_by_field_name("type"),
            "type_identifier" => {
                // Skip the name node of a definition.
                if n.parent().and_then(|p| p.child_by_field_name("name")).is_some_and(|x| x.id() == n.id()) {
                    return;
                }
                Some(n)
            }
            _ => None,
        };
        let Some(t) = target else { return };
        let Some(name) = self.ref_name(t, 0) else { return };
        if name.len() < 2 || name.len() > 100 {
            return;
        }
        let line = n.start_position().row + 1;
        if self.seen_refs.insert((name.to_string(), line)) {
            self.refs.push(Ref { name: name.to_string(), line });
        }
    }

    fn ref_name(&self, n: Node, depth: u8) -> Option<&'s str> {
        if depth > 4 {
            return None;
        }
        match n.kind() {
            "identifier" | "field_identifier" | "property_identifier" | "type_identifier" => Some(self.text(n)),
            "scoped_identifier" | "qualified_identifier" | "template_function" | "scoped_type_identifier" => {
                self.ref_name(n.child_by_field_name("name")?, depth + 1)
            }
            "field_expression" => self.ref_name(n.child_by_field_name("field")?, depth + 1),
            "member_expression" => self.ref_name(n.child_by_field_name("property")?, depth + 1),
            "selector_expression" => self.ref_name(n.child_by_field_name("field")?, depth + 1),
            "attribute" => self.ref_name(n.child_by_field_name("attribute")?, depth + 1),
            "generic_function" | "generic_type" => {
                let inner = n.child_by_field_name("function").or_else(|| n.child_by_field_name("type"))?;
                self.ref_name(inner, depth + 1)
            }
            _ => None,
        }
    }
}

fn strip_generics(s: &str) -> &str {
    let s = s.trim();
    let s = s.split('<').next().unwrap_or(s);
    s.split('[').next().unwrap_or(s).trim()
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn clean_signature(raw: &str, no_body: bool) -> Option<String> {
    let mut s = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    // Multi-line parameter lists: "( a: u8, b: u8, )" → "(a: u8, b: u8)".
    for (from, to) in [("( ", "("), (", )", ")"), (" )", ")"), ("[ ", "["), (" ]", "]")] {
        while s.contains(from) {
            s = s.replace(from, to);
        }
    }
    if no_body {
        // No body node (const, type alias): keep the first line-ish up to `{` / `=`... but whole
        // short declarations read best, so only cut at a `{`.
        if let Some(i) = s.find('{') {
            s.truncate(i);
        }
    }
    let s = s.trim().trim_end_matches(['{', ':', ';']).trim_end().to_string();
    if s.is_empty() { None } else { Some(truncate_chars(&s, SIG_MAX)) }
}

fn clean_doc(raw: &str, lang: Lang) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    let raw = raw.trim();
    let raw = if matches!(lang, Lang::Python) {
        let r = raw.trim_start_matches(['r', 'b', 'u', 'R', 'B', 'U']);
        r.trim_start_matches("\"\"\"")
            .trim_end_matches("\"\"\"")
            .trim_start_matches("'''")
            .trim_end_matches("'''")
            .trim_matches(['"', '\''])
    } else {
        raw
    };
    for line in raw.lines() {
        let mut l = line.trim();
        for p in ["///", "//!", "//", "/**", "/*!", "/*", "#"] {
            if let Some(r) = l.strip_prefix(p) {
                l = r;
                break;
            }
        }
        l = l.trim_end_matches("*/");
        let l = l.trim_start();
        let l = l.strip_prefix("* ").unwrap_or(if l == "*" { "" } else { l });
        out.push(l.trim_end().to_string());
    }
    let s = out.join("\n").trim().to_string();
    if s.is_empty() { None } else { Some(truncate_chars(&s, DOC_MAX)) }
}

// ───────────────────────────── chunking ─────────────────────────────

fn plan_chunks(symbols: &[Symbol], lines: &Lines) -> Vec<Chunk> {
    let n = lines.count();
    let mut covered = vec![false; n + 2];
    let mut chunks = Vec::new();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); symbols.len()];
    let mut tops = Vec::new();
    for (i, s) in symbols.iter().enumerate() {
        match s.parent_idx {
            Some(p) => children[p].push(i),
            None => tops.push(i),
        }
    }
    for i in tops {
        emit_symbol(i, symbols, &children, lines, &mut covered, &mut chunks);
    }
    // Leftovers: contiguous uncovered runs.
    let mut i = 1;
    while i <= n {
        if covered[i] {
            i += 1;
            continue;
        }
        let lo = i;
        while i <= n && !covered[i] {
            i += 1;
        }
        chunks.extend(windows(lines, lo, i - 1, LEFTOVER_TOKENS, 0, None));
    }
    chunks.sort_by_key(|c| (c.line_start, c.line_end));
    chunks
}

fn emit_symbol(
    i: usize,
    symbols: &[Symbol],
    children: &[Vec<usize>],
    lines: &Lines,
    covered: &mut [bool],
    out: &mut Vec<Chunk>,
) {
    let s = &symbols[i];
    let (lo, hi) = (s.chunk_start, s.line_end.min(lines.count()));
    if lo > hi || (lo..=hi).all(|l| covered[l]) {
        return;
    }
    let text = lines.slice(lo, hi);
    if approx_tokens(text) <= MAX_BLOCK_TOKENS {
        out.push(Chunk { line_start: lo, line_end: hi, content: text.to_string(), symbol_idx: Some(i) });
        covered[lo..=hi].iter_mut().for_each(|c| *c = true);
        return;
    }
    if children[i].is_empty() {
        out.extend(windows(lines, lo, hi, MAX_BLOCK_TOKENS, OVERLAP_LINES, Some(i)));
        covered[lo..=hi].iter_mut().for_each(|c| *c = true);
        return;
    }
    for &c in &children[i] {
        emit_symbol(c, symbols, children, lines, covered, out);
    }
    // The container's own lines (header, fields) not taken by children.
    let mut l = lo;
    while l <= hi {
        if covered[l] {
            l += 1;
            continue;
        }
        let a = l;
        while l <= hi && !covered[l] {
            l += 1;
        }
        out.extend(windows(lines, a, l - 1, MAX_BLOCK_TOKENS, 0, Some(i)));
    }
    covered[lo..=hi].iter_mut().for_each(|c| *c = true);
}

fn substantive(s: &str) -> bool {
    s.chars().filter(|c| c.is_alphanumeric()).count() >= 12
}

/// Line windows over lo..=hi of at most `max_tokens`, with `overlap` lines carried over.
pub fn windows(lines: &Lines, lo: usize, hi: usize, max_tokens: usize, overlap: usize, sym: Option<usize>) -> Vec<Chunk> {
    let mut out = Vec::new();
    let hi = hi.min(lines.count());
    let mut start = lo;
    while start <= hi {
        // Skip leading blank lines.
        while start <= hi && lines.line(start).trim().is_empty() {
            start += 1;
        }
        if start > hi {
            break;
        }
        let mut end = start;
        let mut toks = approx_tokens(lines.line(start)) + 1;
        while end < hi {
            let t = approx_tokens(lines.line(end + 1)) + 1;
            if toks + t > max_tokens {
                break;
            }
            toks += t;
            end += 1;
        }
        let mut e = end;
        while e > start && lines.line(e).trim().is_empty() {
            e -= 1;
        }
        let content = lines.slice(start, e);
        if substantive(content) {
            out.push(Chunk { line_start: start, line_end: e, content: content.to_string(), symbol_idx: sym });
        }
        if end >= hi {
            break;
        }
        start = if overlap > 0 && end + 1 > start + overlap { end + 1 - overlap } else { end + 1 };
    }
    out
}

/// Markdown: sections by heading (outside code fences), merged up to ~DOC_TOKENS, big ones split.
fn markdown_chunks(lines: &Lines) -> Vec<Chunk> {
    let n = lines.count();
    let mut sections: Vec<(usize, usize)> = Vec::new();
    let mut in_fence = false;
    let mut start = 1;
    for i in 1..=n {
        let l = lines.line(i).trim_start();
        if l.starts_with("```") || l.starts_with("~~~") {
            in_fence = !in_fence;
        }
        if !in_fence && l.starts_with('#') && i > start {
            sections.push((start, i - 1));
            start = i;
        }
    }
    if n >= start {
        sections.push((start, n));
    }
    let mut out = Vec::new();
    let mut acc: Option<(usize, usize, usize)> = None; // lo, hi, tokens
    for (lo, hi) in sections {
        let t = approx_tokens(lines.slice(lo, hi));
        if t > DOC_TOKENS {
            if let Some((a, b, _)) = acc.take() {
                out.extend(windows(lines, a, b, DOC_TOKENS, 0, None));
            }
            out.extend(windows(lines, lo, hi, DOC_TOKENS, 1, None));
            continue;
        }
        acc = match acc {
            Some((a, _, at)) if at + t <= DOC_TOKENS => Some((a, hi, at + t)),
            Some((a, b, _)) => {
                out.extend(windows(lines, a, b, DOC_TOKENS, 0, None));
                Some((lo, hi, t))
            }
            None => Some((lo, hi, t)),
        };
    }
    if let Some((a, b, _)) = acc {
        out.extend(windows(lines, a, b, DOC_TOKENS, 0, None));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(lang: Lang, src: &str) -> Parsed {
        Parsers::default().parse_file(lang, src)
    }

    #[test]
    fn rust_symbols_docs_and_chunks() {
        let src = r#"use std::fmt;

/// A point.
#[derive(Debug)]
pub struct Point { x: i32, y: i32 }

impl Point {
    /// Make one.
    pub fn new(x: i32, y: i32) -> Self {
        Point { x, y }
    }
    fn norm(&self) -> i32 { helper(self.x) + self.y }
}

fn helper(v: i32) -> i32 { v * 2 }

macro_rules! sq { ($x:expr) => { $x * $x }; }
const LIMIT: usize = 10;
"#;
        let p = parse(Lang::Rust, src);
        let names: Vec<(&str, &str)> = p.symbols.iter().map(|s| (s.name.as_str(), s.kind)).collect();
        assert!(names.contains(&("Point", "struct")), "{names:?}");
        assert!(names.contains(&("Point", "impl")));
        assert!(names.contains(&("new", "method")));
        assert!(names.contains(&("helper", "function")));
        assert!(names.contains(&("sq", "macro")));
        assert!(names.contains(&("LIMIT", "const")));
        assert_eq!(clean_signature("pub fn f(\n    a: u8,\n    b: u8,\n) -> u8 ", false).as_deref(), Some("pub fn f(a: u8, b: u8) -> u8"));
        let st = p.symbols.iter().find(|s| s.kind == "struct").unwrap();
        assert_eq!(st.doc.as_deref(), Some("A point."));
        assert_eq!(st.chunk_start, 3);
        assert_eq!(st.line_start, 5);
        let new = p.symbols.iter().find(|s| s.name == "new").unwrap();
        assert_eq!(new.parent.as_deref(), Some("Point"));
        assert_eq!(new.signature.as_deref(), Some("pub fn new(x: i32, y: i32) -> Self"));
        assert_eq!(new.doc.as_deref(), Some("Make one."));
        assert!(p.refs.iter().any(|r| r.name == "helper" && r.line == 12));
        // impl is small: one chunk for the whole impl.
        let imp = p.symbols.iter().position(|s| s.kind == "impl").unwrap();
        assert!(p.chunks.iter().any(|c| c.symbol_idx == Some(imp) && c.line_start == 7 && c.line_end == 13));
        // `use std::fmt;` is a leftover window (too small to be substantive) — no chunk.
        assert!(p.chunks.iter().all(|c| c.line_start > 1));
    }

    #[test]
    fn python_and_ts() {
        let py = "import os\n\nclass Store:\n    \"\"\"Holds things.\"\"\"\n    @property\n    def size(self):\n        return len(self.items)\n\ndef load(path: str) -> Store:\n    '''Load a store.'''\n    return Store()\n";
        let p = parse(Lang::Python, py);
        let st = p.symbols.iter().find(|s| s.name == "Store").unwrap();
        assert_eq!(st.kind, "class");
        assert_eq!(st.doc.as_deref(), Some("Holds things."));
        let size = p.symbols.iter().find(|s| s.name == "size").unwrap();
        assert_eq!(size.kind, "method");
        assert_eq!(size.line_start, 5); // decorator included
        let load = p.symbols.iter().find(|s| s.name == "load").unwrap();
        assert_eq!(load.signature.as_deref(), Some("def load(path: str) -> Store"));
        assert!(p.refs.iter().any(|r| r.name == "Store"));

        let ts = "/** Adds. */\nexport function add(a: number, b: number): number {\n  return a + b;\n}\nexport const mul = (a: number) => a * 2;\ninterface Shape { area(): number }\nclass Sq implements Shape {\n  area(): number { return add(1, 2); }\n}\n";
        let p = parse(Lang::TypeScript, ts);
        let add = p.symbols.iter().find(|s| s.name == "add").unwrap();
        assert_eq!(add.doc.as_deref(), Some("Adds."));
        assert_eq!(add.signature.as_deref(), Some("export function add(a: number, b: number): number"));
        assert!(p.symbols.iter().any(|s| s.name == "mul" && s.kind == "function"));
        assert!(p.symbols.iter().any(|s| s.name == "Shape" && s.kind == "interface"));
        assert!(p.symbols.iter().any(|s| s.name == "area" && s.kind == "method" && s.parent.as_deref() == Some("Sq")));
    }

    #[test]
    fn go_c_cpp() {
        let go = "package x\n\n// Server serves.\ntype Server struct { n int }\n\nfunc (s *Server) Run() error { return nil }\n\nfunc New() *Server { return &Server{} }\n";
        let p = parse(Lang::Go, go);
        assert!(p.symbols.iter().any(|s| s.name == "Server" && s.kind == "struct" && s.doc.as_deref() == Some("Server serves.")));
        assert!(p.symbols.iter().any(|s| s.name == "Run" && s.kind == "method" && s.parent.as_deref() == Some("Server")));
        let c = "typedef struct { int a; } pair_t;\nstatic int add(int a, int b) { return a + b; }\n#define SQ(x) ((x)*(x))\n";
        let p = parse(Lang::C, c);
        assert!(p.symbols.iter().any(|s| s.name == "add" && s.kind == "function"), "{:?}", p.symbols);
        assert!(p.symbols.iter().any(|s| s.name == "pair_t" && s.kind == "struct"));
        assert!(p.symbols.iter().any(|s| s.name == "SQ" && s.kind == "macro"));
        let cpp = "namespace n {\nclass A { public: int f() { return 1; } };\nint A::g() { return 2; }\n}\n";
        let p = parse(Lang::Cpp, cpp);
        assert!(p.symbols.iter().any(|s| s.name == "A" && s.kind == "class"));
        assert!(p.symbols.iter().any(|s| s.name == "f" && s.kind == "method"));
        assert!(p.symbols.iter().any(|s| s.name == "g" && s.kind == "method" && s.parent.as_deref() == Some("A")));
    }

    #[test]
    fn big_function_splits_with_overlap() {
        let mut src = String::from("fn big() {\n");
        for i in 0..800 {
            src.push_str(&format!("    let value_{i} = compute_something({i}) + other_thing({i});\n"));
        }
        src.push_str("}\n");
        let p = parse(Lang::Rust, &src);
        assert!(p.chunks.len() >= 3);
        assert!(p.chunks.iter().all(|c| approx_tokens(&c.content) <= MAX_BLOCK_TOKENS + 50));
        assert!(p.chunks.windows(2).all(|w| w[1].line_start <= w[0].line_end + 1));
        assert!(p.chunks.windows(2).any(|w| w[1].line_start <= w[0].line_end)); // overlap
    }

    #[test]
    fn no_nameless_symbols() {
        // Broken C++ makes tree-sitter insert zero-width MISSING identifiers; those never become symbols.
        let cpp = "class { int a; };\nint (int x) { return x; }\nstruct S { void (); };\nnamespace { int f() { return 1; } }\n";
        let p = parse(Lang::Cpp, cpp);
        assert!(p.symbols.iter().all(|s| !s.name.trim().is_empty()), "{:?}", p.symbols);
        let p = parse(Lang::C, "int (void) { return 0; }\ntypedef struct { int a; } ;\n");
        assert!(p.symbols.iter().all(|s| !s.name.trim().is_empty()), "{:?}", p.symbols);
    }

    #[test]
    fn markdown_by_heading() {
        let md = "# Title\n\nIntro paragraph with enough words.\n\n## Usage\n\n```\n# not a heading\n```\nMore text here for usage.\n";
        let p = parse(Lang::Markdown, md);
        assert_eq!(p.chunks.len(), 1); // small sections merge
        assert_eq!(p.chunks[0].line_start, 1);
    }
}

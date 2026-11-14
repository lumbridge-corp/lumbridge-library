//! Repo card (aider's repo-map idea): a defs/refs graph between files, PageRank over it, and a
//! markdown card with the top files and their most-referenced symbols.

use super::git;
use super::license::LicenseInfo;
use anyhow::Result;
use sqlx::PgPool;
use std::collections::HashMap;
use std::fmt::Write;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

pub type SummaryFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

/// Hook for an LLM-written summary at the top of the card. lumbridge-libraryd does not call an LLM yet;
/// pass one in [`super::IndexOpts::summarizer`] when it should.
pub trait CardSummarizer: Send + Sync {
    /// Gets the generated card (without summary); returns a short paragraph.
    fn summarize<'a>(&'a self, slug: &'a str, card_md: &'a str) -> SummaryFuture<'a>;
}

pub(crate) struct RepoMeta {
    pub slug: String,
    pub tier: String,
    pub origin_url: Option<String>,
    pub default_branch: String,
    pub head: String,
    pub committed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub license: LicenseInfo,
    pub manifests: Vec<Manifest>,
}

pub(crate) struct BuiltCard {
    pub card_md: String,
    pub description: Option<String>,
}

const TOP_FILES: usize = 15;

/// (file_id, name, kind, signature, line_start)
type CardSym = (i64, String, String, Option<String>, i32);
const SYMS_PER_FILE: i64 = 6;
const MAX_MODULES: usize = 20;

/// One line of the card's module map.
#[derive(Debug, PartialEq)]
pub(crate) struct Module {
    pub key: String,
    pub krate: Option<String>,
    pub files: usize,
    pub central: Option<String>,
}

fn is_prose(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    [".md", ".txt", ".rst", ".toml", ".json", ".yml", ".yaml", ".lock"].iter().any(|e| l.ends_with(e))
}

/// Top-level directories / crates with their file counts and most central (PageRank) file.
/// `ranked` is best-first. Crate directories (non-root manifests) win over top-level directories.
pub(crate) fn module_map(ranked: &[(i64, String, f64)], manifests: &[Manifest]) -> Vec<Module> {
    let mut crate_dirs: Vec<&Manifest> = manifests.iter().filter(|m| !m.dir.is_empty()).collect();
    crate_dirs.sort_by_key(|m| std::cmp::Reverse(m.dir.len()));
    let mut mods: Vec<Module> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (_, path, _) in ranked {
        let (key, krate) = match crate_dirs.iter().find(|m| path.starts_with(&format!("{}/", m.dir))) {
            Some(m) => (m.dir.clone(), Some(m.name.clone())),
            None => match path.split_once('/') {
                Some((top, _)) => (top.to_string(), None),
                None => continue, // root files: README, Cargo.toml, ...
            },
        };
        if is_aux(&format!("{key}/x")) || key.starts_with('.') {
            continue;
        }
        let i = *index.entry(key.clone()).or_insert_with(|| {
            mods.push(Module { key, krate, files: 0, central: None });
            mods.len() - 1
        });
        let m = &mut mods[i];
        m.files += 1;
        if m.central.is_none() && !is_prose(path) && !is_aux(path) {
            m.central = Some(path.clone());
        }
    }
    mods.sort_by(|a, b| b.files.cmp(&a.files).then_with(|| a.key.cmp(&b.key)));
    mods.truncate(MAX_MODULES);
    mods
}

/// PageRank with uniform teleport; dangling mass spread evenly. `edges`: (src, dst, weight).
pub fn pagerank(n: usize, edges: &[(usize, usize, f64)], iters: usize, damping: f64) -> Vec<f64> {
    if n == 0 {
        return vec![];
    }
    let mut out_w = vec![0.0; n];
    for &(s, _, w) in edges {
        out_w[s] += w;
    }
    let mut r = vec![1.0 / n as f64; n];
    for _ in 0..iters {
        let dangling: f64 = (0..n).filter(|&i| out_w[i] == 0.0).map(|i| r[i]).sum();
        let base = (1.0 - damping) / n as f64 + damping * dangling / n as f64;
        let mut next = vec![base; n];
        for &(s, d, w) in edges {
            next[d] += damping * r[s] * w / out_w[s];
        }
        let delta: f64 = next.iter().zip(&r).map(|(a, b)| (a - b).abs()).sum();
        r = next;
        if delta < 1e-9 {
            break;
        }
    }
    r
}

/// Rank the repo's files. Returns (file_id, path, score) best-first.
pub(crate) async fn rank_files(pool: &PgPool, repo_id: &str) -> Result<Vec<(i64, String, f64)>> {
    let files: Vec<(i64, String)> = sqlx::query_as("select id, path from wh.files where repo_id = $1::uuid order by id")
        .bind(repo_id)
        .fetch_all(pool)
        .await?;
    // A reference to a name defined in k files gives each of them weight 1/k; names defined in
    // more than 10 files (new, default, Error...) say little about structure and are dropped.
    let edges: Vec<(i64, i64, f64)> = sqlx::query_as(
        "with d as (
           select name, file_id, count(*) over (partition by name) as ndef
             from (select distinct name, file_id from wh.symbols
                    where repo_id = $1::uuid and kind not in ('impl', 'module')) x)
         select r.file_id, d.file_id, sum(1.0 / d.ndef)::float8
           from wh.refs r join d on d.name = r.name
          where r.repo_id = $1::uuid and r.file_id <> d.file_id and d.ndef <= 10
          group by 1, 2
          order by 3 desc
          limit 500000",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?;
    let idx: HashMap<i64, usize> = files.iter().enumerate().map(|(i, (id, _))| (*id, i)).collect();
    let e: Vec<(usize, usize, f64)> = edges
        .into_iter()
        .filter_map(|(s, d, w)| Some((*idx.get(&s)?, *idx.get(&d)?, w.sqrt())))
        .collect();
    let pr = pagerank(files.len(), &e, 50, 0.85);
    let mut out: Vec<(i64, String, f64)> = files.into_iter().zip(pr).map(|((id, p), s)| (id, p, s)).collect();
    out.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.1.len().cmp(&b.1.len())));
    Ok(out)
}

fn is_noise_line(l: &str) -> bool {
    let t = l.trim();
    t.is_empty()
        || t.starts_with('#')
        || t.starts_with("[![")
        || t.starts_with("![")
        || t.starts_with('<')
        || t.starts_with("[!")
        || t.starts_with("---")
        || t.starts_with("===")
        || t.starts_with("```")
        || t.starts_with('|')
        || (t.starts_with('[') && t.contains("]: "))
}

/// Inline markdown to plain text: `**b**` / `__b__` → b, `[t](u)` / `[t][r]` → t, backticks dropped.
fn strip_inline_md(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == '[' {
            // [text](url) or [text][ref]
            if let Some(close) = b[i + 1..].iter().position(|&x| x == ']').map(|p| p + i + 1)
                && let Some(&open) = b.get(close + 1)
                && (open == '(' || open == '[')
            {
                let end_ch = if open == '(' { ')' } else { ']' };
                if let Some(end) = b[close + 2..].iter().position(|&x| x == end_ch).map(|p| p + close + 2) {
                    out.extend(&b[i + 1..close]);
                    i = end + 1;
                    continue;
                }
            }
        }
        if (c == '*' || c == '_') && b.get(i + 1) == Some(&c) {
            i += 2;
            continue;
        }
        if c == '`' {
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn list_item(l: &str) -> Option<&str> {
    let t = l.trim_start();
    for m in ["* ", "- ", "+ "] {
        if let Some(r) = t.strip_prefix(m) {
            return Some(r);
        }
    }
    let digits = t.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && (t[digits..].starts_with(". ") || t[digits..].starts_with(") ")) {
        return Some(&t[digits + 2..]);
    }
    None
}

/// First prose paragraph of a README (markdown stripped). A paragraph that introduces a list
/// ("It is:") takes the list's items along instead of ending on the colon.
pub fn readme_paragraph(readme: &str) -> Option<String> {
    let mut para: Vec<&str> = Vec::new();
    let mut in_fence = false;
    let lines: Vec<&str> = readme.lines().collect();
    let mut end = lines.len();
    for (i, l) in lines.iter().enumerate() {
        if l.trim_start().starts_with("```") {
            in_fence = !in_fence;
            if !para.is_empty() {
                end = i;
                break;
            }
            continue;
        }
        if in_fence {
            continue;
        }
        if is_noise_line(l) || (para.is_empty() && list_item(l).is_some()) {
            if !para.is_empty() {
                end = i;
                break;
            }
            continue;
        }
        para.push(l.trim());
    }
    let mut p = strip_inline_md(&para.join(" "));
    if p.ends_with(':') {
        // The list right after: items (with their continuation lines), until prose or a heading.
        let mut items: Vec<String> = Vec::new();
        let mut cur: Option<String> = None;
        for l in lines.iter().skip(end) {
            if let Some(it) = list_item(l) {
                if let Some(c) = cur.take() {
                    items.push(c);
                }
                cur = Some(it.trim().to_string());
            } else if l.trim().is_empty() {
                continue;
            } else if l.starts_with(' ') || l.starts_with('\t') {
                if let Some(c) = cur.as_mut() {
                    c.push(' ');
                    c.push_str(l.trim());
                }
            } else {
                break;
            }
            if items.len() >= 6 {
                break;
            }
        }
        items.extend(cur);
        let items: Vec<String> = items.iter().map(|i| strip_inline_md(i)).filter(|i| !i.is_empty()).collect();
        if items.is_empty() {
            // "It is:" with nothing after it: drop the dangling sentence.
            let cut = p.trim_end_matches(':').rfind(['.', '!', '?']).map(|i| i + 1).unwrap_or(0);
            p = p[..cut].trim().to_string();
        } else {
            let items: Vec<&str> = items.iter().map(|i| i.trim_end_matches(['.', ';'])).collect();
            p = format!("{p} {}.", items.join("; "));
        }
    }
    let p = p.trim();
    if p.len() < 20 {
        return None;
    }
    Some(if p.chars().count() > 400 { format!("{}…", p.chars().take(399).collect::<String>()) } else { p.to_string() })
}

fn readme_excerpt(readme: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for l in readme.lines() {
        let t = l.trim_start();
        if t.starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && (t.starts_with("[![") || t.starts_with("![") || t.starts_with('<')) {
            continue;
        }
        if l.trim().is_empty() && (out.is_empty() || out.ends_with("\n\n")) {
            continue;
        }
        if out.len() + l.len() > max_chars {
            out.push_str("…\n");
            break;
        }
        out.push_str(l);
        out.push('\n');
    }
    if in_fence && !out.trim_end().ends_with("```") {
        out.push_str("```\n");
    }
    out.trim().to_string()
}

/// A package manifest found while indexing (`Cargo.toml` with `[package]`, `package.json`,
/// `pyproject.toml`, `go.mod`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Manifest {
    /// Directory of the manifest ("" = repo root).
    pub dir: String,
    pub name: String,
    pub description: Option<String>,
    /// Files under `dir` (set by the indexer; 0 when unknown).
    pub files: usize,
}

/// Manifest file names [`parse_manifest`] understands.
pub(crate) fn is_manifest(file: &str) -> bool {
    matches!(file, "Cargo.toml" | "package.json" | "pyproject.toml" | "go.mod")
}

/// Package name + description from a manifest. None for workspace-only roots and unnamed packages.
pub(crate) fn parse_manifest(path: &str, text: &str) -> Option<Manifest> {
    let (dir, file) = match path.rsplit_once('/') {
        Some((d, f)) => (d.to_string(), f),
        None => (String::new(), path),
    };
    let clean = |d: Option<&str>| {
        d.map(|d| d.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|d| !d.is_empty())
    };
    let (name, description) = match file {
        "Cargo.toml" | "pyproject.toml" => {
            let v: toml::Value = toml::from_str(text).ok()?;
            let sect = if file == "Cargo.toml" {
                v.get("package")?
            } else {
                v.get("project").or_else(|| v.get("tool").and_then(|t| t.get("poetry")))?
            };
            let name = sect.get("name")?.as_str()?.to_string();
            (name, clean(sect.get("description").and_then(|d| d.as_str())))
        }
        "package.json" => {
            let v: serde_json::Value = serde_json::from_str(text).ok()?;
            (v.get("name")?.as_str()?.to_string(), clean(v.get("description").and_then(|d| d.as_str())))
        }
        "go.mod" => {
            let m = text.lines().find_map(|l| l.trim().strip_prefix("module "))?.trim().trim_matches('"');
            let mut segs: Vec<&str> = m.split('/').collect();
            // github.com/o/n/v2 → n
            if segs.len() > 1 && segs.last().is_some_and(|s| s.len() >= 2 && s.starts_with('v') && s[1..].bytes().all(|b| b.is_ascii_digit())) {
                segs.pop();
            }
            (segs.last()?.to_string(), None)
        }
        _ => return None,
    };
    let name = name.trim().to_string();
    if name.is_empty() || name.len() > 100 {
        return None;
    }
    Some(Manifest { dir, name, description, files: 0 })
}

/// Test / bench / example / fuzz / conformance packages say nothing about what a repo is.
fn aux_package(m: &Manifest) -> bool {
    let n = m.name.to_ascii_lowercase();
    is_aux(&format!("{}/x", m.dir))
        || m.dir.split('/').any(|d| {
            let d = d.to_ascii_lowercase();
            d.starts_with("test")
                || d.ends_with("-test")
                || d.ends_with("-tests")
                || d.contains("bench")
                || d.contains("example")
                || d.contains("fuzz")
                || d.contains("conformance")
        })
        || ["test", "bench", "example", "fuzz", "conformance"].iter().any(|w| n.contains(w))
}

/// Package names for `wh.repos.crates`: the root package first, then the package named like the
/// repo, then by size (files under the manifest's directory); test / bench / example packages,
/// `xtask` and duplicates dropped. npm scopes are kept (`@scope/name`) and the bare name added.
pub(crate) fn crate_names(manifests: &[Manifest], repo_short: &str) -> Vec<String> {
    let mut ms: Vec<&Manifest> = manifests.iter().filter(|m| !aux_package(m)).collect();
    let short = repo_short.to_ascii_lowercase();
    ms.sort_by_key(|m| (!m.dir.is_empty(), m.name.to_ascii_lowercase() != short, std::cmp::Reverse(m.files), m.dir.len()));
    let mut out: Vec<String> = Vec::new();
    for m in ms {
        let mut names = vec![m.name.clone()];
        if let Some((_, bare)) = m.name.strip_prefix('@').and_then(|n| n.split_once('/')) {
            names.push(bare.to_string());
        }
        for n in names {
            if !out.contains(&n) && n != "xtask" {
                out.push(n);
            }
        }
    }
    out.truncate(200);
    out
}

/// Tests, benches, examples and fuzz targets: never entry points, key files or modules.
pub(crate) fn is_aux(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    let mut parts: Vec<&str> = l.split('/').collect();
    let file = parts.pop().unwrap_or("");
    parts.iter().any(|d| {
        matches!(*d, "tests" | "test" | "benches" | "bench" | "benchmarks" | "examples" | "example" | "fuzz" | "testing" | "test-utils" | "test_utils")
            || ["tests-", "test-", "tests_"].iter().any(|p| d.starts_with(p))
            || ["-tests", "_tests", "-test", "-bench", "-benches", "-examples", "-fuzz"].iter().any(|x| d.ends_with(x))
    }) || file.contains("_test.")
        || file.contains(".test.")
        || file.starts_with("test_")
}

/// Signatures that say nothing about a file in a card.
fn trivial_symbol(name: &str, kind: &str, sig: Option<&str>) -> bool {
    let n = name;
    let trivial_fn = matches!(
        n,
        "drop" | "fmt" | "clone" | "clone_from" | "eq" | "ne" | "hash" | "partial_cmp" | "cmp" | "from" | "into"
            | "try_from" | "deref" | "deref_mut" | "as_ref" | "as_mut" | "borrow" | "borrow_mut" | "default"
            | "to_string" | "main" | "__init__" | "__repr__" | "__str__" | "__eq__" | "__hash__" | "toString"
            | "String" | "Error"
    );
    let trivial_type = kind == "type" && matches!(n, "Output" | "Error" | "Item" | "Target" | "Ok" | "Err" | "Future" | "Owned" | "Err_");
    let cfg_macro = kind == "macro" && (n.starts_with("cfg_") || n.starts_with("__"));
    let s = sig.unwrap_or("");
    (matches!(kind, "function" | "method") && trivial_fn) || trivial_type || cfg_macro || s.starts_with("#[cfg")
}

pub(crate) async fn build(
    pool: &PgPool,
    repo_id: &str,
    dir: &Path,
    meta: &RepoMeta,
    summarizer: Option<&dyn CardSummarizer>,
) -> Result<BuiltCard> {
    // README + manifest straight from git at the indexed commit.
    let root = git::run(Some(dir), &["ls-tree", "--name-only", &meta.head], git::SHORT).await?;
    let names: Vec<&str> = root.lines().collect();
    let readme_name = ["README.md", "README.markdown", "README.rst", "README.txt", "README", "readme.md", "Readme.md"]
        .into_iter()
        .find(|n| names.contains(n));
    let readme = match readme_name {
        Some(n) => git::show(dir, &meta.head, n, git::META_MAX_BYTES)
            .await?
            .data()
            .map(|b| String::from_utf8_lossy(&b[..b.len().min(64 * 1024)]).into_owned()),
        None => None,
    };
    let readme_para = readme.as_deref().and_then(readme_paragraph);
    let short = meta.slug.rsplit('/').next().unwrap_or(&meta.slug);
    let root_desc = meta.manifests.iter().find(|m| m.dir.is_empty()).and_then(|m| m.description.clone());
    let named_desc = meta.manifests.iter().find(|m| m.name == short).and_then(|m| m.description.clone());
    let any_desc = || {
        let mut ms: Vec<&Manifest> =
            meta.manifests.iter().filter(|m| m.description.is_some() && !is_aux(&format!("{}/x", m.dir))).collect();
        ms.sort_by_key(|m| m.dir.len());
        ms.first().and_then(|m| m.description.clone())
    };
    let description = root_desc.or(readme_para).or(named_desc).or_else(any_desc);

    let langs: Vec<(Option<String>, i64, i64, i64)> = sqlx::query_as(
        "select language, count(*), coalesce(sum(bytes), 0)::bigint, coalesce(sum(lines), 0)::bigint
           from wh.files where repo_id = $1::uuid group by language order by 3 desc",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?;
    let total_bytes: i64 = langs.iter().map(|l| l.2).sum::<i64>().max(1);
    let total_files: i64 = langs.iter().map(|l| l.1).sum();
    let total_lines: i64 = langs.iter().map(|l| l.3).sum();
    let (nsym, nchunks): (i64, i64) = sqlx::query_as(
        "select (select count(*) from wh.symbols where repo_id = $1::uuid), (select count(*) from wh.chunks where repo_id = $1::uuid)",
    )
    .bind(repo_id)
    .fetch_one(pool)
    .await?;

    let mut md = String::new();
    let _ = writeln!(md, "# {}", meta.slug);
    md.push('\n');
    if let Some(d) = &description {
        let _ = writeln!(md, "{d}\n");
    }
    let lic = match (&meta.license.spdx, meta.license.copyleft) {
        (Some(s), Some(true)) => format!(
            "{s} — **copyleft: never copy into closed projects; reimplement via a cleanroom component**"
        ),
        (Some(s), _) => format!("{s} (confidence {:.2})", meta.license.confidence.unwrap_or(0.0)),
        (None, _) => "unknown — treat as all rights reserved; do not copy code".to_string(),
    };
    let _ = writeln!(md, "- **License:** {lic}");
    let lang_line: Vec<String> = langs
        .iter()
        .take(6)
        .map(|(l, n, b, _)| {
            format!("{} {:.0}% ({} files)", l.as_deref().unwrap_or("other"), 100.0 * *b as f64 / total_bytes as f64, n)
        })
        .collect();
    let _ = writeln!(md, "- **Languages:** {}", lang_line.join(", "));
    let _ = writeln!(
        md,
        "- **Size:** {total_files} indexed files, {total_lines} lines, {} KB; {nsym} symbols, {nchunks} chunks",
        total_bytes / 1024
    );
    let date = meta.committed_at.map(|d| d.format(" (%Y-%m-%d)").to_string()).unwrap_or_default();
    let _ = writeln!(md, "- **Head:** `{}`{date} on `{}`", &meta.head[..meta.head.len().min(12)], meta.default_branch);
    if let Some(u) = &meta.origin_url
        && meta.tier != "own"
    {
        let _ = writeln!(md, "- **Upstream:** {u}");
    }
    let _ = writeln!(md, "- **Tier:** {}", meta.tier);

    let entries: Vec<String> = sqlx::query_scalar(
        r"select path from wh.files where repo_id = $1::uuid and (
             path ~ '(^|/)src/(main|lib)\.rs$' or path ~ '(^|/)src/bin/[^/]+\.rs$' or path ~ '^cmd/[^/]+/main\.go$'
             or path ~ '^(src/)?(main|app|cli|__main__)\.(go|py|c|cc|cpp|ts|js)$' or path ~ '^(src/)?index\.(ts|tsx|js|mjs)$'
             or path ~ '^(src/)?[^/]+/__init__\.py$')
          order by length(path), path limit 40",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?;
    let entries: Vec<String> = entries.into_iter().filter(|p| !is_aux(p)).take(12).collect();
    if !entries.is_empty() {
        md.push_str("\n## Entry points\n\n");
        for e in &entries {
            let _ = writeln!(md, "- `{e}`");
        }
    }

    let ranked = rank_files(pool, repo_id).await?;
    let modules = module_map(&ranked, &meta.manifests);
    if !modules.is_empty() {
        // Before the entry points, so query()'s card header (cut at "## Entry points") keeps it.
        let pos = md.find("\n## Entry points").unwrap_or(md.len());
        let mut sect = String::from("\n## Modules\n\n");
        for m in &modules {
            let krate = m.krate.as_deref().map(|c| format!("crate `{c}`, ")).unwrap_or_default();
            let central = m.central.as_deref().map(|c| format!("; central: `{c}`")).unwrap_or_default();
            let _ = writeln!(sect, "- `{}` — {krate}{} files{central}", m.key, m.files);
        }
        md.insert_str(pos, &sect);
    }
    // Library code only: tests, benches, examples and prose never make the key files.
    // Twice as many candidates as shown: files without a non-trivial symbol are skipped.
    let top: Vec<&(i64, String, f64)> = ranked.iter().filter(|(_, p, _)| !is_prose(p) && !is_aux(p)).take(TOP_FILES * 2).collect();
    if !top.is_empty() {
        let ids: Vec<i64> = top.iter().map(|t| t.0).collect();
        let syms: Vec<CardSym> = sqlx::query_as(
            "select file_id, name, kind, signature, line_start from (
               select s.file_id, s.name, s.kind, s.signature, s.line_start,
                      row_number() over (partition by s.file_id order by coalesce(rc.n, 0) desc, s.line_start) as rn
                 from wh.symbols s
                 left join (select name, count(*) as n from wh.refs where repo_id = $1::uuid group by name) rc on rc.name = s.name
                where s.file_id = any($2) and s.kind not in ('impl', 'module')) x
              where rn <= $3 order by file_id, rn",
        )
        .bind(repo_id)
        .bind(&ids)
        .bind(SYMS_PER_FILE * 3)
        .fetch_all(pool)
        .await?;
        let mut by_file: HashMap<i64, Vec<&CardSym>> = HashMap::new();
        for s in syms.iter().filter(|s| !trivial_symbol(&s.1, &s.2, s.3.as_deref())) {
            let v = by_file.entry(s.0).or_default();
            if v.len() < SYMS_PER_FILE as usize {
                v.push(s);
            }
        }
        for v in by_file.values_mut() {
            v.sort_by_key(|s| s.4);
        }
        md.push_str("\n## Key files (PageRank over defs/refs)\n");
        for (id, path, _) in top.into_iter().filter(|t| by_file.contains_key(&t.0)).take(TOP_FILES) {
            let _ = writeln!(md, "\n### `{path}`");
            for (_, name, kind, sig, line) in by_file.get(id).map(|v| v.as_slice()).unwrap_or(&[]) {
                let s = sig.as_deref().unwrap_or(name);
                let _ = writeln!(md, "- `{s}` — {kind} L{line}");
            }
        }
    }

    if let Some(r) = &readme {
        let ex = readme_excerpt(r, 1500);
        if !ex.is_empty() {
            let _ = writeln!(md, "\n## README excerpt\n\n{ex}");
        }
    }

    if let Some(s) = summarizer {
        match s.summarize(&meta.slug, &md).await {
            Ok(sum) if !sum.trim().is_empty() => {
                // Insert the summary after the title line.
                let (title, rest) = md.split_once('\n').unwrap_or((&md, ""));
                md = format!("{title}\n\n## Summary\n\n{}\n{rest}", sum.trim());
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(slug = %meta.slug, "card summarizer failed: {e:#}"),
        }
    }
    Ok(BuiltCard { card_md: md, description })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagerank_prefers_the_hub() {
        // 1, 2, 3 all reference 0.
        let r = pagerank(4, &[(1, 0, 1.0), (2, 0, 1.0), (3, 0, 1.0)], 50, 0.85);
        assert!(r[0] > r[1] && r[0] > r[2]);
        assert!((r.iter().sum::<f64>() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn readme_para_skips_badges() {
        let r = "# memchr\n\n[![ci](x)](y)\n\nThis library provides heavily optimized routines\nfor string search.\n\n## Usage\n";
        assert_eq!(readme_paragraph(r).unwrap(), "This library provides heavily optimized routines for string search.");
    }

    #[test]
    fn readme_para_takes_the_list_it_introduces() {
        let tokio = "# Tokio\n\nA runtime for writing reliable, asynchronous, and slim applications with\nthe Rust programming language. It is:\n\n* **Fast**: Tokio's zero-cost abstractions give you bare-metal\n  performance.\n\n* **Reliable**: Tokio leverages Rust's ownership.\n\n[![Crates.io][crates-badge]][crates-url]\n";
        assert_eq!(
            readme_paragraph(tokio).unwrap(),
            "A runtime for writing reliable, asynchronous, and slim applications with the Rust programming language. \
             It is: Fast: Tokio's zero-cost abstractions give you bare-metal performance; Reliable: Tokio leverages Rust's ownership."
        );
        // A colon with nothing after it: the dangling sentence goes.
        let r = "# x\n\nA small library for parsing widgets quickly. Features:\n\n## Install\n";
        assert_eq!(readme_paragraph(r).unwrap(), "A small library for parsing widgets quickly.");
        assert_eq!(strip_inline_md("A **bold** [link](http://x) and `code` [ref][r]"), "A bold link and code ref");
    }

    #[test]
    fn manifests_and_package_names() {
        let m = parse_manifest("crates/rmcp/Cargo.toml", "[package]\nname = \"rmcp\"\ndescription = \"Rust SDK for MCP\"\n").unwrap();
        assert_eq!((m.dir.as_str(), m.name.as_str(), m.description.as_deref()), ("crates/rmcp", "rmcp", Some("Rust SDK for MCP")));
        assert!(parse_manifest("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n").is_none());
        assert_eq!(parse_manifest("web/package.json", r#"{"name": "@acme/widgets", "description": "UI"}"#).unwrap().name, "@acme/widgets");
        assert_eq!(parse_manifest("go.mod", "module github.com/o/thing/v2\n\ngo 1.22\n").unwrap().name, "thing");
        assert_eq!(parse_manifest("py/pyproject.toml", "[project]\nname = \"pkg\"\n").unwrap().name, "pkg");
        let mk = |dir: &str, name: &str, files: usize| Manifest { dir: dir.into(), name: name.into(), description: None, files };
        let ms = vec![
            mk("conformance", "mcp-conformance", 10),
            mk("crates/rmcp-macros", "rmcp-macros", 5),
            mk("crates/rmcp", "rmcp", 200),
            mk("examples/servers", "mcp-server-examples", 50),
            mk("xtask", "xtask", 3),
            mk("tests-integration", "tests-integration", 30),
            mk("web", "@acme/widgets", 4),
        ];
        assert_eq!(crate_names(&ms, "rust-sdk"), ["rmcp", "rmcp-macros", "@acme/widgets", "widgets"]);
        let ms = vec![mk("tokio-util", "tokio-util", 90), mk("tokio", "tokio", 400), mk("", "workspace-root", 900)];
        assert_eq!(crate_names(&ms, "tokio"), ["workspace-root", "tokio", "tokio-util"]);
    }

    #[test]
    fn module_map_groups_by_crate_and_top_dir() {
        let ranked: Vec<(i64, String, f64)> = [
            "crates/ignore/src/walk.rs",
            "crates/ignore/README.md",
            "crates/core/main.rs",
            "crates/ignore/src/gitignore.rs",
            "crates/ignore/tests/a.rs",
            "tests/misc.rs",
            "benches/b.rs",
            "build.rs",
            "scripts/release.sh",
        ]
        .iter()
        .enumerate()
        .map(|(i, p)| (i as i64, p.to_string(), 1.0 / (i as f64 + 1.0)))
        .collect();
        let ms = vec![Manifest { dir: "crates/ignore".into(), name: "ignore".into(), description: None, files: 0 }];
        let m = module_map(&ranked, &ms);
        assert_eq!(m[0], Module { key: "crates/ignore".into(), krate: Some("ignore".into()), files: 4, central: Some("crates/ignore/src/walk.rs".into()) });
        assert!(m.iter().any(|x| x.key == "crates" && x.files == 1 && x.central.as_deref() == Some("crates/core/main.rs")));
        assert!(m.iter().all(|x| x.key != "tests" && x.key != "benches"));
        assert!(m.iter().any(|x| x.key == "scripts"));
        assert!(!m.iter().any(|x| x.key == "build.rs"));
    }

    #[test]
    fn trivial_symbols_and_aux_paths() {
        assert!(trivial_symbol("drop", "method", Some("fn drop(&mut self)")));
        assert!(trivial_symbol("Output", "type", Some("type Output = T")));
        assert!(trivial_symbol("cfg_rt", "macro", Some("macro_rules! cfg_rt")));
        assert!(!trivial_symbol("spawn", "function", Some("pub fn spawn<F>(f: F)")));
        assert!(is_aux("tokio/tests/io.rs") && is_aux("tests-build/src/lib.rs") && is_aux("stress-test/src/main.rs") && is_aux("src/foo_test.go"));
        assert!(!is_aux("tokio/src/runtime/task/mod.rs") && !is_aux("crates/ignore/src/gitignore.rs"));
    }
}

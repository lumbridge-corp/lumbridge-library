//! The design shelf: website / product design systems kept as reference.
//!
//! A design is two things:
//! - **the system**: our own write-up (DESIGN.md, tokens.css, COMPONENTS.md, preview / gallery HTML)
//!   plus a `design.toml`, in a git repo at `{root}/warehouse/design-systems/<slug>/`, indexed like
//!   any repo (tier `design`, slug `designs/<slug>`) and published to Gitea `warehouse/design-<slug>`;
//! - **the capture** (kind `capture` only): what a capture tool measured from the live site
//!   (manifest.json, tokens.json, pages/NN/{desktop-fold,desktop,mobile}.png, assets) under
//!   `{root}/warehouse/designs/<capture>/`. Third-party material: private reference, never in git.
//!
//! The embedder reads text only, so screenshots are searchable through captions written by a vision
//! model ([`Vision`], `[designs] vision_base_url`) and embedded next to the design's own text. Measured tokens
//! become facets (theme, bg / fg / accent, fonts, radius) for filtering.

use super::git;
use super::index::{IndexOpts, sync_repo};
use super::warehouse_dir;
use crate::db::halfvec_literal;
use crate::models::{Embedder, Kind, Reranker};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::{Component as PathComponent, Path, PathBuf};
use std::time::Duration;

pub const KINDS: &[&str] = &["capture", "reference", "own"];
pub const THEMES: &[&str] = &["dark", "light", "both"];
/// Text files copied from a capture's `system/` folder into the design repo.
const SYSTEM_EXTS: &[&str] = &["md", "css", "html", "json", "toml", "txt", "svg", "js"];
const MAX_FILE: u64 = 2 * 1024 * 1024;
/// Shots the vision model captions (viewport-sized; full-page shots are 7-13k px tall).
const CAPTION_VIEWS: &[&str] = &["fold", "preview"];

// ───────────────────────────── types ─────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NewDesign {
    /// `[a-z0-9][a-z0-9._-]*`, e.g. `wafer-ai`.
    pub slug: String,
    /// Display name (default: from the slug).
    #[serde(default)]
    pub name: Option<String>,
    /// capture | reference | own
    pub kind: String,
    #[serde(default)]
    pub site_url: Option<String>,
    /// One or two sentences on the look. Default: the first paragraph of DESIGN.md.
    #[serde(default)]
    pub summary: Option<String>,
    /// dark | light | both (default: measured from the capture).
    #[serde(default)]
    pub theme: Option<String>,
    /// `#rrggbb` page background / ink / accent (default: measured).
    #[serde(default)]
    pub bg: Option<String>,
    #[serde(default)]
    pub fg: Option<String>,
    #[serde(default)]
    pub accent: Option<String>,
    /// Font family names, display first (default: measured).
    #[serde(default)]
    pub fonts: Vec<String>,
    #[serde(default)]
    pub radius_px: Option<f32>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Where it came from, e.g. the capture tool and its version, or a design collection.
    #[serde(default)]
    pub source: Option<String>,
    /// Capture folder name under `{root}/warehouse/designs/` (kind capture). Its `system/` text files
    /// become the design repo when `files` is empty; its screenshots are registered as shots.
    #[serde(default)]
    pub capture: Option<String>,
    /// (relative path, content). Replaces the repo's files when non-empty.
    #[serde(default)]
    pub files: Vec<(String, String)>,
    pub created_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, sqlx::FromRow)]
pub struct DesignRow {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub kind: String,
    pub site_url: Option<String>,
    pub summary: String,
    pub theme: Option<String>,
    pub bg: Option<String>,
    pub fg: Option<String>,
    pub accent: Option<String>,
    pub fonts: Vec<String>,
    pub radius_px: Option<f32>,
    pub tags: Vec<String>,
    pub source: Option<String>,
    pub license_spdx: String,
    pub status: String,
    /// `designs/<slug>` in wh.repos: read files with warehouse_read / warehouse_tree.
    pub repo_slug: Option<String>,
    pub local_path: Option<String>,
    pub gitea: Option<String>,
    pub capture_dir: Option<String>,
    #[schemars(with = "Option<String>")]
    pub captured_at: Option<DateTime<Utc>>,
    pub page_count: Option<i32>,
    pub shots: i64,
    pub captioned: i64,
    pub created_by: String,
    #[schemars(with = "String")]
    pub created_at: DateTime<Utc>,
    #[schemars(with = "String")]
    pub updated_at: DateTime<Utc>,
    #[schemars(with = "Option<String>")]
    pub approved_at: Option<DateTime<Utc>>,
}

const DESIGN_SELECT: &str = "select d.id::text as id, d.slug, d.name, d.kind, d.site_url, d.summary, d.theme, d.bg, d.fg,
        d.accent, d.fonts, d.radius_px, d.tags, d.source, d.license_spdx, d.status, r.slug as repo_slug,
        r.local_path, r.gitea_full_name as gitea, d.capture_dir, d.captured_at, d.page_count,
        (select count(*) from wh.design_shots s where s.design_id = d.id) as shots,
        (select count(*) from wh.design_shots s where s.design_id = d.id and s.caption is not null) as captioned,
        d.created_by, d.created_at, d.updated_at, d.approved_at
   from wh.designs d left join wh.repos r on r.id = d.repo_id";

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, sqlx::FromRow)]
pub struct ShotRow {
    pub id: i64,
    pub path: String,
    /// fold | desktop | mobile | preview | components
    pub view: String,
    pub page_n: Option<String>,
    pub page_url: Option<String>,
    pub page_title: Option<String>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub caption: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DesignFilter {
    /// draft | approved | retired (default: draft + approved).
    #[serde(default)]
    pub status: Option<String>,
    /// capture | reference | own
    #[serde(default)]
    pub kind: Option<String>,
    /// dark | light | both
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    /// Font family, case-insensitive substring (e.g. `mono`, `Inter`).
    #[serde(default)]
    pub font: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DesignDetail {
    pub design: DesignRow,
    /// design.toml facets plus measured extras (spacing, type scale, shadows ...).
    pub facets: Value,
    /// Files in the design repo (read one with design_read path=...).
    pub files: Vec<String>,
    pub shots: Vec<ShotRow>,
}

fn valid_slug(s: &str) -> bool {
    let mut ch = s.chars();
    matches!(ch.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.len() <= 80
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c))
}

fn valid_rel_path(p: &str) -> bool {
    let path = Path::new(p);
    !p.is_empty()
        && path.components().all(|c| matches!(c, PathComponent::Normal(_)))
        && !path.components().any(|c| c.as_os_str() == ".git")
}

pub fn bare(slug: &str) -> &str {
    let s = slug.trim().trim_start_matches('/');
    let s = s.strip_prefix("warehouse/").unwrap_or(s);
    s.strip_prefix("designs/").unwrap_or(s)
}

pub fn designs_dir(root: &Path) -> PathBuf {
    warehouse_dir(root).join("designs")
}
pub fn systems_dir(root: &Path) -> PathBuf {
    warehouse_dir(root).join("design-systems")
}

// ───────────────────────────── colours & facets ─────────────────────────────

/// `#abc`, `#aabbcc`, `#aabbccdd`, `rgb(…)`, `rgba(…)` → (rgb, alpha).
pub fn parse_color(s: &str) -> Option<([u8; 3], f32)> {
    let s = s.trim().to_ascii_lowercase();
    if let Some(h) = s.strip_prefix('#') {
        let h: String = h.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
        return match h.len() {
            3 | 4 => {
                let d: Vec<u8> = h.chars().map(|c| c.to_digit(16).unwrap_or(0) as u8 * 17).collect();
                let a = if h.len() == 4 { d[3] as f32 / 255.0 } else { 1.0 };
                Some(([d[0], d[1], d[2]], a))
            }
            6 => Some(([byte(0)?, byte(2)?, byte(4)?], 1.0)),
            8 => Some(([byte(0)?, byte(2)?, byte(4)?], byte(6)? as f32 / 255.0)),
            _ => None,
        };
    }
    let inner = s.strip_prefix("rgba(").or_else(|| s.strip_prefix("rgb("))?.split(')').next()?;
    let parts: Vec<f32> = inner
        .split(|c: char| c == ',' || c == '/' || c.is_whitespace())
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.trim_end_matches('%').parse::<f32>().ok())
        .collect();
    if parts.len() < 3 {
        return None;
    }
    let a = parts.get(3).copied().unwrap_or(1.0);
    Some(([parts[0].round() as u8, parts[1].round() as u8, parts[2].round() as u8], a))
}

pub fn hex(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

fn luminance(rgb: [u8; 3]) -> f32 {
    let f = |c: u8| {
        let c = c as f32 / 255.0;
        if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * f(rgb[0]) + 0.7152 * f(rgb[1]) + 0.0722 * f(rgb[2])
}

/// max - min channel: 0 for greys, high for vivid colours.
fn chroma(c: [u8; 3]) -> u8 {
    c.iter().max().unwrap_or(&0) - c.iter().min().unwrap_or(&0)
}

/// WCAG contrast ratio (1..21).
fn contrast(a: [u8; 3], b: [u8; 3]) -> f32 {
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// Euclidean distance in RGB (0..441).
pub fn color_distance(a: &str, b: &str) -> Option<f32> {
    let (x, _) = parse_color(a)?;
    let (y, _) = parse_color(b)?;
    let d: f32 = (0..3).map(|i| (x[i] as f32 - y[i] as f32).powi(2)).sum();
    Some(d.sqrt())
}

fn normalise_hex(s: Option<&str>) -> Option<String> {
    s.and_then(parse_color).map(|(c, _)| hex(c))
}

const GENERIC_FONTS: &[&str] = &[
    "system-ui", "sans-serif", "serif", "monospace", "ui-monospace", "ui-sans-serif", "ui-serif", "-apple-system",
    "blinkmacsystemfont", "segoe ui", "roboto", "helvetica", "helvetica neue", "arial", "apple color emoji",
    "segoe ui emoji", "segoe ui symbol", "noto color emoji", "sfmono-regular", "menlo", "monaco", "consolas",
    "liberation mono", "courier new", "cursive", "inherit", "initial",
];

fn first_family(stack: &str) -> Option<String> {
    stack
        .split(',')
        .map(|f| f.trim().trim_matches(|c| c == '"' || c == '\'').trim().to_string())
        .find(|f| !f.is_empty() && !GENERIC_FONTS.contains(&f.to_ascii_lowercase().as_str()) && !f.starts_with("var("))
}

fn ranked(tokens: &Value, key: &str) -> Vec<String> {
    tokens[key]
        .as_array()
        .map(|a| a.iter().filter_map(|e| e["v"].as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// What can be read off a capture's tokens.json (computed styles counted over all pages) and the
/// system's tokens.css.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Measured {
    pub theme: Option<String>,
    pub bg: Option<String>,
    pub fg: Option<String>,
    pub accent: Option<String>,
    pub fonts: Vec<String>,
    pub radius_px: Option<f32>,
    pub extra: Value,
}

pub fn measure(tokens: Option<&Value>, tokens_css: Option<&str>) -> Measured {
    let mut m = Measured::default();
    let opaque = |list: Vec<String>| {
        list.iter().filter_map(|v| parse_color(v)).find(|(_, a)| *a >= 0.99).map(|(c, _)| c)
    };
    if let Some(t) = tokens {
        if let Some(bg) = opaque(ranked(t, "background")) {
            m.bg = Some(hex(bg));
            m.theme = Some(if luminance(bg) < 0.18 { "dark" } else { "light" }.into());
        }
        // Ink: among the most used near-neutral text colours that read on the background, the one
        // with the most contrast (a brand colour used for many labels is not the ink).
        if let Some(bg) = m.bg.as_deref().and_then(parse_color).map(|c| c.0) {
            let inks: Vec<[u8; 3]> = ranked(t, "text_color")
                .iter()
                .filter_map(|v| parse_color(v))
                .filter(|(c, a)| *a >= 0.99 && chroma(*c) < 48 && contrast(*c, bg) >= 4.5)
                .map(|(c, _)| c)
                .take(3)
                .collect();
            m.fg = inks.iter().copied().max_by(|a, b| contrast(*a, bg).total_cmp(&contrast(*b, bg))).map(hex);
        }
        // Accent fallback: the most used saturated colour across backgrounds, text and borders.
        let mut sat: HashMap<[u8; 3], f64> = HashMap::new();
        for key in ["background", "text_color", "border"] {
            for e in t[key].as_array().into_iter().flatten() {
                let (Some(v), Some(c)) = (e["v"].as_str(), e["c"].as_f64()) else { continue };
                let found = if key == "border" { v.find("rgb").map(|i| &v[i..]).and_then(parse_color) } else { parse_color(v) };
                if let Some((rgb, a)) = found
                    && a >= 0.99
                    && chroma(rgb) >= 80
                {
                    *sat.entry(rgb).or_default() += c;
                }
            }
        }
        m.accent = sat.into_iter().max_by(|a, b| a.1.total_cmp(&b.1)).map(|(c, _)| hex(c));
        let mut fonts: Vec<String> = Vec::new();
        for f in ranked(t, "font_family").iter().filter_map(|s| first_family(s)) {
            if !fonts.iter().any(|x| x.eq_ignore_ascii_case(&f)) {
                fonts.push(f);
            }
        }
        fonts.truncate(4);
        m.fonts = fonts;
        m.radius_px = ranked(t, "radius")
            .iter()
            .filter_map(|r| r.strip_suffix("px").and_then(|n| n.parse::<f32>().ok()))
            .find(|r| *r > 0.0 && *r < 999.0);
        let top = |k: &str, n: usize| ranked(t, k).into_iter().take(n).collect::<Vec<_>>();
        m.extra = json!({
            "spacing": top("spacing", 8),
            "radius": top("radius", 6),
            "type_scale": top("type_scale", 6),
            "font_weight": top("font_weight", 4),
            "shadow": top("shadow", 3),
            "border": top("border", 3),
            "backdrop_filter": top("backdrop_filter", 3),
            "transition": top("transition", 3),
        });
    }
    if let Some(css) = tokens_css {
        // A custom property named `accent` wins (the write-up's own choice); `brand` / `primary`
        // only when nothing was measured.
        let re = regex::Regex::new(r"(?m)--([a-z0-9-]*(?:accent|brand|primary)[a-z0-9-]*)\s*:\s*([^;]+);").expect("regex");
        let skip = ["fg", "text", "ink", "hover", "tint", "light", "dark", "case", "link", "font", "line", "soft", "dim"];
        let pick = |want: &[&str]| {
            re.captures_iter(css).find_map(|c| {
                let name = &c[1];
                if !want.iter().any(|w| name.contains(w)) || skip.iter().any(|s| name.split('-').any(|p| p == *s)) {
                    return None;
                }
                parse_color(c[2].trim()).filter(|(_, a)| *a >= 0.99).map(|(rgb, _)| hex(rgb))
            })
        };
        if let Some(a) = pick(&["accent"]) {
            m.accent = Some(a);
        } else if m.accent.is_none() {
            m.accent = pick(&["brand", "primary"]);
        }
        let has_alt = css.contains("prefers-color-scheme") || css.contains("[data-theme");
        if has_alt && m.theme.is_some() {
            m.theme = Some("both".into());
        }
    }
    m
}

/// The first prose paragraph of a markdown document (skipping headings, quotes, tables, lists).
pub fn first_paragraph(md: &str) -> Option<String> {
    let mut para = String::new();
    // Prefer the paragraph under a "feel" / "theme" / "atmosphere" heading when there is one.
    let lines: Vec<&str> = md.lines().collect();
    let start = lines
        .iter()
        .position(|l| {
            let l = l.to_ascii_lowercase();
            l.starts_with('#') && (l.contains("feel") || l.contains("atmosphere") || l.contains("visual theme"))
        })
        .map(|i| i + 1)
        .unwrap_or(0);
    for l in &lines[start..] {
        let t = l.trim();
        if t.is_empty() {
            if !para.is_empty() {
                break;
            }
            continue;
        }
        if t.starts_with('#') || t.starts_with('>') || t.starts_with('|') || t.starts_with("- ") || t.starts_with("```") || t.starts_with("**Use") {
            if !para.is_empty() {
                break;
            }
            continue;
        }
        if !para.is_empty() {
            para.push(' ');
        }
        para.push_str(t);
    }
    let p = para.trim().to_string();
    if p.len() < 20 {
        return None;
    }
    Some(p.chars().take(600).collect())
}

fn title_from_slug(slug: &str) -> String {
    let base = slug.trim_end_matches("-com").trim_end_matches("-ai").trim_end_matches("-io");
    base.split(['-', '_', '.'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// PNG width/height from the IHDR chunk.
fn png_size(bytes: &[u8]) -> Option<(i32, i32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((w as i32, h as i32))
}

fn read_png_size(p: &Path) -> Option<(i32, i32)> {
    use std::io::Read;
    let mut f = std::fs::File::open(p).ok()?;
    let mut b = [0u8; 24];
    f.read_exact(&mut b).ok()?;
    png_size(&b)
}

// ───────────────────────────── create / update ─────────────────────────────

struct Capture {
    dir: PathBuf,
    name: String,
    manifest: Value,
    tokens: Option<Value>,
}

fn load_capture(root: &Path, name: &str) -> Result<Capture> {
    if !valid_slug(name) {
        bail!("invalid capture name {name:?}");
    }
    let dir = designs_dir(root).join(name);
    if !dir.is_dir() {
        bail!("no capture folder {} (copy the capture to the server first)", dir.display());
    }
    let read_json = |p: PathBuf| -> Option<Value> { std::fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok()) };
    let manifest = read_json(dir.join("manifest.json")).unwrap_or(Value::Null);
    let tokens = read_json(dir.join("tokens.json"));
    Ok(Capture { dir, name: name.to_string(), manifest, tokens })
}

/// Text files of a capture's `system/` folder, as (relative path, content).
fn system_files(cap: &Capture) -> Result<Vec<(String, String)>> {
    let sys = cap.dir.join("system");
    if !sys.is_dir() {
        bail!("capture {} has no system/ folder yet (write DESIGN.md + tokens.css first)", cap.name);
    }
    let mut out = Vec::new();
    for e in walkdir::WalkDir::new(&sys).max_depth(3).into_iter().filter_map(|e| e.ok()) {
        if !e.file_type().is_file() {
            continue;
        }
        let ext = e.path().extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
        if !SYSTEM_EXTS.contains(&ext.as_str()) || e.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > MAX_FILE {
            continue;
        }
        let rel = e.path().strip_prefix(&sys)?.to_string_lossy().to_string();
        if let Ok(text) = std::fs::read_to_string(e.path()) {
            out.push((rel, text));
        }
    }
    out.sort();
    if !out.iter().any(|(p, _)| p.eq_ignore_ascii_case("DESIGN.md")) {
        bail!("capture {} has no system/DESIGN.md", cap.name);
    }
    Ok(out)
}

#[derive(Serialize)]
struct DesignToml<'a> {
    slug: &'a str,
    name: &'a str,
    kind: &'a str,
    summary: &'a str,
    status: &'a str,
    license: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    site: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capture: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    captured_at: Option<String>,
    tags: &'a [String],
    facets: FacetsToml<'a>,
    created_by: &'a str,
}

#[derive(Serialize)]
struct FacetsToml<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    theme: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bg: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fg: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    accent: Option<&'a str>,
    fonts: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    radius_px: Option<f32>,
}

/// The text a design is found by: identity, facets, summary, DESIGN.md and COMPONENTS.md headings.
fn build_search_text(name: &str, site: Option<&str>, summary: &str, facets: &FacetsToml, tags: &[String], files: &[(String, String)]) -> String {
    let mut s = format!("{name}{}\n{summary}\n", site.map(|u| format!(" ({u})")).unwrap_or_default());
    s.push_str(&format!(
        "theme {} · background {} · ink {} · accent {} · fonts {} · radius {}\n",
        facets.theme.unwrap_or("?"),
        facets.bg.unwrap_or("?"),
        facets.fg.unwrap_or("?"),
        facets.accent.unwrap_or("?"),
        facets.fonts.join(", "),
        facets.radius_px.map(|r| format!("{r}px")).unwrap_or("?".into()),
    ));
    if !tags.is_empty() {
        s.push_str(&format!("tags: {}\n", tags.join(", ")));
    }
    if let Some((_, d)) = files.iter().find(|(p, _)| p.eq_ignore_ascii_case("DESIGN.md")) {
        s.push_str(&d.chars().take(5000).collect::<String>());
        s.push('\n');
    }
    if let Some((_, c)) = files.iter().find(|(p, _)| p.eq_ignore_ascii_case("COMPONENTS.md")) {
        let heads: Vec<&str> = c.lines().filter(|l| l.starts_with("## ") || l.starts_with("### ")).take(60).collect();
        if !heads.is_empty() {
            s.push_str("components: ");
            s.push_str(&heads.iter().map(|h| h.trim_start_matches('#').trim()).collect::<Vec<_>>().join("; "));
        }
    }
    s
}

/// Remove every file of a working tree except `.git`.
fn clear_worktree(dir: &Path) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        if e.file_name() == ".git" {
            continue;
        }
        let p = e.path();
        if e.file_type()?.is_dir() { std::fs::remove_dir_all(&p)? } else { std::fs::remove_file(&p)? }
    }
    Ok(())
}

/// Create or update a design: write the repo (design.toml + files), commit, upsert the rows,
/// register the capture's screenshots, and index the repo. The caller holds the index slot.
///
/// If the design repo doesn't exist yet but a component repo `components/design-<slug>` does (the
/// older way designs were stored), its history is cloned so nothing is lost.
pub async fn upsert_design(pool: &PgPool, root: &Path, d: NewDesign) -> Result<DesignRow> {
    if !valid_slug(&d.slug) {
        bail!("invalid design slug {:?}: use [a-z0-9][a-z0-9._-]*", d.slug);
    }
    if !KINDS.contains(&d.kind.as_str()) {
        bail!("kind must be one of {KINDS:?}");
    }
    if d.created_by.trim().is_empty() {
        bail!("created_by is required");
    }
    if let Some(t) = &d.theme
        && !THEMES.contains(&t.as_str())
    {
        bail!("theme must be one of {THEMES:?}");
    }
    for (p, _) in &d.files {
        if !valid_rel_path(p) {
            bail!("invalid file path {p:?}");
        }
        if p.eq_ignore_ascii_case("design.toml") || p.eq_ignore_ascii_case("README.md") {
            bail!("{p} is written by the warehouse");
        }
    }
    let cap = match &d.capture {
        Some(c) if !c.trim().is_empty() => Some(load_capture(root, c.trim())?),
        _ => None,
    };
    if d.kind == "capture" && cap.is_none() {
        bail!("kind capture needs `capture` (the folder name under warehouse/designs/)");
    }
    let files = if !d.files.is_empty() {
        d.files.clone()
    } else if let Some(c) = &cap {
        system_files(c)?
    } else {
        bail!("give `files` (at least DESIGN.md) or a `capture` with a system/ folder");
    };
    let design_md = files.iter().find(|(p, _)| p.eq_ignore_ascii_case("DESIGN.md")).map(|(_, c)| c.as_str());
    let tokens_css = files.iter().find(|(p, _)| p.ends_with("tokens.css")).map(|(_, c)| c.as_str());
    let m = measure(cap.as_ref().and_then(|c| c.tokens.as_ref()), tokens_css);

    let name = d.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| title_from_slug(&d.slug));
    let summary = d
        .summary
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| design_md.and_then(first_paragraph))
        .context("summary is required (DESIGN.md has no prose paragraph to use)")?;
    let site = d.site_url.clone().or_else(|| cap.as_ref().and_then(|c| c.manifest["site"].as_str().map(String::from)));
    let captured_at: Option<DateTime<Utc>> =
        cap.as_ref().and_then(|c| c.manifest["captured_at"].as_str()).and_then(|s| s.parse().ok());
    let page_count = cap.as_ref().and_then(|c| c.manifest["pages"].as_array().map(|a| a.len() as i32));
    let theme = d.theme.clone().or(m.theme.clone());
    let bg = normalise_hex(d.bg.as_deref()).or(m.bg.clone());
    let fg = normalise_hex(d.fg.as_deref()).or(m.fg.clone());
    let accent = normalise_hex(d.accent.as_deref()).or(m.accent.clone());
    let fonts = if d.fonts.is_empty() { m.fonts.clone() } else { d.fonts.clone() };
    let radius = d.radius_px.or(m.radius_px);
    let source = d.source.clone().or_else(|| cap.as_ref().and_then(|c| c.manifest["tool"].as_str().map(String::from)));

    let existing: Option<(String, String)> = sqlx::query_as("select id::text, status from wh.designs where slug = $1")
        .bind(&d.slug)
        .fetch_optional(pool)
        .await?;
    let status = existing.as_ref().map(|e| e.1.clone()).unwrap_or_else(|| "draft".into());

    let facets = FacetsToml {
        theme: theme.as_deref(),
        bg: bg.as_deref(),
        fg: fg.as_deref(),
        accent: accent.as_deref(),
        fonts: &fonts,
        radius_px: radius,
    };
    let search_text = build_search_text(&name, site.as_deref(), summary.trim(), &facets, &d.tags, &files);
    let manifest = DesignToml {
        slug: &d.slug,
        name: &name,
        kind: &d.kind,
        summary: summary.trim(),
        status: &status,
        license: &crate::identity::get().design_license,
        site: site.as_deref(),
        source: source.as_deref(),
        capture: cap.as_ref().map(|c| c.name.as_str()),
        captured_at: captured_at.map(|t| t.to_rfc3339()),
        tags: &d.tags,
        facets,
        created_by: &d.created_by,
    };

    // ── the repo
    let dir = systems_dir(root).join(&d.slug);
    if !dir.exists() {
        tokio::fs::create_dir_all(systems_dir(root)).await?;
        let old = warehouse_dir(root).join("components").join(format!("design-{}", d.slug));
        if old.join(".git").is_dir() {
            // A local path the warehouse built itself, so the file transport is safe here.
            git::run(None, &["-c", "protocol.file.allow=always", "clone", "--quiet", &old.to_string_lossy(), &dir.to_string_lossy()], git::SHORT)
                .await?;
            let _ = git::run(Some(&dir), &["remote", "remove", "origin"], git::SHORT).await;
        } else {
            tokio::fs::create_dir_all(&dir).await?;
        }
    }
    clear_worktree(&dir)?;
    tokio::fs::write(dir.join("design.toml"), toml::to_string_pretty(&manifest)?).await?;
    let mut readme = format!("# {name} design system\n\n{}\n\n- Kind: {}\n", summary.trim(), d.kind);
    if let Some(s) = &site {
        readme.push_str(&format!("- Site: {s}\n"));
    }
    if cap.is_some() {
        readme.push_str(
            "- Raw capture (screenshots, page HTML, assets) lives on the library server only and belongs to the site: private \
             reference, never ship it. This repo holds only our own write-up, tokens and placeholder rebuilds.\n",
        );
    }
    readme.push_str("- Files: ");
    readme.push_str(&files.iter().map(|(p, _)| format!("[{p}]({p})")).collect::<Vec<_>>().join(", "));
    readme.push('\n');
    tokio::fs::write(dir.join("README.md"), readme).await?;
    for (p, content) in &files {
        let dest = dir.join(p);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&dest, content).await?;
    }
    let dirty = !dir.join(".git").exists()
        || !git::run(Some(&dir), &["status", "--porcelain"], git::SHORT).await.unwrap_or_default().trim().is_empty();
    let head = if dirty {
        let verb = if existing.is_some() { "update" } else { "add" };
        git::init_and_commit(&dir, &d.created_by, &format!("{}: {verb} {} design\n\n{}", d.slug, d.kind, summary.trim())).await?
    } else {
        git::head(&dir).await?.unwrap_or_default()
    };

    // ── rows
    let mut tx = pool.begin().await?;
    let repo_id: String = sqlx::query_scalar(
        "insert into wh.repos (slug, tier, origin_url, local_path, default_branch, head_commit, license_spdx,
                               license_confidence, copyleft, description, tags, added_by)
         values ($1, 'design', $2, $3, 'main', $4, $5, 1.0, false, $6, $7, $8)
         on conflict (slug) do update set origin_url = excluded.origin_url, local_path = excluded.local_path,
           head_commit = excluded.head_commit, description = excluded.description, tags = excluded.tags
         returning id::text",
    )
    .bind(format!("designs/{}", d.slug))
    .bind(&site)
    .bind(dir.to_string_lossy().as_ref())
    .bind(&head)
    .bind(&crate::identity::get().design_license)
    .bind(summary.trim())
    .bind(&d.tags)
    .bind(&d.created_by)
    .fetch_one(&mut *tx)
    .await?;
    let design_id: String = sqlx::query_scalar(
        "insert into wh.designs (slug, name, kind, site_url, summary, theme, bg, fg, accent, fonts, radius_px, tags,
                                 source, repo_id, capture_dir, captured_at, page_count, facets, search_text, created_by)
         values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14::uuid,$15,$16,$17,$18,$19,$20)
         on conflict (slug) do update set name = excluded.name, kind = excluded.kind, site_url = excluded.site_url,
           summary = excluded.summary, theme = excluded.theme, bg = excluded.bg, fg = excluded.fg,
           accent = excluded.accent, fonts = excluded.fonts, radius_px = excluded.radius_px, tags = excluded.tags,
           source = excluded.source, repo_id = excluded.repo_id, capture_dir = excluded.capture_dir,
           captured_at = excluded.captured_at, page_count = excluded.page_count, facets = excluded.facets,
           embedding = case when wh.designs.search_text = excluded.search_text then wh.designs.embedding end,
           search_text = excluded.search_text, updated_at = now()
         returning id::text",
    )
    .bind(&d.slug)
    .bind(&name)
    .bind(&d.kind)
    .bind(&site)
    .bind(summary.trim())
    .bind(&theme)
    .bind(&bg)
    .bind(&fg)
    .bind(&accent)
    .bind(&fonts)
    .bind(radius)
    .bind(&d.tags)
    .bind(&source)
    .bind(&repo_id)
    .bind(cap.as_ref().map(|c| c.name.as_str()))
    .bind(captured_at)
    .bind(page_count)
    .bind(&m.extra)
    .bind(&search_text)
    .bind(&d.created_by)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    if let Some(c) = &cap {
        register_shots(pool, &design_id, c).await?;
    }
    let opts = IndexOpts { fetch: false, ..Default::default() };
    sync_repo(pool, root, &format!("designs/{}", d.slug), &opts).await?;
    get_design(pool, &d.slug).await?.context("design vanished")
}

/// Upsert the capture's screenshots (captions survive), drop rows whose files are gone.
async fn register_shots(pool: &PgPool, design_id: &str, cap: &Capture) -> Result<usize> {
    let pages: HashMap<String, (Option<String>, Option<String>)> = cap.manifest["pages"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let n = p["n"].as_str()?.to_string();
                    let url = p["final_url"].as_str().or(p["url"].as_str()).map(String::from);
                    Some((n, (url, p["title"].as_str().map(String::from))))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut shots: Vec<(String, &str, Option<String>)> = Vec::new();
    let pages_dir = cap.dir.join("pages");
    if let Ok(rd) = std::fs::read_dir(&pages_dir) {
        let mut ns: Vec<String> = rd.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
        ns.sort();
        for n in ns {
            for (file, view) in [("desktop-fold.png", "fold"), ("desktop.png", "desktop"), ("mobile.png", "mobile")] {
                if pages_dir.join(&n).join(file).is_file() {
                    shots.push((format!("pages/{n}/{file}"), view, Some(n.clone())));
                }
            }
        }
    }
    for (file, view) in [("preview-fold.png", "preview"), ("preview-full.png", "preview"), ("components-full.png", "components")] {
        if cap.dir.join("system").join(file).is_file() {
            shots.push((format!("system/{file}"), view, None));
        }
    }
    let mut keep: Vec<String> = Vec::new();
    for (path, view, n) in &shots {
        let full = cap.dir.join(path);
        let bytes = std::fs::metadata(&full).map(|m| m.len() as i32).unwrap_or(0);
        let (w, h) = read_png_size(&full).unzip();
        // Full-length previews are not captioned: only the fold-sized one.
        let view = if path.ends_with("preview-full.png") { "preview-full" } else { view };
        let (url, title) = n.as_ref().and_then(|n| pages.get(n)).cloned().unwrap_or((None, None));
        sqlx::query(
            "insert into wh.design_shots (design_id, path, view, page_n, page_url, page_title, width, height, bytes)
             values ($1::uuid, $2, $3, $4, $5, $6, $7, $8, $9)
             on conflict (design_id, path) do update set view = excluded.view, page_n = excluded.page_n,
               page_url = excluded.page_url, page_title = excluded.page_title, width = excluded.width,
               height = excluded.height,
               caption = case when wh.design_shots.bytes = excluded.bytes then wh.design_shots.caption end,
               caption_error = null,
               embedding = case when wh.design_shots.bytes = excluded.bytes then wh.design_shots.embedding end,
               bytes = excluded.bytes",
        )
        .bind(design_id)
        .bind(path)
        .bind(view)
        .bind(n)
        .bind(url)
        .bind(title)
        .bind(w)
        .bind(h)
        .bind(bytes)
        .execute(pool)
        .await?;
        keep.push(path.clone());
    }
    sqlx::query("delete from wh.design_shots where design_id = $1::uuid and not (path = any($2))")
        .bind(design_id)
        .bind(&keep)
        .execute(pool)
        .await?;
    Ok(keep.len())
}

// ───────────────────────────── read ─────────────────────────────

pub async fn get_design(pool: &PgPool, slug: &str) -> Result<Option<DesignRow>> {
    let sql = format!("{DESIGN_SELECT} where d.slug = $1");
    Ok(sqlx::query_as::<_, DesignRow>(&sql).bind(bare(slug)).fetch_optional(pool).await?)
}

pub async fn list_designs(pool: &PgPool, f: &DesignFilter) -> Result<Vec<DesignRow>> {
    let sql = format!(
        "{DESIGN_SELECT}
          where ($1::text is null or d.status = $1) and ($1::text is not null or d.status <> 'retired')
            and ($2::text is null or d.kind = $2)
            and ($3::text is null or d.theme = $3 or d.theme = 'both')
            and ($4::text is null or $4 = any(d.tags))
            and ($5::text is null or exists (select 1 from unnest(d.fonts) f where f ilike '%' || $5 || '%'))
          order by d.status = 'approved' desc, d.kind = 'capture' desc, d.slug"
    );
    Ok(sqlx::query_as::<_, DesignRow>(&sql)
        .bind(&f.status)
        .bind(&f.kind)
        .bind(&f.theme)
        .bind(&f.tag)
        .bind(&f.font)
        .fetch_all(pool)
        .await?)
}

pub async fn design_detail(pool: &PgPool, slug: &str) -> Result<Option<DesignDetail>> {
    let Some(design) = get_design(pool, slug).await? else { return Ok(None) };
    let facets: Value = sqlx::query_scalar("select facets from wh.designs where slug = $1").bind(&design.slug).fetch_one(pool).await?;
    let shots = shots_of(pool, &design.id).await?;
    let mut files = Vec::new();
    if let Some(p) = &design.local_path {
        let dir = Path::new(p);
        if let Ok(out) = git::run(Some(dir), &["ls-files"], git::SHORT).await {
            files = out.lines().map(String::from).collect();
        }
    }
    Ok(Some(DesignDetail { design, facets, files, shots }))
}

async fn shots_of(pool: &PgPool, design_id: &str) -> Result<Vec<ShotRow>> {
    Ok(sqlx::query_as::<_, ShotRow>(
        "select id, path, view, page_n, page_url, page_title, width, height, caption from wh.design_shots
          where design_id = $1::uuid
          order by coalesce(page_n, 'zz'), array_position(array['fold','preview','desktop','mobile','components','preview-full'], view)",
    )
    .bind(design_id)
    .fetch_all(pool)
    .await?)
}

/// One file of the design repo (working tree at the committed head).
pub async fn read_design_file(pool: &PgPool, slug: &str, path: &str) -> Result<String> {
    let d = get_design(pool, slug).await?.with_context(|| format!("no design {slug:?}"))?;
    if !valid_rel_path(path) {
        bail!("invalid path {path:?}");
    }
    let dir = d.local_path.context("design has no repo")?;
    match git::show(Path::new(&dir), "HEAD", path, MAX_FILE).await? {
        git::Blob::Data(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
        git::Blob::TooLarge(n) => bail!("{path} is {n} bytes (over the 2 MB cap)"),
        git::Blob::Missing => bail!("{path} is not in design {} (files: see design_read without path)", d.slug),
    }
}

/// Absolute path of a screenshot on the server (for serving the PNG).
pub async fn shot_file(pool: &PgPool, root: &Path, slug: &str, shot_id: i64) -> Result<PathBuf> {
    let (capture, path): (Option<String>, String) = sqlx::query_as(
        "select d.capture_dir, s.path from wh.design_shots s join wh.designs d on d.id = s.design_id
          where d.slug = $1 and s.id = $2",
    )
    .bind(bare(slug))
    .bind(shot_id)
    .fetch_optional(pool)
    .await?
    .with_context(|| format!("no shot {shot_id} in design {slug:?}"))?;
    let capture = capture.context("design has no capture")?;
    if !valid_slug(&capture) || !valid_rel_path(&path) {
        bail!("bad shot path");
    }
    Ok(designs_dir(root).join(capture).join(path))
}

pub async fn set_status(pool: &PgPool, slug: &str, status: &str) -> Result<DesignRow> {
    if !["draft", "approved", "retired"].contains(&status) {
        bail!("status must be draft | approved | retired");
    }
    let n = sqlx::query(
        "update wh.designs set status = $2, updated_at = now(),
           approved_at = case when $2 = 'approved' then now() else approved_at end
         where slug = $1",
    )
    .bind(bare(slug))
    .bind(status)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        bail!("no design {slug:?}");
    }
    get_design(pool, slug).await?.context("design vanished")
}

/// Rewrite `status` in design.toml and commit; returns whether a commit was made.
pub async fn write_status_file(row: &DesignRow) -> Result<bool> {
    let Some(dir) = row.local_path.as_deref().map(Path::new) else { return Ok(false) };
    let p = dir.join("design.toml");
    let text = tokio::fs::read_to_string(&p).await?;
    let new: String = text
        .lines()
        .map(|l| if l.starts_with("status") && l.contains('=') { format!("status = \"{}\"", row.status) } else { l.to_string() })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    if new == text {
        return Ok(false);
    }
    tokio::fs::write(&p, new).await?;
    git::init_and_commit(dir, &crate::identity::get().git_name, &format!("status: {}", row.status)).await?;
    Ok(true)
}

// ───────────────────────────── captions & embeddings ─────────────────────────────

/// OpenAI-compatible vision chat client (`[designs] vision_base_url`).
#[derive(Clone)]
pub struct Vision {
    base_url: String,
    model: String,
    http: reqwest::Client,
}

pub const CAPTION_PROMPT: &str = "You are cataloguing website designs for a design reference library. Describe this \
screenshot's visual design in 80-120 words: layout and grid, background and accent colours (hex if you can tell), \
typography (serif / sans / mono, weight, size contrast), imagery and illustration style, notable UI components \
(nav, buttons, cards, pills, tables, code blocks), density and mood. Plain prose. Do not repeat marketing copy \
beyond a few words.";

impl Vision {
    pub fn new(base_url: &str, model: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            http: reqwest::Client::builder().timeout(Duration::from_secs(120)).build().expect("reqwest client"),
        }
    }
    pub fn model(&self) -> &str {
        &self.model
    }

    pub async fn caption_png(&self, png: &[u8]) -> Result<String> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        let body = json!({
            "model": self.model,
            "temperature": 0.1,
            "max_tokens": 320,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": CAPTION_PROMPT},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{b64}")}},
            ]}],
        });
        let url = format!("{}/chat/completions", self.base_url);
        let v: Value = self
            .http
            .post(&url)
            .header("x-tracekit-client", crate::legacy::wire::TRACE_DESIGNS)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .error_for_status()?
            .json()
            .await?;
        let text = v["choices"][0]["message"]["content"].as_str().unwrap_or("").trim().to_string();
        if text.len() < 20 {
            bail!("vision model returned an empty caption");
        }
        Ok(text)
    }
}

/// Caption up to `limit` uncaptioned fold / preview shots. Returns (captioned, failed).
pub async fn caption_pending(pool: &PgPool, root: &Path, vision: &Vision, limit: i64) -> Result<(usize, usize)> {
    let views: Vec<String> = CAPTION_VIEWS.iter().map(|s| s.to_string()).collect();
    let rows: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        "select s.id, s.path, d.capture_dir from wh.design_shots s join wh.designs d on d.id = s.design_id
          where s.caption is null and s.caption_error is null and s.view = any($1) and d.status <> 'retired'
          order by d.created_at, s.path limit $2",
    )
    .bind(&views)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let (mut ok, mut failed) = (0, 0);
    for (id, path, capture) in rows {
        let res: Result<String> = async {
            let capture = capture.context("no capture folder")?;
            if !valid_slug(&capture) || !valid_rel_path(&path) {
                bail!("bad shot path");
            }
            let full = designs_dir(root).join(capture).join(&path);
            let bytes = tokio::fs::read(&full).await.with_context(|| format!("reading {}", full.display()))?;
            if bytes.len() > 12 * 1024 * 1024 {
                bail!("screenshot is {} bytes; too large to caption", bytes.len());
            }
            vision.caption_png(&bytes).await
        }
        .await;
        match res {
            Ok(c) => {
                sqlx::query(
                    "update wh.design_shots set caption = $2, caption_model = $3, captioned_at = now(), caption_error = null,
                       embedding = null where id = $1",
                )
                .bind(id)
                .bind(&c)
                .bind(vision.model())
                .execute(pool)
                .await?;
                ok += 1;
            }
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!(shot = id, "design caption failed: {msg}");
                // Connection problems are retried next round; anything else is recorded.
                if msg.contains("POST ") && (msg.contains("connect") || msg.contains("timed out")) {
                    return Ok((ok, failed + 1));
                }
                sqlx::query("update wh.design_shots set caption_error = $2 where id = $1").bind(id).bind(&msg).execute(pool).await?;
                failed += 1;
            }
        }
    }
    Ok((ok, failed))
}

fn shot_embed_text(title: Option<&str>, view: &str, design: &str, caption: &str) -> String {
    format!("{design} — {} ({view})\n{caption}", title.unwrap_or("page"))
}

/// Embed designs and captioned shots that have no vector (or one from another model).
pub async fn embed_pending(pool: &PgPool, embedder: &Embedder, mid: i32, limit: usize) -> Result<usize> {
    let mut done = 0usize;
    let designs: Vec<(String, String)> = sqlx::query_as(
        "select id::text, search_text from wh.designs where embedding is null or embed_model_id is distinct from $1 limit $2",
    )
    .bind(mid)
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;
    for chunk in designs.chunks(16) {
        let texts: Vec<String> = chunk.iter().map(|(_, t)| t.chars().take(6000).collect()).collect();
        let vecs = embedder.embed(&texts, Kind::Passage).await?;
        for ((id, _), v) in chunk.iter().zip(vecs) {
            sqlx::query("update wh.designs set embedding = $2::extensions.halfvec, embed_model_id = $3 where id = $1::uuid")
                .bind(id)
                .bind(halfvec_literal(&v))
                .bind(mid)
                .execute(pool)
                .await?;
            done += 1;
        }
    }
    let shots: Vec<(i64, Option<String>, String, String, String)> = sqlx::query_as(
        "select s.id, s.page_title, s.view, d.name, s.caption from wh.design_shots s join wh.designs d on d.id = s.design_id
          where s.caption is not null and (s.embedding is null or s.embed_model_id is distinct from $1) limit $2",
    )
    .bind(mid)
    .bind(limit.saturating_sub(done).max(1) as i64)
    .fetch_all(pool)
    .await?;
    for chunk in shots.chunks(16) {
        let texts: Vec<String> = chunk.iter().map(|(_, t, v, d, c)| shot_embed_text(t.as_deref(), v, d, c)).collect();
        let vecs = embedder.embed(&texts, Kind::Passage).await?;
        let ids: Vec<i64> = chunk.iter().map(|r| r.0).collect();
        let lits: Vec<String> = vecs.iter().map(|v| halfvec_literal(v)).collect();
        sqlx::query(
            "update wh.design_shots s set embedding = u.v::extensions.halfvec, embed_model_id = $3
               from unnest($1::bigint[], $2::text[]) as u(id, v) where s.id = u.id",
        )
        .bind(&ids)
        .bind(&lits)
        .bind(mid)
        .execute(pool)
        .await?;
        done += chunk.len();
    }
    Ok(done)
}

// ───────────────────────────── search ─────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DesignSearchParams {
    /// What you want it to look like, in plain words: "dark console with mono labels",
    /// "warm editorial serif", "pricing table with glass cards". Empty = list by filters.
    #[serde(default)]
    pub query: String,
    /// dark | light | both
    #[serde(default)]
    pub theme: Option<String>,
    /// capture | reference | own
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    /// Font family substring, e.g. `mono`, `Inter`, `Space Grotesk`.
    #[serde(default)]
    pub font: Option<String>,
    /// A colour (`#ff5a00`, `rgb(…)`): keep designs whose background or accent is close to it.
    #[serde(default)]
    pub color: Option<String>,
    /// Include drafts (default true; retired never).
    #[serde(default = "yes")]
    pub include_drafts: bool,
    #[serde(default = "default_limit")]
    pub limit: usize,
}
fn yes() -> bool {
    true
}
fn default_limit() -> usize {
    6
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ShotHit {
    pub id: i64,
    pub view: String,
    pub page_title: Option<String>,
    pub page_url: Option<String>,
    pub caption: Option<String>,
    /// lumbridge-libraryd path serving the PNG (bearer token): /v1/designs/<slug>/shots/<id>
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DesignHit {
    pub slug: String,
    pub name: String,
    pub kind: String,
    pub status: String,
    pub site_url: Option<String>,
    pub summary: String,
    pub theme: Option<String>,
    pub bg: Option<String>,
    pub fg: Option<String>,
    pub accent: Option<String>,
    pub fonts: Vec<String>,
    pub radius_px: Option<f32>,
    pub tags: Vec<String>,
    /// Read its files with design_read / warehouse_read repo=<repo_slug>.
    pub repo_slug: Option<String>,
    pub score: f64,
    /// The screenshots that matched best (or the first fold shot).
    pub shots: Vec<ShotHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DesignSearchResult {
    pub hits: Vec<DesignHit>,
    /// Which retrieval lists answered (fts, vector, shot_fts, shot_vector, rerank).
    pub lists: Vec<String>,
}

#[derive(sqlx::FromRow)]
struct Cand {
    id: String,
    slug: String,
    name: String,
    kind: String,
    status: String,
    site_url: Option<String>,
    summary: String,
    theme: Option<String>,
    bg: Option<String>,
    fg: Option<String>,
    accent: Option<String>,
    fonts: Vec<String>,
    radius_px: Option<f32>,
    tags: Vec<String>,
    repo_slug: Option<String>,
}

const RRF_K: f64 = 60.0;
/// (design id, shot id, view, page title, page url, caption)
type ShotLite = (String, i64, String, Option<String>, Option<String>, Option<String>);

pub async fn search(pool: &PgPool, embedder: &Embedder, reranker: &Reranker, mid: Option<i32>, p: &DesignSearchParams) -> Result<DesignSearchResult> {
    if let Some(t) = &p.theme
        && !THEMES.contains(&t.as_str())
    {
        bail!("unknown theme {t:?}: valid themes are dark | light | both");
    }
    if let Some(k) = &p.kind
        && !KINDS.contains(&k.as_str())
    {
        bail!("unknown kind {k:?}: valid kinds are {KINDS:?}");
    }
    let target = match &p.color {
        Some(c) => Some(normalise_hex(Some(c)).with_context(|| format!("unknown colour {c:?}: valid colours are #rrggbb or rgb(...)"))?),
        None => None,
    };
    let limit = p.limit.clamp(1, 30);
    let cands: Vec<Cand> = sqlx::query_as(
        "select d.id::text as id, d.slug, d.name, d.kind, d.status, d.site_url, d.summary, d.theme, d.bg, d.fg, d.accent,
                d.fonts, d.radius_px, d.tags, r.slug as repo_slug
           from wh.designs d left join wh.repos r on r.id = d.repo_id
          where d.status <> 'retired' and ($1 or d.status = 'approved')
            and ($2::text is null or d.theme = $2 or d.theme = 'both')
            and ($3::text is null or d.kind = $3)
            and ($4::text is null or $4 = any(d.tags))
            and ($5::text is null or exists (select 1 from unnest(d.fonts) f where f ilike '%' || $5 || '%'))",
    )
    .bind(p.include_drafts)
    .bind(&p.theme)
    .bind(&p.kind)
    .bind(&p.tag)
    .bind(&p.font)
    .fetch_all(pool)
    .await?;
    let mut cands: Vec<Cand> = match &target {
        Some(t) => cands
            .into_iter()
            .filter(|c| {
                // Ink is nearly always black or white, so it doesn't count.
                [&c.bg, &c.accent]
                    .iter()
                    .filter_map(|x| x.as_deref())
                    .any(|x| color_distance(x, t).is_some_and(|d| d < 70.0))
            })
            .collect(),
        None => cands,
    };
    let ids: Vec<String> = cands.iter().map(|c| c.id.clone()).collect();
    let mut lists = Vec::new();
    let mut score: HashMap<String, f64> = HashMap::new();
    let mut best_shot: HashMap<String, Vec<i64>> = HashMap::new();
    let q = p.query.trim();

    if !q.is_empty() && !ids.is_empty() {
        let add = |score: &mut HashMap<String, f64>, ranked: &[String], w: f64| {
            for (i, id) in ranked.iter().enumerate() {
                *score.entry(id.clone()).or_default() += w / (RRF_K + i as f64 + 1.0) + if i == 0 { 0.002 * w } else { 0.0 };
            }
        };
        let fts: Vec<String> = sqlx::query_scalar(
            "select id::text from wh.designs, websearch_to_tsquery('english', $1) q
              where id = any($2::uuid[]) and tsv @@ q order by ts_rank_cd(tsv, q) desc limit 30",
        )
        .bind(q)
        .bind(&ids)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        if !fts.is_empty() {
            lists.push("fts".to_string());
            add(&mut score, &fts, 1.0);
        }
        let shot_fts: Vec<(String, i64)> = sqlx::query_as(
            "select design_id::text, id from wh.design_shots, websearch_to_tsquery('english', $1) q
              where design_id = any($2::uuid[]) and tsv @@ q order by ts_rank_cd(tsv, q) desc limit 60",
        )
        .bind(q)
        .bind(&ids)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        if !shot_fts.is_empty() {
            lists.push("shot_fts".to_string());
            add(&mut score, &dedupe_designs(&shot_fts, &mut best_shot), 0.8);
        }
        if let Some(mid) = mid {
            match embedder.embed(&[q.to_string()], Kind::Query).await {
                Ok(v) if !v.is_empty() => {
                    let lit = halfvec_literal(&v[0]);
                    let vec: Vec<String> = sqlx::query_scalar(
                        "select id::text from wh.designs where id = any($2::uuid[]) and embed_model_id = $3 and embedding is not null
                          order by embedding operator(extensions.<=>) $1::extensions.halfvec limit 30",
                    )
                    .bind(&lit)
                    .bind(&ids)
                    .bind(mid)
                    .fetch_all(pool)
                    .await?;
                    if !vec.is_empty() {
                        lists.push("vector".to_string());
                        add(&mut score, &vec, 1.0);
                    }
                    let shot_vec: Vec<(String, i64)> = sqlx::query_as(
                        "select design_id::text, id from wh.design_shots
                          where design_id = any($2::uuid[]) and embed_model_id = $3 and embedding is not null
                          order by embedding operator(extensions.<=>) $1::extensions.halfvec limit 60",
                    )
                    .bind(&lit)
                    .bind(&ids)
                    .bind(mid)
                    .fetch_all(pool)
                    .await?;
                    if !shot_vec.is_empty() {
                        lists.push("shot_vector".to_string());
                        add(&mut score, &dedupe_designs(&shot_vec, &mut best_shot), 1.0);
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("design search: embedder unavailable: {e:#}"),
            }
        }
        cands.retain(|c| score.contains_key(&c.id));
        cands.sort_by(|a, b| score[&b.id].total_cmp(&score[&a.id]));
    }

    // Load shots for the top candidates.
    let top: Vec<&Cand> = cands.iter().take((limit * 2).max(10)).collect();
    let top_ids: Vec<String> = top.iter().map(|c| c.id.clone()).collect();
    let shots: Vec<ShotLite> = sqlx::query_as(
        "select design_id::text, id, view, page_title, page_url, caption from wh.design_shots
          where design_id = any($1::uuid[]) and view in ('fold', 'preview')
          order by coalesce(page_n, 'zz'), id",
    )
    .bind(&top_ids)
    .fetch_all(pool)
    .await?;
    let mut by_design: HashMap<&str, Vec<&ShotLite>> = HashMap::new();
    for s in &shots {
        by_design.entry(s.0.as_str()).or_default().push(s);
    }

    // Rerank the fused top on a short card (summary + facets + best caption).
    let mut final_score: Vec<(usize, f64)> = top.iter().enumerate().map(|(i, c)| (i, score.get(&c.id).copied().unwrap_or(0.0))).collect();
    if !q.is_empty() && top.len() > 1 {
        let docs: Vec<String> = top
            .iter()
            .map(|c| {
                let cap = best_shot
                    .get(&c.id)
                    .and_then(|ids| ids.first())
                    .and_then(|sid| shots.iter().find(|s| s.1 == *sid))
                    .or_else(|| by_design.get(c.id.as_str()).and_then(|v| v.first().copied()))
                    .and_then(|s| s.5.clone())
                    .unwrap_or_default();
                format!(
                    "{} design system. {} Theme: {}. Colours: background {}, ink {}, accent {}. Fonts: {}. {}",
                    c.name,
                    c.summary,
                    c.theme.as_deref().unwrap_or("?"),
                    c.bg.as_deref().unwrap_or("?"),
                    c.fg.as_deref().unwrap_or("?"),
                    c.accent.as_deref().unwrap_or("?"),
                    c.fonts.join(", "),
                    cap.chars().take(700).collect::<String>()
                )
            })
            .collect();
        match reranker.rerank(q, &docs).await {
            Ok(hits) => {
                lists.push("rerank".to_string());
                let max_rrf = final_score.iter().map(|x| x.1).fold(0.0, f64::max).max(1e-9);
                let n = hits.len().max(1) as f64;
                for (rank, h) in hits.iter().enumerate() {
                    if let Some(fs) = final_score.iter_mut().find(|x| x.0 == h.index) {
                        // Position-aware blend: retrieval keeps a say, the reranker orders the head.
                        let rr = 1.0 - rank as f64 / n;
                        fs.1 = 0.55 * rr + 0.45 * (fs.1 / max_rrf);
                    }
                }
            }
            Err(e) => tracing::warn!("design search: reranker unavailable: {e:#}"),
        }
        final_score.sort_by(|a, b| b.1.total_cmp(&a.1));
    }

    let hits = final_score
        .into_iter()
        .take(limit)
        .map(|(i, s)| {
            let c = top[i];
            let mut picked: Vec<i64> = best_shot.get(&c.id).cloned().unwrap_or_default();
            picked.truncate(2);
            if picked.is_empty()
                && let Some(first) = by_design.get(c.id.as_str()).and_then(|v| v.first())
            {
                picked.push(first.1);
            }
            let shots = picked
                .iter()
                .filter_map(|sid| shots.iter().find(|x| x.1 == *sid))
                .map(|x| ShotHit {
                    id: x.1,
                    view: x.2.clone(),
                    page_title: x.3.clone(),
                    page_url: x.4.clone(),
                    caption: x.5.clone(),
                    url: format!("/v1/designs/{}/shots/{}", c.slug, x.1),
                })
                .collect();
            DesignHit {
                slug: c.slug.clone(),
                name: c.name.clone(),
                kind: c.kind.clone(),
                status: c.status.clone(),
                site_url: c.site_url.clone(),
                summary: c.summary.clone(),
                theme: c.theme.clone(),
                bg: c.bg.clone(),
                fg: c.fg.clone(),
                accent: c.accent.clone(),
                fonts: c.fonts.clone(),
                radius_px: c.radius_px,
                tags: c.tags.clone(),
                repo_slug: c.repo_slug.clone(),
                score: (s * 1000.0).round() / 1000.0,
                shots,
            }
        })
        .collect();
    Ok(DesignSearchResult { hits, lists })
}

/// Shot hits (design, shot) in rank order → designs in order of their best shot; remembers the
/// matching shots per design.
fn dedupe_designs(rows: &[(String, i64)], best: &mut HashMap<String, Vec<i64>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (d, s) in rows {
        let e = best.entry(d.clone()).or_default();
        if !e.contains(s) {
            e.push(*s);
        }
        if !out.contains(d) {
            out.push(d.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours() {
        assert_eq!(parse_color("rgb(10, 10, 10)"), Some(([10, 10, 10], 1.0)));
        assert_eq!(parse_color("rgba(255, 255, 255, 0.55)").map(|c| c.0), Some([255, 255, 255]));
        assert_eq!(parse_color("#0a0a0a").map(|c| hex(c.0)), Some("#0a0a0a".into()));
        assert_eq!(parse_color("#fff").map(|c| hex(c.0)), Some("#ffffff".into()));
        assert!(parse_color("#ffffff80").unwrap().1 < 0.6);
        assert!(parse_color("oklab(0 0 0 / 0.2)").is_none());
        assert!(color_distance("#000000", "#0a0a0a").unwrap() < 20.0);
        assert!(color_distance("#000000", "#ffffff").unwrap() > 400.0);
    }

    #[test]
    fn measure_tokens() {
        let t = json!({
            "background": [{"v": "oklab(0 0 0 / 0.2)", "c": 9}, {"v": "rgba(0,0,0,0.5)", "c": 5}, {"v": "rgb(10, 10, 10)", "c": 4}],
            "text_color": [{"v": "rgb(153, 213, 42)", "c": 20}, {"v": "rgb(255, 255, 255)", "c": 9}, {"v": "rgb(200, 200, 200)", "c": 8}],
            "font_family": [{"v": "\"Space Mono\", ui-monospace, monospace", "c": 9}, {"v": "system-ui, sans-serif", "c": 5},
                            {"v": "'DM Sans', system-ui", "c": 2}, {"v": "\"space mono\", monospace", "c": 1}],
            "radius": [{"v": "0px", "c": 9}, {"v": "6px", "c": 4}],
        });
        let css = ":root { --w-ink: #fff; --w-accent: #95aba7; }\n@media (prefers-color-scheme: light) {}";
        let m = measure(Some(&t), Some(css));
        assert_eq!(m.bg.as_deref(), Some("#0a0a0a"));
        assert_eq!(m.fg.as_deref(), Some("#ffffff"));
        assert_eq!(m.theme.as_deref(), Some("both"));
        assert_eq!(m.fonts, vec!["Space Mono".to_string(), "DM Sans".to_string()]);
        assert_eq!(m.radius_px, Some(6.0));
        assert_eq!(m.accent.as_deref(), Some("#95aba7"));
        // No accent variable: the most used vivid colour (lime labels) is the accent, not the ink.
        let m = measure(Some(&t), Some(":root { --x-primary: #1e3a5f; --x-accent-hover: #000; }"));
        assert_eq!(m.accent.as_deref(), Some("#99d52a"));
        assert_eq!(m.fg.as_deref(), Some("#ffffff"));
    }

    #[test]
    fn paragraphs() {
        let md = "# Wafer\n\nCaptured 2026.\n\n**Use:** private.\n\n## The feel in one line\nA near-black console\nover a sky.\n\n## Colour\n";
        assert_eq!(first_paragraph(md).as_deref(), Some("A near-black console over a sky."));
        let md2 = "# Design System: Airbnb\n\n> notes\n> more\n\n## 1. Visual Theme & Atmosphere\n\nAirbnb's website is warm and photographic.\n";
        assert_eq!(first_paragraph(md2).as_deref(), Some("Airbnb's website is warm and photographic."));
    }

    #[test]
    fn slugs_and_names() {
        assert!(valid_slug("wafer-ai"));
        assert!(!valid_slug("../x"));
        assert_eq!(bare("designs/wafer-ai"), "wafer-ai");
        assert_eq!(title_from_slug("morphllm-com"), "Morphllm");
        assert_eq!(title_from_slug("relace-ai-old"), "Relace Ai Old");
        assert_eq!(png_size(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\x05\xa0\0\0\x03\x84"), Some((1440, 900)));
    }
}

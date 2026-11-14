//! References → `kb.facts`. A Reference with an `entity:` block lists its numbers as bullets under
//! "Key specs" / "Prices" / "Key facts" (`- **Memory:** 288 GB HBM3e`,
//! `- **Rental:** Vultr $2.59/GPU-hr, on-demand (…, as of 2026-11-08)`). The parser turns each bullet
//! into one fact; bullets holding several amounts (`$5.99 Daytona / $2.95 TensorWave`) go to the
//! local LLM, whose output is accepted only when every value, qualifier and evidence string is in
//! the bullet verbatim. A current fact from a *different* source with a different value is a
//! contradiction: reported, never overwritten.

use super::{Ctx, record, record_finding};
use crate::docs::{self, Fact, FactAction, NewFact, slugify};
use crate::llm::Llm;
use anyhow::Result;
use chrono::NaiveDate;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

const FACT_SECTIONS: [&str; 6] = ["key specs", "specs", "prices", "pricing", "key facts", "facts"];
const SKIP_LABELS: [&str; 5] = ["source", "sources", "see", "see also", "note"];
const EXAMPLES: usize = 60;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProposedFact {
    pub path: String,
    pub entity: String,
    pub attribute: String,
    pub value: String,
    /// add | update | noop
    pub action: String,
    pub confidence: f32,
    pub verified_by: String,
    pub valid_from: Option<NaiveDate>,
    /// parser | heuristic | llm
    pub method: String,
    pub bullet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Contradiction {
    pub entity: String,
    pub attribute: String,
    pub path: String,
    pub current: String,
    pub current_source: Option<String>,
    pub incoming: String,
    pub incoming_source: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FactsReport {
    pub references: usize,
    pub bullets: usize,
    pub parsed: usize,
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
    /// Over `max_facts_per_run`; picked up by the next run.
    pub skipped_cap: usize,
    /// Bullets neither the parser nor the LLM could turn into facts.
    pub unparsed: usize,
    pub llm_extracted: usize,
    pub llm_rejected: usize,
    pub contradictions: Vec<Contradiction>,
    pub examples: Vec<ProposedFact>,
    pub unparsed_examples: Vec<String>,
    pub llm_rejections: Vec<String>,
    pub errors: Vec<String>,
}

// ───────────────────────────── parser ─────────────────────────────

/// One labelled bullet inside a fact section.
#[derive(Debug, Clone, PartialEq)]
pub struct Bullet {
    /// The `##` section, lowercased.
    pub section: String,
    /// The `###` subsection inside it, if any.
    pub sub: Option<String>,
    pub label: String,
    pub value: String,
    /// The whole bullet line.
    pub raw: String,
    /// The label was bold (`**Label:**`); plain `Label:` bullets must parse as numbers.
    pub bold: bool,
}

static BOLD_BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[-*]\s+\*\*(.+?)\*\*\s*(:)?\s*(.*)$").unwrap());
static PLAIN_BULLET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-*]\s+([A-Za-z][A-Za-z0-9 ()/+.,'-]{0,40}?):\s+(.+)$").unwrap());

/// Labelled bullets under the fact sections of a Reference body.
pub fn fact_bullets(body: &str) -> Vec<Bullet> {
    let mut out = Vec::new();
    let mut section: Option<String> = None;
    let mut sub: Option<String> = None;
    let mut in_code = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        if let Some(h) = line.strip_prefix("## ") {
            let h = h.trim().to_lowercase();
            section = FACT_SECTIONS.contains(&h.as_str()).then_some(h);
            sub = None;
            continue;
        }
        if let Some(h) = line.strip_prefix("### ") {
            sub = Some(h.trim().to_string());
            continue;
        }
        if line.starts_with("# ") {
            section = None;
            continue;
        }
        let Some(sec) = &section else { continue };
        let (label, value, bold) = if let Some(c) = BOLD_BULLET.captures(line) {
            let mut label = c[1].trim().to_string();
            let colon_inside = label.ends_with(':');
            if !colon_inside && c.get(2).is_none() {
                continue; // bold phrase, not a label
            }
            label = label.trim_end_matches(':').trim().to_string();
            (label, c[3].trim().to_string(), true)
        } else if let Some(c) = PLAIN_BULLET.captures(line) {
            (c[1].trim().to_string(), c[2].trim().to_string(), false)
        } else {
            continue;
        };
        if label.is_empty() || value.is_empty() || label.contains("[[") || label.contains("](") {
            continue;
        }
        if SKIP_LABELS.contains(&label.to_lowercase().as_str()) {
            continue;
        }
        out.push(Bullet { section: sec.clone(), sub: sub.clone(), label, value, raw: line.trim().to_string(), bold });
    }
    out
}

/// A parsed bullet value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedValue {
    pub value_num: Option<f64>,
    pub value_hi: Option<f64>,
    pub unit: Option<String>,
    pub value_text: Option<String>,
    pub valid_from: Option<NaiveDate>,
    /// Marked `(U)` (unverified) in the note.
    pub unverified: bool,
    /// `~`, `≈`, `about`.
    pub approx: bool,
    /// Who/what the amount belongs to (`Vultr`, `B300`, `Framework Desktop 128GB`, plus a term
    /// like `on-demand`); makes the attribute unique when a label repeats.
    pub qualifier: Option<String>,
    /// First http(s) link in the bullet (a better source than the note's).
    pub link: Option<String>,
    /// Number parsed but with extra prose around it.
    pub heuristic: bool,
    /// First descriptive parenthetical (`3-yr`, `pod`): used to tell apart bullets whose label and
    /// qualifier collide within one note.
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Parse {
    Value(ParsedValue),
    /// Several amounts in one bullet; needs splitting (LLM).
    Multi,
}

static MD_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]*)\]\((https?://[^)\s]+)\)").unwrap());
static ANGLE_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<(https?://[^>\s]+)>").unwrap());
static WIKILINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\[([^\]|#]+)(?:#[^\]|]*)?(?:\|([^\]]+))?\]\]").unwrap());
static ISO_DATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(\d{4})-(\d{2})-(\d{2})\b").unwrap());
static MDY_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(jan|feb|mar|apr|may|jun|jul|aug|sep|sept|oct|nov|dec)[a-z]*\.?\s+(\d{1,2}),?\s+(\d{4})\b").unwrap()
});
static DMY_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(\d{1,2})\s+(jan|feb|mar|apr|may|jun|jul|aug|sep|sept|oct|nov|dec)[a-z]*\.?\s+(\d{4})\b").unwrap()
});
static MONEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(~|≈|about\s+)?\$\s?(\d[\d,]*(?:\.\d+)?)\s?([kKMB]n?\b)?(?:\s?[-–]\s?\$?(\d[\d,]*(?:\.\d+)?)\s?([kKMB]n?\b)?)?").unwrap()
});
static PER_UNIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*/\s*([A-Za-z0-9][A-Za-z0-9-]*(?:\s+tokens)?)").unwrap());
static LEADING_NUM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(~|≈|about\s+)?(\d[\d,]*(?:\.\d+)?)(?:\s?[-–]\s?(\d[\d,]*(?:\.\d+)?))?(?:\s+(.*))?$").unwrap()
});
static UNIT_TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z%][A-Za-z%/]*(?:/[A-Za-z]+)?$").unwrap());
static TERM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9]*(?:-[A-Za-z0-9]+)*$").unwrap());

const UNIT_STOPWORDS: [&str; 12] = ["per", "to", "of", "for", "x", "at", "up", "and", "or", "in", "on", "with"];

fn month(m: &str) -> u32 {
    match &m.to_lowercase()[..3] {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        _ => 12,
    }
}

/// Every full date written in `s` (ISO, `Oct 2 2026`, `2 Oct 2026`).
pub fn dates_in(s: &str) -> Vec<NaiveDate> {
    let mut out = Vec::new();
    for c in ISO_DATE.captures_iter(s) {
        if let Some(d) = NaiveDate::from_ymd_opt(c[1].parse().unwrap_or(0), c[2].parse().unwrap_or(0), c[3].parse().unwrap_or(0)) {
            out.push(d);
        }
    }
    for c in MDY_DATE.captures_iter(s) {
        if let Some(d) = NaiveDate::from_ymd_opt(c[3].parse().unwrap_or(0), month(&c[1]), c[2].parse().unwrap_or(0)) {
            out.push(d);
        }
    }
    for c in DMY_DATE.captures_iter(s) {
        if let Some(d) = NaiveDate::from_ymd_opt(c[3].parse().unwrap_or(0), month(&c[2]), c[1].parse().unwrap_or(0)) {
            out.push(d);
        }
    }
    out
}

/// Markdown → plain text, returning the first http(s) link.
pub fn clean_markdown(v: &str) -> (String, Option<String>) {
    let mut link = MD_LINK.captures(v).map(|c| c[2].to_string());
    let s = MD_LINK.replace_all(v, "$1");
    if link.is_none() {
        link = ANGLE_LINK.captures(&s).map(|c| c[1].to_string());
    }
    let s = ANGLE_LINK.replace_all(&s, "$1");
    let s = WIKILINK.replace_all(&s, |c: &regex::Captures| c.get(2).map(|m| m.as_str()).unwrap_or(&c[1]).to_string());
    let s = s.replace("**", "").replace('`', "");
    (s.split_whitespace().collect::<Vec<_>>().join(" "), link)
}

/// Split top-level parenthesised groups out: `("Vultr $2.59/GPU-hr", ["8-GPU only"])`.
fn split_parens(s: &str) -> (String, Vec<String>) {
    let (mut main, mut metas, mut cur, mut depth) = (String::new(), Vec::new(), String::new(), 0usize);
    for ch in s.chars() {
        match ch {
            '(' => {
                if depth > 0 {
                    cur.push(ch);
                }
                depth += 1;
            }
            ')' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    metas.push(std::mem::take(&mut cur).trim().to_string());
                } else {
                    cur.push(ch);
                }
            }
            _ if depth > 0 => cur.push(ch),
            _ => main.push(ch),
        }
    }
    if !cur.is_empty() {
        main.push_str(&cur); // unbalanced: keep the text
    }
    (main.split_whitespace().collect::<Vec<_>>().join(" "), metas)
}

fn num(s: &str) -> Option<f64> {
    s.replace(',', "").parse().ok()
}

fn mult(s: Option<&str>) -> f64 {
    match s.map(|m| m.trim().to_lowercase()) {
        Some(m) if m == "k" => 1e3,
        Some(m) if m == "m" => 1e6,
        Some(m) if m == "b" || m == "bn" => 1e9,
        _ => 1.0,
    }
}

/// Parse a bullet value (`288 GB HBM3e`, `~$23-30k (U)`, `Vultr $2.59/GPU-hr, on-demand (…)`).
pub fn parse_value(value: &str, label: &str) -> Parse {
    let (clean, link) = clean_markdown(value);
    let (main, metas) = split_parens(&clean);
    let mut p = ParsedValue {
        link,
        unverified: metas.iter().any(|m| m == "U") || clean.contains("(U)"),
        valid_from: dates_in(&clean).into_iter().max(),
        hint: hint_from(&metas),
        ..Default::default()
    };

    let monies: Vec<regex::Captures> = MONEY.captures_iter(&main).collect();
    if monies.len() > 1 {
        return Parse::Multi;
    }
    if let Some(c) = monies.first() {
        let whole = c.get(0).unwrap();
        let (m_lo, m_hi) = (c.get(3).map(|m| m.as_str()), c.get(5).map(|m| m.as_str()));
        let hi_mult = mult(m_hi);
        let lo_mult = if m_lo.is_none() && m_hi.is_some() { hi_mult } else { mult(m_lo) };
        p.approx = c.get(1).is_some();
        p.value_num = num(&c[2]).map(|x| x * lo_mult);
        p.value_hi = c.get(4).and_then(|m| num(m.as_str())).map(|x| x * hi_mult);
        let mut rest = &main[whole.end()..];
        let mut unit = "USD".to_string();
        if let Some(u) = PER_UNIT.captures(rest) {
            unit = format!("USD/{}", u[1].trim());
            rest = &rest[u.get(0).unwrap().end()..];
        }
        p.unit = Some(unit);
        let pre = main[..whole.start()].trim().trim_end_matches([':', ',', '-', '–']).trim();
        let post = rest.trim().trim_start_matches([',', ';', '·', '-', '–']).trim();
        let mut qual: Vec<String> = Vec::new();
        if !pre.is_empty() {
            qual.push(pre.to_string());
        }
        if let Some(term) = post.split(',').next().map(str::trim).filter(|t| TERM.is_match(t) && t.len() <= 20) {
            qual.push(term.to_string());
        }
        if !qual.is_empty() {
            p.qualifier = Some(qual.join(" "));
        }
        let text = [pre, post].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(", ");
        p.heuristic = post.split_whitespace().count() > 4;
        p.value_text = (!text.is_empty()).then(|| crate::docs::truncate_chars(&text, 200).to_string());
        return Parse::Value(p);
    }

    if let Some(c) = LEADING_NUM.captures(&main) {
        let rest = c.get(4).map(|m| m.as_str().trim()).unwrap_or("");
        let mut words = rest.split_whitespace();
        let first = words.next().unwrap_or("").trim_end_matches([',', ';']);
        let unit_ok = first.is_empty() || (UNIT_TOKEN.is_match(first) && !UNIT_STOPWORDS.contains(&first.to_lowercase().as_str()));
        if unit_ok {
            p.approx = c.get(1).is_some();
            p.value_num = num(&c[2]);
            p.value_hi = c.get(3).and_then(|m| num(m.as_str()));
            p.unit = if first.is_empty() { unit_from_label(label) } else { Some(first.to_string()) };
            let tail: Vec<&str> = words.collect();
            if !tail.is_empty() {
                p.value_text = Some(crate::docs::truncate_chars(&tail.join(" "), 200).to_string());
            }
            p.heuristic = tail.len() > 3;
            if p.value_num.is_some() {
                return Parse::Value(p);
            }
        }
    }

    // Text value (vendor, architecture, status, ...): keep the whole cleaned value.
    p.value_text = Some(crate::docs::truncate_chars(&clean, 200).to_string());
    Parse::Value(p)
}

/// The parentheticals that describe the offer rather than source it (first comma part of each).
fn hint_from(metas: &[String]) -> Option<String> {
    let parts: Vec<&str> = metas
        .iter()
        .filter_map(|m| {
            let l = m.to_lowercase();
            if m == "U" || l.contains("as of") || l.starts_with("pricing") || l.contains("http") || !dates_in(m).is_empty() {
                return None;
            }
            let first = m.split(',').next().unwrap_or("").trim();
            (!first.is_empty() && first.len() <= 30).then_some(first)
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// `FP4 dense TFLOP/s` → `TFLOP/s`; `Total params (B)` → `B`.
fn unit_from_label(label: &str) -> Option<String> {
    if let (Some(i), true) = (label.rfind('('), label.ends_with(')')) {
        let u = label[i + 1..label.len() - 1].trim();
        if !u.is_empty() && u.len() <= 12 {
            return Some(u.to_string());
        }
    }
    let last = label.split_whitespace().last()?;
    (last.contains('/') && last.len() <= 12).then(|| last.to_string())
}

/// `Memory bandwidth` → `memory_bandwidth` (parentheses and `$` dropped).
pub fn attr_slug(label: &str) -> String {
    let (main, _) = split_parens(label);
    slugify(&main).replace('-', "_")
}

/// The attribute name for a bullet: `[sub.]label[.qualifier]`, `price_` prefix in price sections.
pub fn attribute_for(b: &Bullet, qualifier: Option<&str>) -> String {
    let mut a = attr_slug(&b.label);
    if a.is_empty() {
        a = "value".into();
    }
    if (b.section == "prices" || b.section == "pricing") && !a.starts_with("price") {
        a = format!("price_{a}");
    }
    if let Some(s) = b.sub.as_deref().map(attr_slug).filter(|s| !s.is_empty()) {
        a = format!("{s}.{a}");
    }
    if let Some(q) = qualifier.map(slugify).filter(|q| !q.is_empty()) {
        a = format!("{a}.{}", crate::docs::truncate_chars(&q, 60).trim_end_matches('-'));
    }
    a
}

// ───────────────────────────── run ─────────────────────────────

struct Candidate {
    path: String,
    entity: String,
    entity_kind: Option<String>,
    bullet: String,
    attribute: String,
    new: NewFact,
    method: &'static str,
    hint: Option<String>,
}

fn value_eq(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0),
        (None, None) => true,
        _ => false,
    }
}

fn same_new(a: &NewFact, b: &NewFact) -> bool {
    value_eq(a.value_num, b.value_num) && value_eq(a.value_hi, b.value_hi) && a.unit == b.unit && a.value_text == b.value_text
}

fn same_value(f: &Fact, n: &NewFact) -> bool {
    value_eq(f.value_num, n.value_num) && value_eq(f.value_hi, n.value_hi) && f.unit == n.unit && f.value_text == n.value_text
}

fn display(n: &NewFact) -> String {
    let f = Fact {
        id: uuid::Uuid::nil(),
        entity: String::new(),
        entity_name: String::new(),
        attribute: String::new(),
        value_num: n.value_num,
        value_hi: n.value_hi,
        unit: n.unit.clone(),
        value_text: n.value_text.clone(),
        valid_from: None,
        valid_to: None,
        recorded_at: chrono::Utc::now(),
        source_uri: None,
        source_title: None,
        confidence: None,
        verified_by: None,
        superseded_by: None,
        note: None,
    };
    f.value_display()
}

#[derive(Deserialize)]
struct LlmFacts {
    facts: Vec<LlmFact>,
}
#[derive(Deserialize)]
struct LlmFact {
    qualifier: String,
    value: f64,
    value_hi: Option<f64>,
    unit: String,
    evidence: String,
}

fn llm_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {"facts": {"type": "array", "maxItems": 12, "items": {
            "type": "object",
            "properties": {
                "qualifier": {"type": "string"},
                "value": {"type": "number"},
                "value_hi": {"type": ["number", "null"]},
                "unit": {"type": "string"},
                "evidence": {"type": "string"}
            },
            "required": ["qualifier", "value", "value_hi", "unit", "evidence"],
            "additionalProperties": false
        }}},
        "required": ["facts"],
        "additionalProperties": false
    })
}

const LLM_SYSTEM: &str = "You split one markdown bullet from a hardware/pricing reference note into separate facts. \
Only report amounts literally written in the bullet; never compute, convert, round or infer. For each amount give: \
qualifier = the provider, product, plan or direction it belongs to, copied from the bullet (e.g. \"Vultr\", \"TensorWave contract\", \"in\", \"out\", \"cached\", \"512GB\"); \
every fact from one bullet must have a different qualifier, so include the words that tell the amounts apart (input vs output, new vs used, plan names); \
value = the number exactly as written (expand a k suffix only if written, e.g. 30k = 30000); value_hi = upper end of a range or null; \
unit = USD for dollar prices, plus a per-unit suffix only if written (e.g. USD/GPU-hr); \
evidence = the exact substring of the bullet stating this amount. If unsure, return no facts.";

static EVIDENCE_NUM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d[\d,]*(?:\.\d+)?)(?:\s?[-–]\s?\$?(\d[\d,]*(?:\.\d+)?))?\s?([kKmMbB](?:n)?)?\b").unwrap());

/// Is `x` written in `evidence`? Numbers are compared by value (`8.60` = 8.6, `30k` = 30000; a
/// range suffix applies to both ends, `$23-30k` = 23000..30000).
fn number_grounded(x: f64, evidence: &str) -> bool {
    EVIDENCE_NUM.captures_iter(evidence).any(|c| {
        let m = mult(c.get(3).map(|m| m.as_str()));
        [c.get(1), c.get(2)].into_iter().flatten().filter_map(|g| num(g.as_str())).any(|n| value_eq(Some(n), Some(x)) || (m != 1.0 && value_eq(Some(n * m), Some(x))))
    })
}

/// Validate LLM facts against the bullet; returns accepted (qualifier, fact) pairs and rejections.
/// (qualifier, value, value_hi, unit)
type Grounded = (String, f64, Option<f64>, String);

fn ground_llm(b: &Bullet, clean_value: &str, out: LlmFacts) -> (Vec<Grounded>, Vec<String>) {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let hay = norm(clean_value);
    let hay_l = hay.to_lowercase();
    let label_l = b.label.to_lowercase();
    let (mut ok, mut bad) = (Vec::new(), Vec::new());
    for f in out.facts {
        let ev = norm(&f.evidence);
        let why = if ev.is_empty() || !hay.contains(&ev) {
            Some("evidence not verbatim in bullet")
        } else if !number_grounded(f.value, &ev) {
            Some("value not written in evidence")
        } else if f.value_hi.is_some_and(|h| !number_grounded(h, &ev)) {
            Some("value_hi not written in evidence")
        } else if f.qualifier.trim().is_empty() || slugify(&f.qualifier).split('-').any(|t| !hay_l.contains(t)) {
            Some("qualifier not in bullet")
        } else {
            let unit = f.unit.trim().replace('$', "USD");
            let extra: Vec<&str> = unit.split('/').skip(1).collect();
            let extra_missing = extra
                .iter()
                .flat_map(|u| u.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(str::to_lowercase).collect::<Vec<_>>())
                .any(|t| !hay_l.contains(&t) && !label_l.contains(&t));
            let bare_missing = !unit.starts_with("USD") && !hay_l.contains(&unit.to_lowercase());
            (unit.is_empty() || extra_missing || bare_missing).then_some("unit not in bullet")
        };
        match why {
            Some(w) => bad.push(format!("{} → {:?} {} {} ({w})", b.raw, f.qualifier, f.value, f.unit)),
            None => ok.push((f.qualifier.trim().to_string(), f.value, f.value_hi, f.unit.trim().replace('$', "USD"))),
        }
    }
    // Two different amounts under one qualifier (input vs output not told apart): drop both.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for g in &ok {
        *counts.entry(slugify(&g.0)).or_default() += 1;
    }
    let (ok, ambiguous): (Vec<Grounded>, Vec<Grounded>) = ok.into_iter().partition(|g| counts[&slugify(&g.0)] == 1);
    for g in ambiguous {
        bad.push(format!("{} → {:?} {} {} (qualifier shared with another amount)", b.raw, g.0, g.1, g.3));
    }
    (ok, bad)
}

pub async fn run(ctx: &Ctx<'_>, llm: Option<&Llm>, dry: bool, scope: Option<&[String]>) -> Result<FactsReport> {
    let mut rep = FactsReport::default();
    let rows = sqlx::query(
        "select d.path, d.title, d.body, d.frontmatter from kb.documents d
         where d.layer = 'reference' and d.frontmatter ? 'entity' and SCOPE order by d.path"
            .replace("SCOPE", super::SCOPE_SQL)
            .as_str(),
    )
    .bind(scope)
    .fetch_all(ctx.pool)
    .await?;
    let llm_cap = (ctx.cfg.max_llm_calls / 3).max(1);
    let mut llm_used = 0usize;
    let mut cands: Vec<Candidate> = Vec::new();

    for r in &rows {
        let path: String = r.get("path");
        let title: String = r.get("title");
        let body: String = r.get("body");
        let fm: serde_json::Value = r.get("frontmatter");
        let ent = &fm["entity"];
        let slug = ent["slug"].as_str().map(str::to_string).filter(|s| !s.is_empty()).unwrap_or_else(|| slugify(&title));
        if slug.is_empty() {
            continue;
        }
        let kind = ent["kind"].as_str().map(str::to_string);
        let note_source = fm["source"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        let has_vendor = fm.get("vendor").is_some();
        rep.references += 1;

        let mut per_note: Vec<Candidate> = Vec::new();
        for b in fact_bullets(&body) {
            rep.bullets += 1;
            let base = |p: &ParsedValue, method: &'static str| {
                let community = p.unverified || b.sub.as_deref().is_some_and(|s| s.to_lowercase().contains("inferencex")) || (!has_vendor && p.link.is_none());
                let mut conf: f32 = match method {
                    "llm" => 0.6,
                    "heuristic" => 0.75,
                    _ if p.value_num.is_none() => 0.85,
                    _ => 0.9,
                };
                if p.approx {
                    conf -= 0.1;
                }
                if p.unverified {
                    conf = conf.min(0.5);
                }
                NewFact {
                    entity: slug.clone(),
                    entity_kind: kind.clone(),
                    attribute: String::new(),
                    value_num: p.value_num,
                    value_hi: p.value_hi,
                    unit: p.unit.clone(),
                    value_text: p.value_text.clone(),
                    valid_from: p.valid_from,
                    valid_to: None,
                    source_uri: Some(p.link.clone().or_else(|| note_source.clone()).unwrap_or_else(|| format!("vault:{path}"))),
                    source_title: Some(if p.link.is_some() { format!("link in {path}") } else { title.clone() }),
                    confidence: Some(conf),
                    verified_by: Some(if community { "community" } else { "vendor" }.into()),
                    note: Some(format!("librarian ({method}) from {path}: {}", crate::docs::truncate_chars(&b.raw, 240))),
                }
            };
            match parse_value(&b.value, &b.label) {
                Parse::Value(p) => {
                    if !b.bold && p.value_num.is_none() {
                        continue; // plain `Label: prose` bullets are not facts
                    }
                    let method = if p.heuristic { "heuristic" } else { "parser" };
                    let mut n = base(&p, method);
                    n.attribute = attribute_for(&b, p.qualifier.as_deref());
                    rep.parsed += 1;
                    per_note.push(Candidate { path: path.clone(), entity: slug.clone(), entity_kind: kind.clone(), bullet: b.raw.clone(), attribute: n.attribute.clone(), new: n, method, hint: p.hint.clone() });
                }
                Parse::Multi => {
                    let Some(l) = llm.filter(|_| llm_used < llm_cap) else {
                        rep.unparsed += 1;
                        if rep.unparsed_examples.len() < EXAMPLES {
                            rep.unparsed_examples.push(format!("{path}: {}", b.raw));
                        }
                        continue;
                    };
                    llm_used += 1;
                    let (clean, link) = clean_markdown(&b.value);
                    let user = format!("Entity: {title}\nField: {}\nBullet: {clean}", b.label);
                    match l.json::<LlmFacts>(LLM_SYSTEM, &user, "bullet_facts", llm_schema()).await {
                        Ok(out) => {
                            let (ok, bad) = ground_llm(&b, &clean, out);
                            rep.llm_rejected += bad.len();
                            rep.llm_rejections.extend(bad.into_iter().take(EXAMPLES.saturating_sub(rep.llm_rejections.len())));
                            if ok.is_empty() {
                                rep.unparsed += 1;
                            }
                            for (q, v, hi, unit) in ok {
                                let p = ParsedValue {
                                    value_num: Some(v),
                                    value_hi: hi,
                                    unit: Some(unit),
                                    value_text: Some(q.clone()),
                                    valid_from: dates_in(&clean).into_iter().max(),
                                    unverified: clean.contains("(U)"),
                                    link: link.clone(),
                                    ..Default::default()
                                };
                                let mut n = base(&p, "llm");
                                n.verified_by = Some("community".into());
                                n.attribute = attribute_for(&b, Some(&q));
                                rep.llm_extracted += 1;
                                per_note.push(Candidate { path: path.clone(), entity: slug.clone(), entity_kind: kind.clone(), bullet: b.raw.clone(), attribute: n.attribute.clone(), new: n, method: "llm", hint: None });
                            }
                        }
                        Err(e) => {
                            rep.unparsed += 1;
                            rep.errors.push(format!("LLM on {path}: {e:#}"));
                        }
                    }
                }
            }
        }

        // Same attribute twice in one note: identical → keep one; different → contradiction.
        let mut by_attr: HashMap<String, Vec<Candidate>> = HashMap::new();
        let mut order = Vec::new();
        for c in per_note {
            if !by_attr.contains_key(&c.attribute) {
                order.push(c.attribute.clone());
            }
            by_attr.entry(c.attribute.clone()).or_default().push(c);
        }
        for a in order {
            let mut group = by_attr.remove(&a).unwrap_or_default();
            let first = group.remove(0);
            if !group.iter().any(|c| !same_new(&c.new, &first.new)) {
                cands.push(first);
                continue;
            }
            // Distinct parentheticals (`3-yr` vs `5-yr`) make them distinct facts.
            let mut all: Vec<Candidate> = std::iter::once(first).chain(group).collect();
            let hints: HashSet<String> = all.iter().filter_map(|c| c.hint.as_deref().map(slugify)).filter(|h| !h.is_empty()).collect();
            if hints.len() == all.len() {
                for c in all.iter_mut() {
                    let h = slugify(c.hint.as_deref().unwrap_or_default());
                    c.attribute = format!("{a}.{}", crate::docs::truncate_chars(&h, 40).trim_end_matches('-'));
                    c.new.attribute = c.attribute.clone();
                }
                cands.extend(all);
                continue;
            }
            let first = &all[0];
            let other = all[1..].iter().find(|c| !same_new(&c.new, &first.new)).unwrap_or(&all[1]);
            rep.contradictions.push(Contradiction {
                entity: first.entity.clone(),
                attribute: a.clone(),
                path: first.path.clone(),
                current: display(&first.new),
                current_source: first.new.source_uri.clone(),
                incoming: display(&other.new),
                incoming_source: other.new.source_uri.clone(),
            });
        }
    }

    // Compare with what kb.facts already says; write (unless dry) within the cap.
    let mut current: HashMap<String, HashMap<String, Fact>> = HashMap::new();
    let mut written = 0usize;
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for c in cands {
        if !seen.insert((c.entity.clone(), c.attribute.clone())) {
            continue;
        }
        if !current.contains_key(&c.entity) {
            let fs = docs::facts(ctx.pool, &c.entity, None, None, false).await.unwrap_or_default();
            current.insert(c.entity.clone(), fs.into_iter().map(|f| (f.attribute.clone(), f)).collect());
        }
        let cur = current.get(&c.entity).and_then(|m| m.get(&c.attribute));
        let action = match cur {
            Some(f) if same_value(f, &c.new) => "noop",
            Some(f) if f.source_uri.is_some() && f.source_uri != c.new.source_uri => {
                let k = Contradiction {
                    entity: c.entity.clone(),
                    attribute: c.attribute.clone(),
                    path: c.path.clone(),
                    current: f.value_display(),
                    current_source: f.source_uri.clone(),
                    incoming: display(&c.new),
                    incoming_source: c.new.source_uri.clone(),
                };
                rep.contradictions.push(k);
                continue;
            }
            Some(_) => "update",
            None => "add",
        };
        if action == "noop" {
            rep.unchanged += 1;
            continue;
        }
        if written >= ctx.cfg.max_facts_per_run {
            rep.skipped_cap += 1;
            continue;
        }
        written += 1;
        let proposed = ProposedFact {
            path: c.path.clone(),
            entity: c.entity.clone(),
            attribute: c.attribute.clone(),
            value: display(&c.new),
            action: action.into(),
            confidence: c.new.confidence.unwrap_or(0.0),
            verified_by: c.new.verified_by.clone().unwrap_or_default(),
            valid_from: c.new.valid_from,
            method: c.method.into(),
            bullet: c.bullet.clone(),
        };
        if dry {
            if action == "add" { rep.added += 1 } else { rep.updated += 1 }
        } else {
            let mut nf = c.new.clone();
            nf.entity_kind = c.entity_kind.clone().or(Some("concept".into()));
            match docs::add_fact(ctx.pool, nf).await {
                Ok(o) => {
                    let verb = match o.action {
                        FactAction::Added => {
                            rep.added += 1;
                            "added"
                        }
                        FactAction::Updated => {
                            rep.updated += 1;
                            "updated"
                        }
                        FactAction::Noop => {
                            rep.unchanged += 1;
                            "unchanged"
                        }
                    };
                    if o.action != FactAction::Noop {
                        let summary = format!("fact {verb}: {} {} = {} (from {})", c.entity, c.attribute, proposed.value, c.path);
                        record(
                            ctx,
                            "librarian.fact",
                            "docs",
                            &format!("{}/{}", c.entity, c.attribute),
                            true,
                            &summary,
                            json!({"id": o.id, "superseded": o.superseded, "path": c.path, "method": c.method,
                                   "confidence": proposed.confidence, "verified_by": proposed.verified_by}),
                        )
                        .await;
                    }
                }
                Err(e) => rep.errors.push(format!("{} {}: {e:#}", c.entity, c.attribute)),
            }
        }
        if rep.examples.len() < EXAMPLES {
            rep.examples.push(proposed);
        }
    }

    for k in &rep.contradictions {
        let summary = format!(
            "contradiction: {} {} is {} ({}) but {} says {}",
            k.entity,
            k.attribute,
            k.current,
            k.current_source.as_deref().unwrap_or("?"),
            k.path,
            k.incoming
        );
        record_finding(ctx, "librarian.contradiction", &format!("{}/{}", k.entity, k.attribute), &summary, serde_json::to_value(k).unwrap_or_default()).await;
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(v: &str) -> ParsedValue {
        match parse_value(v, "x") {
            Parse::Value(p) => p,
            Parse::Multi => panic!("multi: {v}"),
        }
    }

    #[test]
    fn bullets_in_fact_sections_only() {
        let body = "Intro\n\n## Key specs\n\nSource: x\n\n- **Vendor:** AMD\n- **Memory:** 288 GB HBM3e\n- Memory: 144 GB HBM\n- not a label line\n- **Bold phrase** without colon\n\n### InferenceX spec sheet\n\n- **Bandwidth:** 8 TB/s\n\n## Software\n\n- **Engine:** vLLM\n\n## Prices\n\n- **New:** ~$23-30k (U)\n```\n- **Code:** 1\n```\n";
        let b = fact_bullets(body);
        let labels: Vec<_> = b.iter().map(|x| (x.label.as_str(), x.sub.is_some())).collect();
        assert_eq!(labels, vec![("Vendor", false), ("Memory", false), ("Memory", false), ("Bandwidth", true), ("New", false)]);
        assert!(!b[2].bold);
        assert_eq!(b[4].section, "prices");
    }

    #[test]
    fn numbers_and_units() {
        let p = val("288 GB HBM3e");
        assert_eq!((p.value_num, p.unit.as_deref(), p.value_text.as_deref()), (Some(288.0), Some("GB"), Some("HBM3e")));
        let p = val("0.273 TB/s");
        assert_eq!((p.value_num, p.unit.as_deref()), (Some(0.273), Some("TB/s")));
        let p = val("128 GB LPDDR5X-8000, 256-bit");
        assert_eq!((p.value_num, p.unit.as_deref()), (Some(128.0), Some("GB")));
        assert_eq!(val("12.1").value_num, Some(12.1));
        assert_eq!(parse_value("10066", "FP4 dense TFLOP/s"), Parse::Value(ParsedValue { value_num: Some(10066.0), unit: Some("TFLOP/s".into()), ..Default::default() }));
        let p = val("Infinity Fabric 7x153.6 GB/s + PCIe 5");
        assert!(p.value_num.is_none());
        assert_eq!(p.value_text.as_deref(), Some("Infinity Fabric 7x153.6 GB/s + PCIe 5"));
        assert!(val("1 per box (network clusters only)").value_num.is_none());
        assert!(val("7x153.6 GB/s").value_num.is_none());
        assert_eq!(val("yes").value_text.as_deref(), Some("yes"));
    }

    #[test]
    fn money() {
        let p = val("$6,950 (Oct 2 2026)");
        assert_eq!((p.value_num, p.unit.as_deref()), (Some(6950.0), Some("USD")));
        assert_eq!(p.valid_from, NaiveDate::from_ymd_opt(2026, 10, 2));
        let p = val("~$23-30k (U)");
        assert_eq!((p.value_num, p.value_hi), (Some(23000.0), Some(30000.0)));
        assert!(p.unverified && p.approx);
        let p = val("Vultr $2.59/GPU-hr, on-demand, min 8 GPU ([pricing](https://www.vultr.com/x), as of 2026-11-08)");
        assert_eq!((p.value_num, p.unit.as_deref()), (Some(2.59), Some("USD/GPU-hr")));
        assert_eq!(p.qualifier.as_deref(), Some("Vultr on-demand"));
        assert_eq!(p.link.as_deref(), Some("https://www.vultr.com/x"));
        assert_eq!(p.valid_from, NaiveDate::from_ymd_opt(2026, 11, 8));
        let p = val("CoreWeave $4.25/GPU-hr (1-yr reserve ($7.20 on-demand))");
        assert_eq!((p.value_num, p.qualifier.as_deref()), (Some(4.25), Some("CoreWeave")));
        let p = val("Framework Desktop 128GB $3,449 (from $1,999)");
        assert_eq!((p.value_num, p.qualifier.as_deref()), (Some(3449.0), Some("Framework Desktop 128GB")));
        assert_eq!(parse_value("$5.99 Daytona / $2.95 TensorWave contract", "Rent"), Parse::Multi);
        let p = val("B300 $2.99/GPU-hr (3-yr, 256-GPU min) (`hardware.ts`, 2026-11-07)");
        assert_eq!((p.qualifier.as_deref(), p.hint.as_deref()), (Some("B300"), Some("3-yr")));
        assert_eq!(val("Vultr $2.59/GPU-hr ([pricing](https://x.invalid), as of 2026-11-08)").hint, None);
    }

    #[test]
    fn attributes() {
        let b = Bullet { section: "prices".into(), sub: None, label: "Rental".into(), value: String::new(), raw: String::new(), bold: true };
        assert_eq!(attribute_for(&b, Some("Vultr on-demand")), "price_rental.vultr-on-demand");
        let b = Bullet { section: "key specs".into(), sub: Some("InferenceX spec sheet".into()), label: "Memory bandwidth".into(), value: String::new(), raw: String::new(), bold: true };
        assert_eq!(attribute_for(&b, None), "inferencex_spec_sheet.memory_bandwidth");
        assert_eq!(attr_slug("Rent, $/GPU-hr (example.com hardware page)"), "rent_gpu_hr");
    }

    #[test]
    fn llm_grounding() {
        let b = Bullet { section: "prices".into(), sub: None, label: "Rent".into(), value: String::new(), raw: "- **Rent:** $5.99 Daytona / $2.95 TensorWave contract".into(), bold: true };
        let clean = "$5.99 Daytona / $2.95 TensorWave contract";
        let out = LlmFacts {
            facts: vec![
                LlmFact { qualifier: "Daytona".into(), value: 5.99, value_hi: None, unit: "USD".into(), evidence: "$5.99 Daytona".into() },
                LlmFact { qualifier: "TensorWave contract".into(), value: 2.95, value_hi: None, unit: "USD".into(), evidence: "$2.95 TensorWave contract".into() },
                LlmFact { qualifier: "Lambda".into(), value: 2.95, value_hi: None, unit: "USD".into(), evidence: "$2.95 TensorWave contract".into() },
                LlmFact { qualifier: "Daytona".into(), value: 6.0, value_hi: None, unit: "USD".into(), evidence: "$5.99 Daytona".into() },
                LlmFact { qualifier: "Daytona".into(), value: 5.99, value_hi: None, unit: "USD/GPU-hr".into(), evidence: "$5.99 Daytona".into() },
                LlmFact { qualifier: "Daytona".into(), value: 5.99, value_hi: None, unit: "USD".into(), evidence: "$5.99 at Daytona".into() },
            ],
        };
        let (ok, bad) = ground_llm(&b, clean, out);
        assert_eq!(ok.len(), 2, "{bad:?}");
        assert_eq!(bad.len(), 4);
        let out = LlmFacts {
            facts: vec![
                LlmFact { qualifier: "NanoGPT".into(), value: 0.2, value_hi: None, unit: "USD".into(), evidence: "NanoGPT $0.2 in".into() },
                LlmFact { qualifier: "NanoGPT".into(), value: 0.8, value_hi: None, unit: "USD".into(), evidence: "$0.8 out".into() },
            ],
        };
        let (ok, bad) = ground_llm(&b, "NanoGPT $0.2 in / $0.8 out", out);
        assert!(ok.is_empty() && bad.len() == 2, "{ok:?}");
        assert!(number_grounded(30000.0, "~$23-30k"));
        assert!(!number_grounded(5.9, "$5.99"));
        assert!(number_grounded(8.6, "$8.60 OCI"));
        assert!(number_grounded(4.0, "$4.00/hr"));
        assert!(!number_grounded(4.0, "$4.05/hr"));
        assert!(number_grounded(2.0, "2.0 TB"));
    }
}

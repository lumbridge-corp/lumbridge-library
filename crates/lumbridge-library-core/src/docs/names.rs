//! Look-alike name guard: a question that names a specific model/version the vault has no page for
//! ("Intel Gaudi 5") still gets confident hits about a sibling ("Intel Gaudi 3", rerank 0.95), because
//! the reranker reads both as "the Gaudi page". This module finds versioned names in the question,
//! checks them against every document title / alias / entity name, and reports the first one that is
//! missing while a sibling of the same family (different number, suffix or qualifier) exists.
//!
//! Deterministic and cheap: one SQL query (only when the question contains a versioned name), then
//! pure string work. Names are compared as *atoms* (letter runs and number runs), so case, spaces and
//! hyphens do not matter: `MI355X` = `MI 355X` = `mi355x` = `[mi, 355, x]`.

use super::truncate_chars;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Row};
use std::collections::HashSet;

/// Closest sibling pages reported.
const MAX_CLOSEST: usize = 3;
/// Question text scanned for names.
const MAX_TEXT_CHARS: usize = 2048;

/// The question names a versioned thing the vault has no page for, but a sibling exists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NameMismatch {
    /// The name as written in the question (with its vendor word when the sibling shares it).
    pub asked: String,
    /// Best sibling pages: same family, different version; closest number first.
    pub closest: Vec<NamedDoc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NamedDoc {
    pub title: String,
    pub path: String,
}

impl NameMismatch {
    /// One-line warning for `note` / the head of `context_md`.
    pub fn note(&self) -> String {
        let closest = self.closest.iter().map(|d| format!("{} ({})", d.title, d.path)).collect::<Vec<_>>().join(", ");
        format!(
            "The vault has no page for {}; closest: {closest}. These results are about a different model, not {}.",
            self.asked, self.asked
        )
    }
}

/// A letter run or a number run (`4.1` is one number; `4,699` is one number with `comma`).
#[derive(Debug, Clone)]
struct Atom {
    /// Lowercased text.
    t: String,
    num: bool,
    comma: bool,
    start: usize,
    end: usize,
    /// First letter was uppercase in the original.
    upper: bool,
}

fn atomize(s: &str) -> Vec<Atom> {
    let b: Vec<(usize, char)> = s.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let (start, c) = b[i];
        if c.is_ascii_digit() {
            let mut j = i;
            let mut comma = false;
            while j < b.len() {
                let ch = b[j].1;
                if ch.is_ascii_digit() {
                    j += 1;
                } else if (ch == '.' || ch == ',') && j + 1 < b.len() && b[j + 1].1.is_ascii_digit() {
                    comma |= ch == ',';
                    j += 1;
                } else {
                    break;
                }
            }
            let end = b.get(j).map_or(s.len(), |x| x.0);
            out.push(Atom { t: s[start..end].to_string(), num: true, comma, start, end, upper: false });
            i = j;
        } else if c.is_alphabetic() {
            let mut j = i;
            while j < b.len() && b[j].1.is_alphabetic() {
                j += 1;
            }
            let end = b.get(j).map_or(s.len(), |x| x.0);
            out.push(Atom { t: s[start..end].to_lowercase(), num: false, comma: false, start, end, upper: c.is_uppercase() });
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Text between atom `i-1` and atom `i` (empty = glued, as in `H100` or `355X`).
fn gap<'a>(s: &'a str, a: &[Atom], i: usize) -> &'a str {
    if i == 0 { &s[..a[0].start] } else { &s[a[i - 1].end..a[i].start] }
}

/// Separators that keep two atoms in one name: spaces, `-`, `+` (`Max+ 395`), `_`.
fn soft(g: &str) -> bool {
    g.chars().all(|c| c.is_whitespace() || matches!(c, '-' | '+' | '_'))
}

/// A number followed by one of these is a quantity (`273 GB/s`, `120W`, `3Q26`, `128K`), not a version.
const UNITS: &[&str] = &[
    "gb", "tb", "mb", "kb", "pb", "gib", "tib", "mib", "gbps", "tbps", "mbps", "gbs", "tbs", "bit", "bits", "byte",
    "bytes", "w", "kw", "mw", "gw", "watt", "watts", "kwh", "mwh", "nm", "mm", "ms", "us", "ns", "s", "sec", "secs",
    "hz", "khz", "mhz", "ghz", "flops", "tflops", "pflops", "eflops", "tops", "petaflops", "teraflops", "k", "m", "t",
    "bn", "x", "q", "h", "hr", "hrs", "hour", "hours", "min", "mins", "minute", "minutes", "second", "seconds", "day",
    "days", "week", "weeks", "month", "months", "yr", "yrs", "year", "years", "st", "nd", "rd", "th", "am", "pm",
    "gpu", "gpus", "chip", "chips", "card", "cards", "node", "nodes", "rack", "racks", "token", "tokens", "tok", "tps",
    "users", "seats", "usd", "dollars", "dollar", "cents", "percent", "pct", "c", "lanes", "ports", "cores", "layers",
    "experts", "params", "parameters", "million", "billion", "trillion", "thousand", "times", "per", "dimm", "dimms",
];

/// Words that are not a product family when they come before a number (`top 10`, `November 3`, `on 5`).
const WORD_STOP: &[&str] = &[
    "the", "a", "an", "of", "for", "in", "on", "at", "to", "by", "with", "vs", "versus", "and", "or", "than", "about",
    "top", "last", "first", "next", "over", "under", "around", "per", "is", "are", "was", "were", "be", "been", "has",
    "have", "had", "get", "gets", "got", "from", "into", "up", "down", "only", "just", "all", "each", "every", "any",
    "some", "most", "least", "more", "less", "many", "much", "how", "what", "which", "who", "when", "where", "why",
    "does", "do", "did", "can", "could", "would", "should", "will", "my", "your", "our", "their", "his", "her", "its",
    "it", "this", "that", "these", "those", "page", "chapter", "step", "part", "section", "item", "number", "no", "nr",
    "version", "ver", "rev", "revision", "gen", "generation", "phase", "tier", "level", "round", "stage", "quarter",
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "sept", "oct", "nov", "dec", "january", "february",
    "march", "april", "june", "july", "august", "september", "october", "november", "december", "monday", "tuesday",
    "wednesday", "thursday", "friday", "saturday", "sunday", "price", "prices", "cost", "costs", "bandwidth", "memory",
    "power", "draw", "draws", "use", "uses", "using", "run", "runs", "running", "buy", "rent", "rental", "cheapest",
    "best", "worst", "new", "used", "old", "latest", "nearly", "approx", "roughly", "almost", "plan", "model",
    "models", "chip", "chips", "gpu", "gpus", "card", "cards", "node", "nodes", "server", "servers", "rack", "racks",
    "token", "tokens", "batch", "size", "context", "length", "x", "q", "fy", "cy", "pp", "tp", "ep", "dp", "rank",
    "need", "needs", "take", "takes", "make", "makes", "say", "says", "said", "like", "issue", "vol", "volume",
    "episode", "season", "table", "figure", "fig", "line", "lines", "row", "column",
    "slot", "slots", "port", "lane", "socket", "sockets", "core", "layer", "expert", "users", "people", "seats",
];

/// Glued prefixes that are formats or interfaces, not products (`FP8`, `TP4`, `HBM4`, `PCIe5`, `NVL72`).
const GLUED_STOP: &[&str] = &[
    "fp", "bf", "int", "uint", "nvfp", "mxfp", "q", "x", "tp", "pp", "ep", "dp", "hbm", "ddr", "lpddr", "gddr", "pcie",
    "gen", "nvl", "usb", "cuda", "sm", "fy", "cy",
];

/// Words after a version that make it a different product (`GB10 Ultra`, `M5 Max`, `RTX 4090 Ti`).
const QUALIFIERS: &[&str] = &["ultra", "pro", "max", "mini", "plus", "flash", "next", "lite", "nano", "turbo", "super", "ti", "duo", "air"];
/// Qualifiers that count even in lowercase (the others are ordinary words: "max power", "pro tip").
const LOWER_QUALIFIERS: &[&str] = &["ultra", "ti"];

/// A versioned name found in a question.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Asked {
    /// Family atom right before the number (`gaudi`, `h`, `mi`, `v`); shifted past a qualifier
    /// (`Rubin Ultra 2` → `rubin` with qualifier `ultra`).
    pub family: String,
    /// The family is glued to the number (`H300`, `V5`), as opposed to a word (`Gaudi 5`).
    pub glued: bool,
    pub num: String,
    /// Letters glued after the number (`x` in `MI600X`, `dt` in `950DT`).
    pub suffix: String,
    pub qualifiers: Vec<String>,
    /// The word before the family (`intel` in `Intel Gaudi 5`, `deepseek` in `DeepSeek V6`).
    pub context: Option<String>,
    /// The family word was capitalised (or the question has no capitals at all).
    pub name_like: bool,
    /// Atoms from the family through suffix/qualifiers, as written in order (for the hit-text check).
    pub seq: Vec<String>,
    /// Byte range of the name in the question.
    pub span: (usize, usize),
    /// Up to two words before the family, nearest first, with their start offsets (`intel`, or
    /// `tpu` then `google`): shown in the warning when the sibling shares them.
    pub lead: Vec<(String, usize)>,
}

fn is_year(n: &str) -> bool {
    n.len() == 4 && n.chars().all(|c| c.is_ascii_digit()) && (1900..=2099).contains(&n.parse::<u32>().unwrap_or(0))
}

/// Versioned names in `q`, in order: a number run preceded by a letter run (glued like `H300` or as a
/// word like `Gaudi 5`), minus quantities (`273 GB/s`, `$1.98`, `50%`, `4,699`, years) and minus
/// stopword "families" (`top 10`, `November 3`, `FP8`).
pub(crate) fn asked_names(q: &str) -> Vec<Asked> {
    let q = truncate_chars(q, MAX_TEXT_CHARS);
    let a = atomize(q);
    let any_upper = q.chars().any(char::is_uppercase);
    let mut out: Vec<Asked> = Vec::new();
    for i in 1..a.len() {
        let n = &a[i];
        if !n.num || n.comma {
            continue;
        }
        if q[..n.start].ends_with(['$', '€', '£', '¥']) || q[n.end..].starts_with('%') {
            continue;
        }
        let fam = &a[i - 1];
        let g = gap(q, &a, i);
        if fam.num || !soft(g) {
            continue;
        }
        let glued = g.is_empty();
        // Glued suffix, or a unit (glued or not) that makes this a quantity.
        let mut suffix = String::new();
        let mut next = i + 1;
        if let Some(s) = a.get(i + 1).filter(|s| !s.num && soft(gap(q, &a, i + 1))) {
            let glued_s = gap(q, &a, i + 1).is_empty();
            // Glued letters after a glued name are its suffix (`MI500X`, `H100s`); so is `X` anywhere.
            let unit_like = UNITS.contains(&s.t.as_str())
                && !(glued_s && (glued || s.t == "x"))
                && !a.get(i + 2).is_some_and(|x| x.num && gap(q, &a, i + 2).is_empty());
            if unit_like {
                continue;
            }
            if glued_s {
                // A plural `s` (`H100s`) is not part of the name.
                if s.t != "s" {
                    suffix = s.t.clone();
                }
                next = i + 2;
            }
        }
        if glued {
            if GLUED_STOP.contains(&fam.t.as_str()) {
                continue;
            }
        } else if fam.t.chars().count() < 2
            || WORD_STOP.contains(&fam.t.as_str())
            || UNITS.contains(&fam.t.as_str())
            || is_year(&n.t)
        {
            continue;
        }
        let mut qualifiers = Vec::new();
        let mut end = a[next - 1].end;
        let mut j = next;
        while j < a.len() && qualifiers.len() < 2 {
            let w = &a[j];
            let ok = !w.num
                && soft(gap(q, &a, j))
                && !gap(q, &a, j).is_empty()
                && QUALIFIERS.contains(&w.t.as_str())
                && (w.upper || (!any_upper && LOWER_QUALIFIERS.contains(&w.t.as_str())));
            if !ok {
                break;
            }
            qualifiers.push(w.t.clone());
            end = w.end;
            j += 1;
        }
        // `Rubin Ultra 2`, `RTX PRO 6000`: the family is the word before the qualifier.
        let mut k = i - 1;
        if !glued
            && QUALIFIERS.contains(&fam.t.as_str())
            && k >= 1
            && !a[k - 1].num
            && soft(gap(q, &a, k))
            && !WORD_STOP.contains(&a[k - 1].t.as_str())
        {
            qualifiers.insert(0, fam.t.clone());
            k -= 1;
        }
        let family = &a[k];
        let mut lead = Vec::new();
        let mut w = k;
        while w >= 1 && lead.len() < 2 && !a[w - 1].num && soft(gap(q, &a, w)) && !WORD_STOP.contains(&a[w - 1].t.as_str()) {
            lead.push((a[w - 1].t.clone(), a[w - 1].start));
            w -= 1;
        }
        out.push(Asked {
            family: family.t.clone(),
            glued,
            num: n.t.clone(),
            suffix,
            qualifiers,
            context: lead.first().map(|c| c.0.clone()),
            name_like: family.upper || !any_upper,
            seq: a[k..j].iter().map(|x| x.t.clone()).collect(),
            span: (family.start, end),
            lead,
        });
    }
    out
}

/// A versioned mention inside a vault name.
#[derive(Debug, Clone)]
struct VMention {
    family: String,
    num: String,
    /// Glued suffix, or the next word (`950 DT`).
    suffix: String,
    next_word: String,
}

/// Everything one document is called (title, aliases, entity names), as atoms.
#[derive(Debug, Clone)]
pub(crate) struct VaultDoc {
    pub title: String,
    pub path: String,
    pub layer: String,
    words: HashSet<String>,
    mentions: Vec<VMention>,
}

impl VaultDoc {
    pub fn new(title: &str, path: &str, layer: &str, names: &[String]) -> Self {
        let mut words = HashSet::new();
        let mut mentions = Vec::new();
        for name in names.iter().map(String::as_str).chain(std::iter::once(title)) {
            let a = atomize(name);
            for (i, x) in a.iter().enumerate() {
                if !x.num {
                    words.insert(x.t.clone());
                    continue;
                }
                if x.comma || i == 0 || a[i - 1].num || !soft(gap(name, &a, i)) {
                    continue;
                }
                let (mut suffix, mut next_word) = (String::new(), String::new());
                if let Some(s) = a.get(i + 1).filter(|s| !s.num && soft(gap(name, &a, i + 1))) {
                    if gap(name, &a, i + 1).is_empty() {
                        suffix = s.t.clone();
                    } else {
                        next_word = s.t.clone();
                    }
                }
                let m = VMention { family: a[i - 1].t.clone(), num: x.t.clone(), suffix, next_word };
                // `Ryzen AI Max+ 395`, `RTX PRO 6000`: also file it under the word before the qualifier.
                if QUALIFIERS.contains(&m.family.as_str()) && i >= 2 && !a[i - 2].num {
                    mentions.push(VMention { family: a[i - 2].t.clone(), ..m.clone() });
                }
                mentions.push(m);
            }
        }
        Self { title: title.to_string(), path: path.to_string(), layer: layer.to_string(), words, mentions }
    }
}

/// `3` matches `3` and `3.3` (asking about the family level), not the reverse.
fn num_match(asked: &str, have: &str) -> bool {
    asked == have || (have.starts_with(asked) && have[asked.len()..].starts_with('.'))
}

fn suffix_match(asked: &Asked, m: &VMention) -> bool {
    asked.suffix.is_empty() || asked.suffix == m.suffix || asked.suffix == m.next_word
}

/// The vault has a page for exactly this name. With `ctx_required`, the page must also carry the
/// vendor/model word (`DeepSeek V6` is not `Google TPU v6e`).
fn exists(asked: &Asked, d: &VaultDoc, ctx_required: bool) -> bool {
    if !asked.qualifiers.iter().all(|q| d.words.contains(q)) {
        return false;
    }
    if ctx_required && !asked.context.as_ref().is_some_and(|c| d.words.contains(c)) {
        return false;
    }
    let mention = d.mentions.iter().any(|m| m.family == asked.family && num_match(&asked.num, &m.num) && suffix_match(asked, m));
    // Word families may sit elsewhere in the name (`RTX 6000 Pro` = `RTX PRO 6000 Blackwell`).
    let bag = !asked.glued
        && asked.family.chars().count() >= 3
        && d.words.contains(&asked.family)
        && d.mentions.iter().any(|m| num_match(&asked.num, &m.num) && suffix_match(asked, m));
    mention || bag
}

/// Sibling closeness: (layer rank, number distance ×1000, suffix differs, title length); lower is closer.
type SibKey = (u8, u64, u8, usize);

/// Sort key of a sibling: lower is closer. None = not a sibling.
fn sibling_key(asked: &Asked, d: &VaultDoc, ctx_required: bool) -> Option<SibKey> {
    if ctx_required && !asked.context.as_ref().is_some_and(|c| d.words.contains(c)) {
        return None;
    }
    let layer_rank = match d.layer.as_str() {
        "reference" => 0,
        "wiki" => 1,
        _ => 2,
    };
    let asked_n: f64 = asked.num.parse().unwrap_or(0.0);
    let best = d
        .mentions
        .iter()
        .filter(|m| m.family == asked.family)
        .map(|m| {
            let diff = (m.num.parse::<f64>().unwrap_or(f64::MAX) - asked_n).abs();
            let suffix_off = u8::from(!suffix_match(asked, m));
            ((diff * 1000.0).min(u64::MAX as f64) as u64, suffix_off)
        })
        .min();
    if let Some((diff, suffix_off)) = best {
        return Some((layer_rank, diff, suffix_off, d.title.chars().count()));
    }
    // An unversioned page of the same family (`Tesla Dojo 3` vs "The Tesla Dojo Chip ..."): only for a
    // capitalised word family; a qualifier-shifted one (`Rubin Ultra 2`) needs no more than its word.
    let unversioned = !asked.glued && asked.name_like && asked.family.chars().count() >= 3 && d.words.contains(&asked.family);
    unversioned.then_some((layer_rank + 3, u64::MAX, 0, d.title.chars().count()))
}

/// `seq` occurs as consecutive atoms (soft separators only) in `text`.
fn mentions_seq(text: &str, seq: &[String]) -> bool {
    if seq.is_empty() {
        return false;
    }
    let a = atomize(text);
    a.windows(seq.len()).enumerate().any(|(i, w)| {
        w.iter().zip(seq).all(|(x, s)| x.t == *s) && (1..seq.len()).all(|k| soft(gap(text, &a, i + k)))
    })
}

/// The first versioned name in `question` that no vault document is called, while a sibling is, and
/// that none of `hit_texts` mentions either (a passage naming it may well answer the question).
pub(crate) fn find_mismatch(question: &str, docs: &[VaultDoc], hit_texts: &[&str]) -> Option<NameMismatch> {
    let q = truncate_chars(question, MAX_TEXT_CHARS);
    for asked in asked_names(q) {
        // Short glued prefixes (`v`, `r`, `h`) are only a family next to their vendor/model word, when the
        // vault knows that word (`DeepSeek V6` → DeepSeek V*, not TPU v6e / v7).
        let ctx_required = asked.glued
            && asked.family.chars().count() <= 2
            && asked.context.as_ref().is_some_and(|c| docs.iter().any(|d| d.words.contains(c)));
        if docs.iter().any(|d| exists(&asked, d, ctx_required)) {
            continue;
        }
        let mut sibs: Vec<(SibKey, &VaultDoc)> =
            docs.iter().filter_map(|d| sibling_key(&asked, d, ctx_required).map(|k| (k, d))).collect();
        if sibs.is_empty() {
            continue;
        }
        if hit_texts.iter().any(|t| mentions_seq(t, &asked.seq)) {
            continue;
        }
        sibs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.path.cmp(&b.1.path)));
        let mut seen = HashSet::new();
        let closest: Vec<NamedDoc> = sibs
            .iter()
            .filter(|(_, d)| seen.insert(d.path.clone()))
            .take(MAX_CLOSEST)
            .map(|(_, d)| NamedDoc { title: d.title.clone(), path: d.path.clone() })
            .collect();
        let start = asked.lead.iter().take_while(|(w, _)| sibs[0].1.words.contains(w)).last().map_or(asked.span.0, |l| l.1);
        return Some(NameMismatch { asked: q[start..asked.span.1].to_string(), closest });
    }
    None
}

/// Lowercase strings the vault-name query filters on (families and context words).
fn needles(asked: &[Asked]) -> Vec<String> {
    let mut v: Vec<String> = asked.iter().flat_map(|a| std::iter::once(a.family.clone()).chain(a.context.clone())).collect();
    v.sort();
    v.dedup();
    v
}

/// Documents (any layer but templates) whose title, aliases or entity names contain one of the
/// question's family / context words.
pub(crate) async fn vault_docs<'e, E>(exec: E, question: &str) -> Result<Vec<VaultDoc>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let asked = asked_names(truncate_chars(question, MAX_TEXT_CHARS));
    if asked.is_empty() {
        return Ok(vec![]);
    }
    let rows = sqlx::query(
        r"with n as (
          select d.path, d.title, d.layer,
            d.aliases || coalesce((select array_agg(x) from kb.entities e, unnest(e.aliases || e.name) x
                                   where e.doc_id = d.id), '{}') as names
          from kb.documents d where d.layer <> 'template'
        )
        select path, title, layer, names from n
        where exists (select 1 from unnest($1::text[]) f
                      where strpos(lower(title || ' ' || array_to_string(names, ' ')), f) > 0)",
    )
    .bind(needles(&asked))
    .fetch_all(exec)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let names: Vec<String> = r.get("names");
            VaultDoc::new(r.get("title"), r.get("path"), r.get("layer"), &names)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(q: &str) -> Vec<String> {
        asked_names(q)
            .into_iter()
            .map(|a| {
                let mut s = format!("{}|{}", a.family, a.num);
                if !a.suffix.is_empty() {
                    s.push_str(&format!("|{}", a.suffix));
                }
                for x in &a.qualifiers {
                    s.push_str(&format!("+{x}"));
                }
                s
            })
            .collect()
    }

    #[test]
    fn extracts_versioned_names() {
        assert_eq!(names("Intel Gaudi 5 memory bandwidth"), ["gaudi|5"]);
        assert_eq!(names("Tesla Dojo 3 price"), ["dojo|3"]);
        assert_eq!(names("H300 vs B400"), ["h|300", "b|400"]);
        assert_eq!(names("DeepSeek V5 release"), ["v|5"]);
        assert_eq!(names("Ascend 960 specs"), ["ascend|960"]);
        assert_eq!(names("Ascend 950DT memory bandwidth"), ["ascend|950|dt"]);
        assert_eq!(names("AMD MI500X power"), ["mi|500|x"]);
        assert_eq!(names("Qwen 4.5 context"), ["qwen|4.5"]);
        assert_eq!(names("Qwen3.8 Flash Next price"), ["qwen|3.8+flash+next"]);
        assert_eq!(names("GB10 Ultra bandwidth"), ["gb|10+ultra"]);
        assert_eq!(names("gb10 ultra bandwidth"), ["gb|10+ultra"]);
        assert_eq!(names("Mac Studio M3 Ultra"), ["m|3+ultra"]);
        assert_eq!(names("Rubin Ultra 2 launch"), ["rubin|2+ultra"]);
        assert_eq!(asked_names("Rubin Ultra 2 launch")[0].qualifiers, ["ultra"]);
        assert_eq!(names("Cerebras WSE-3 SRAM"), ["wse|3"]);
        assert_eq!(names("Trainium2 memory bandwidth"), ["trainium|2"]);
        assert_eq!(names("Ryzen AI Max+ 395 power draw"), ["ai|395+max"]);
        assert_eq!(names("TPU v8 vs TPU v7"), ["v|8", "v|7"]);
        assert_eq!(names("Kimi K3 and DeepSeek R2"), ["k|3", "r|2"]);
        assert_eq!(names("RTX 5090 or RTX PRO 6000"), ["rtx|5090", "rtx|6000+pro"]);
        assert_eq!(names("DeepSeek V4.1 Flash 1017 price"), ["v|4.1+flash", "flash|1017"]);
    }

    #[test]
    fn ignores_quantities_and_ordinary_numbers() {
        for q in [
            "GB10 bandwidth is 273 GB/s",
            "in 2026 the price rose 50%",
            "costs $4,699 new",
            "rental at $1.98 per hour",
            "top 10 GPUs of 2026",
            "What did Sam eat on November 3?",
            "Provider price per million tokens for 1017 output",
            "draws 1400 W",
            "1400W TDP",
            "128K context and 5M tokens",
            "3Q26 profit",
            "FP8 vs FP4 and TP4 on HBM4 over PCIe5",
            "a 4x DGX Spark cluster",
            "step 3 of the guide",
            "the 3rd chip",
            "used 8 GPUs",
            "1.6T parameters",
        ] {
            let n = names(q);
            assert!(n.iter().all(|x| x.starts_with("gb|10")), "{q:?} → {n:?}");
        }
        assert_eq!(names("GB10 bandwidth is 273 GB/s"), ["gb|10"]);
        assert!(names("MI355X max power").iter().all(|n| !n.contains('+')), "lowercase max is not a qualifier");
        assert_eq!(names("used H100s"), ["h|100"]);
        assert_eq!(names("AMD MI 600X"), ["mi|600|x"]);
    }

    fn doc(title: &str, path: &str, aliases: &[&str]) -> VaultDoc {
        let layer = super::super::layer_for(path);
        VaultDoc::new(title, path, layer, &aliases.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn vault() -> Vec<VaultDoc> {
        vec![
            doc("Intel Gaudi 3", "References/Intel Gaudi 3.md", &["Gaudi 3"]),
            doc("NVIDIA DGX Spark", "References/NVIDIA DGX Spark.md", &["DGX Spark", "GB10", "NVIDIA GB10", "Spark"]),
            doc("AMD Instinct MI355X", "References/AMD Instinct MI355X.md", &["MI355X"]),
            doc("AMD Instinct MI455X Helios", "References/AMD Instinct MI455X Helios.md", &["MI455X"]),
            doc("AMD Instinct MI300X", "References/AMD Instinct MI300X.md", &["MI300X"]),
            doc("NVIDIA H100 SXM", "References/NVIDIA H100 SXM.md", &["H100 SXM", "H100"]),
            doc("NVIDIA H200 SXM", "References/NVIDIA H200 SXM.md", &["H200"]),
            doc("NVIDIA B200", "References/NVIDIA B200.md", &["B200"]),
            doc("NVIDIA B300", "References/NVIDIA B300.md", &["B300", "Blackwell Ultra"]),
            doc("Intel Arc Pro B60", "References/Intel Arc Pro B60.md", &[]),
            doc("Huawei Ascend 950DT", "References/Huawei Ascend 950DT.md", &["Ascend 950DT"]),
            doc("Huawei Ascend 960", "References/Huawei Ascend 960.md", &[]),
            doc("Huawei Ascend 970", "References/Huawei Ascend 970.md", &[]),
            doc("DeepSeek V4.1 Flash", "References/DeepSeek V4.1 Flash.md", &["DeepSeek-V4.1-Flash", "DeepSeek V4.1 Flash 1017", "1017"]),
            doc("DeepSeek V4 Pro", "References/DeepSeek V4 Pro.md", &[]),
            doc("DeepSeek R1", "References/DeepSeek R1.md", &[]),
            doc("Google TPU v7", "References/Google TPU v7.md", &[]),
            doc("Qwen3.8 27B", "References/Qwen3.8 27B.md", &[]),
            doc("Qwen3.5 397B A17B", "References/Qwen3.5 397B A17B.md", &[]),
            doc("NVIDIA RTX PRO 6000 Blackwell", "References/NVIDIA RTX PRO 6000 Blackwell.md", &[]),
            doc("NVIDIA GeForce RTX 5090", "References/NVIDIA GeForce RTX 5090.md", &["RTX 5090"]),
            doc("AMD Ryzen AI Max+ 395", "References/AMD Ryzen AI Max+ 395.md", &[]),
            doc("Apple Mac Studio M3 Ultra", "References/Apple Mac Studio M3 Ultra.md", &[]),
            doc("Llama 3.3 70B Instruct", "References/Llama 3.3 70B Instruct.md", &[]),
            doc("NVIDIA Vera Rubin NVL72", "References/NVIDIA Vera Rubin NVL72.md", &[]),
            doc(
                "The Tesla Dojo Chip Is Impressive, But There Are Some Major Technical Issues",
                "Clippings/The Tesla Dojo Chip Is Impressive.md",
                &[],
            ),
            doc("GLM-5.3-Flash-FP4-AWQ (Community)", "References/GLM-5.3-Flash-FP4-AWQ (Community).md", &[]),
        ]
    }

    fn check(q: &str) -> Option<NameMismatch> {
        find_mismatch(q, &vault(), &[])
    }

    #[test]
    fn near_misses_warn() {
        let m = check("Intel Gaudi 5 memory bandwidth").unwrap();
        assert_eq!(m.asked, "Intel Gaudi 5");
        assert_eq!(m.closest, [NamedDoc { title: "Intel Gaudi 3".into(), path: "References/Intel Gaudi 3.md".into() }]);
        assert_eq!(
            m.note(),
            "The vault has no page for Intel Gaudi 5; closest: Intel Gaudi 3 (References/Intel Gaudi 3.md). \
             These results are about a different model, not Intel Gaudi 5."
        );
        let m = check("gaudi 5 bandwidth").unwrap();
        assert_eq!(m.asked, "gaudi 5");
        let m = check("Tesla Dojo 3 price").unwrap();
        assert_eq!(m.asked, "Tesla Dojo 3");
        assert!(m.closest[0].path.starts_with("Clippings/The Tesla Dojo"));
        assert_eq!(check("H300 price").unwrap().closest[0].title, "NVIDIA H200 SXM");
        assert_eq!(check("NVIDIA B400 rental").unwrap().asked, "NVIDIA B400");
        assert_eq!(check("B400 rental").unwrap().closest[0].title, "NVIDIA B300");
        assert_eq!(check("AMD MI600X power").unwrap().closest[0].title, "AMD Instinct MI455X Helios");
        assert_eq!(check("Ascend 990 bandwidth").unwrap().closest[0].title, "Huawei Ascend 970");
        assert_eq!(check("Ascend 950DX bandwidth").unwrap().closest[0].title, "Huawei Ascend 950DT");
        let m = check("DeepSeek V6 context length").unwrap();
        assert_eq!(m.asked, "DeepSeek V6");
        assert!(m.closest.iter().all(|d| d.title.starts_with("DeepSeek V")), "{m:?}");
        assert_eq!(check("DeepSeek R2 benchmarks").unwrap().closest[0].title, "DeepSeek R1");
        assert_eq!(check("DeepSeek V4.1 Pro price").unwrap().asked, "DeepSeek V4.1 Pro");
        assert_eq!(check("GB10 Ultra bandwidth").unwrap().closest[0].title, "NVIDIA DGX Spark");
        assert_eq!(check("Qwen 4.5 context").unwrap().closest[0].title, "Qwen3.8 27B");
        assert_eq!(check("TPU v8 memory").unwrap().closest[0].title, "Google TPU v7");
        assert_eq!(check("Google TPU v8 memory").unwrap().asked, "Google TPU v8");
        // `v6e` exists, but under Google TPU, not DeepSeek.
        let mut v = vault();
        v.push(doc("Google TPU v6e", "References/Google TPU v6e.md", &[]));
        assert_eq!(find_mismatch("DeepSeek V6 context length", &v, &[]).unwrap().asked, "DeepSeek V6");
        assert_eq!(find_mismatch("TPU v6 memory", &v, &[]), None);
        assert_eq!(check("Rubin Ultra 2 launch").unwrap().closest[0].title, "NVIDIA Vera Rubin NVL72");
        assert_eq!(check("Is the H100 faster than an H300?").unwrap().asked, "H300");
    }

    #[test]
    fn existing_names_do_not_warn() {
        for q in [
            "GB10 memory bandwidth",
            "What is the GB10 bandwidth? 273 GB/s?",
            "nvidia gb10 price",
            "Ascend 950DT memory bandwidth",
            "Ascend 960 specs",
            "How much power does the MI355X draw?",
            "MI 355X power",
            "mi355x power",
            "AMD Instinct MI-355X",
            "MI355 power",
            "MI355X max power",
            "renting H100s by the hour",
            "an 8xH100 node",
            "H100 vs B200",
            "cheapest B300 rental per hour at $1.98",
            "DeepSeek V4.1 Flash price",
            "DeepInfra promo input price for DeepSeek V4.1 Flash 1017",
            "DeepSeek V4 context",
            "Intel Arc Pro B60 memory bandwidth",
            "Mac Studio M3 Ultra memory bandwidth",
            "Ryzen AI Max+ 395 power draw",
            "RTX 6000 Pro price",
            "RTX PRO 6000 price",
            "Llama 3 license",
            "Google TPU v7 bandwidth",
            "FP8 support",
            "FP16 throughput on GLM",
            "DGX Spark price in 2026",
            "top 10 chips",
            "What is the boiling point of mercury?",
            "Intel Gaudi 3 memory bandwidth",
            "Gaudi3 memory bandwidth",
        ] {
            assert_eq!(check(q), None, "{q}");
        }
    }

    #[test]
    fn unknown_families_and_hit_mentions_do_not_warn() {
        // No sibling in the vault: nothing to confuse it with.
        assert_eq!(check("Snapdragon 8 Gen 4 NPU"), None);
        assert_eq!(check("iPhone 17 battery"), None);
        // Short glued prefix with a known vendor word but no sibling under it.
        assert_eq!(check("Tesla D1 SerDes"), None);
        // A returned passage that names the asked model may answer it.
        assert_eq!(find_mismatch("B400 price", &vault(), &["NVIDIA's B400 is expected in 2027."]), None);
        assert_eq!(find_mismatch("B400 price", &vault(), &["NVIDIA B300 pricing"]).unwrap().asked, "B400");
        assert!(mentions_seq("the MI 600X ships", &["mi".into(), "600".into(), "x".into()]));
        assert!(!mentions_seq("the MI6000X ships", &["mi".into(), "600".into(), "x".into()]));
    }

    #[test]
    fn atoms_and_numbers() {
        let a: Vec<String> = atomize("MI355X, V4.1 $4,699 950DT.").into_iter().map(|a| a.t).collect();
        assert_eq!(a, ["mi", "355", "x", "v", "4.1", "4,699", "950", "dt"]);
        assert!(num_match("3", "3.3") && num_match("4.1", "4.1") && !num_match("4.1", "4") && !num_match("3", "30"));
    }
}

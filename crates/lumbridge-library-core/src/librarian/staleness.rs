//! Staleness: Wiki/Reference pages whose prices are all older than `stale_days`, lines that say
//! "as of <old date>", "scheduled/expected <date>" lines whose date has passed, explicit
//! "re-check" notes, and lint's wiki-older-than-its-sources. Reported only.

use super::facts::dates_in;
use super::{Ctx, record_finding};
use anyhow::Result;
use chrono::{Duration, NaiveDate};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::LazyLock;

const EXAMPLES_PER_PAGE: usize = 5;
/// A "scheduled/expected <date>" line is stale when that date passed within this window.
const SCHEDULED_WINDOW_DAYS: i64 = 90;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StalePage {
    pub path: String,
    pub reasons: Vec<String>,
    /// Newest date on any price line (None = prices are undated or there are none).
    pub newest_price_date: Option<NaiveDate>,
    pub price_lines: usize,
    /// A few offending lines (line number: text, truncated).
    pub examples: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StalenessReport {
    pub pages_checked: usize,
    pub cutoff: Option<NaiveDate>,
    pub pages: Vec<StalePage>,
    pub new_findings: usize,
}

static PRICE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\s?\d").unwrap());
static RECHECK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bre-?check\b|\bverify before\b|\bto be confirmed\b|\bnot yet confirmed\b|\bTBC\b").unwrap());
static SCHEDULED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(scheduled|expected|upcoming|planned|slated|due)\b").unwrap());
static AS_OF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bas of\b").unwrap());
static MONTH_YEAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(jan|feb|mar|apr|may|jun|jul|aug|sep|sept|oct|nov|dec)[a-z]*\.?\s+(\d{4})\b").unwrap()
});
static QUARTER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bQ([1-4])\s+(\d{4})\b").unwrap());

fn last_day(y: i32, m: u32) -> Option<NaiveDate> {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    NaiveDate::from_ymd_opt(ny, nm, 1).map(|d| d - Duration::days(1))
}

/// Dates on a line, including month-year (`Oct 2026` → end of October) and quarters (`Q3 2026`).
pub fn line_dates(s: &str) -> Vec<NaiveDate> {
    let mut out = dates_in(s);
    for c in MONTH_YEAR.captures_iter(s) {
        let m = match &c[1].to_lowercase()[..3] {
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
        };
        if let Some(d) = c[2].parse().ok().and_then(|y| last_day(y, m)) {
            out.push(d);
        }
    }
    for c in QUARTER.captures_iter(s) {
        let q: u32 = c[1].parse().unwrap_or(4);
        if let Some(d) = c[2].parse().ok().and_then(|y| last_day(y, q * 3)) {
            out.push(d);
        }
    }
    out
}

/// Check one page body. `today` and `cutoff` are passed in so tests are deterministic.
pub fn check_page(path: &str, body: &str, today: NaiveDate, cutoff: NaiveDate) -> Option<StalePage> {
    let mut reasons = Vec::new();
    let mut examples = Vec::new();
    let (mut price_lines, mut newest): (usize, Option<NaiveDate>) = (0, None);
    let (mut old_as_of, mut passed, mut recheck) = (0usize, 0usize, 0usize);
    let push_example = |n: usize, l: &str, ex: &mut Vec<String>| {
        if ex.len() < EXAMPLES_PER_PAGE {
            ex.push(format!("{}: {}", n + 1, crate::docs::truncate_chars(l.trim(), 160)));
        }
    };
    for (n, line) in body.lines().enumerate() {
        let ds = line_dates(line);
        if PRICE.is_match(line) {
            price_lines += 1;
            if let Some(d) = ds.iter().max() {
                newest = newest.max(Some(*d));
            }
            if AS_OF.is_match(line) && ds.iter().max().is_some_and(|d| *d < cutoff) {
                old_as_of += 1;
                push_example(n, line, &mut examples);
            }
        }
        // only recently passed dates: older "expected ..." lines are history, not stale
        if SCHEDULED.is_match(line) && ds.iter().any(|d| *d < today && *d >= today - Duration::days(SCHEDULED_WINDOW_DAYS)) && !ds.iter().any(|d| *d >= today) {
            passed += 1;
            push_example(n, line, &mut examples);
        }
        if RECHECK.is_match(line) {
            recheck += 1;
            push_example(n, line, &mut examples);
        }
    }
    if let Some(d) = newest
        && d < cutoff
    {
        reasons.push(format!("newest dated price is from {d} (older than {cutoff})"));
    }
    if old_as_of > 0 {
        reasons.push(format!("{old_as_of} price lines say \"as of\" a date before {cutoff}"));
    }
    if passed > 0 {
        reasons.push(format!("{passed} scheduled/expected dates have passed"));
    }
    if recheck > 0 {
        reasons.push(format!("{recheck} re-check notes"));
    }
    (!reasons.is_empty()).then(|| StalePage { path: path.to_string(), reasons, newest_price_date: newest, price_lines, examples })
}

pub async fn run(ctx: &Ctx<'_>, scope: Option<&[String]>) -> Result<StalenessReport> {
    let today = chrono::Local::now().date_naive();
    let cutoff = today - Duration::days(ctx.cfg.stale_days.max(1));
    let rows = sqlx::query_as::<_, (String, String)>(
        "select d.path, d.body from kb.documents d where d.layer in ('wiki', 'reference') and SCOPE order by d.layer desc, d.path"
            .replace("SCOPE", super::SCOPE_SQL)
            .as_str(),
    )
    .bind(scope)
    .fetch_all(ctx.pool)
    .await?;
    let mut rep = StalenessReport { pages_checked: rows.len(), cutoff: Some(cutoff), ..Default::default() };
    for (path, body) in &rows {
        if let Some(p) = check_page(path, body, today, cutoff) {
            rep.pages.push(p);
        }
    }
    // Lint's view: wiki pages dated before a source they link to.
    if let Ok(l) = crate::docs::lint(ctx.pool, ctx.vault_root).await {
        for s in l.stale_wiki.into_iter().filter(|s| super::in_scope(scope, &s.path)) {
            let why = format!("updated {} but links {} from {}", s.wiki_date, s.source_path, s.source_date);
            match rep.pages.iter_mut().find(|p| p.path == s.path) {
                Some(p) => p.reasons.push(why),
                None => rep.pages.push(StalePage { path: s.path, reasons: vec![why], newest_price_date: None, price_lines: 0, examples: vec![] }),
            }
        }
    }
    for p in &rep.pages {
        let summary = format!("stale page {}: {}", p.path, p.reasons.join("; "));
        if record_finding(ctx, "librarian.stale", &p.path, &summary, json!(p)).await {
            rep.new_findings += 1;
        }
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_signals() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 8).unwrap();
        let cutoff = today - Duration::days(45);
        let body = "Intro\n- B200 $6.10/hr (as of 2026-08-01)\n- H100 $2.00/hr in Jul 2026\n\
                    Global launch scheduled 2026-11-01.\nPrices are dated. Re-check before acting.\n";
        let p = check_page("Wiki/X.md", body, today, cutoff).unwrap();
        assert_eq!(p.price_lines, 2);
        assert_eq!(p.newest_price_date, NaiveDate::from_ymd_opt(2026, 8, 1));
        assert_eq!(p.reasons.len(), 4, "{:?}", p.reasons);
        let fresh = "- B200 $6.10/hr (as of 2026-11-07)\nLaunch scheduled 2026-11-30.\n";
        assert!(check_page("Wiki/Y.md", fresh, today, cutoff).is_none());
        assert_eq!(line_dates("Q3 2026 and late Oct 2025"), vec![NaiveDate::from_ymd_opt(2025, 10, 31).unwrap(), NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()]);
    }
}

//! Orphan clippings → links. A clipping no Wiki/Reference page links to is invisible when browsing,
//! so the librarian finds the best existing page(s) (hybrid search + resolve on the title), asks
//! the local LLM whether the clipping is really *about* that page's subject (confident yes only),
//! and appends `- [[Clipping]]` under the page's Related / Clippings / Sources section.
//!
//! Only agent-authored pages are touched (and `write_doc` re-checks), never Clippings themselves,
//! at most `max_edits_per_run` edits. Only the link (the clipping's filename) is written, never
//! clipping text, so paid posts are not quoted into pages.

use super::{Ctx, record, wikilink_for};
use crate::docs::{self, SearchParams, WriteDoc};
use crate::llm::Llm;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use std::collections::HashSet;

/// Candidate pages shown to the LLM per clipping.
const CANDIDATES: usize = 6;
/// LLM confidence needed for a link.
const MIN_CONFIDENCE: f64 = 0.8;
/// Pages linked per clipping.
const MAX_PER_CLIPPING: usize = 2;
/// Characters of clipping text the (local) LLM sees.
const SUMMARY_CHARS: usize = 900;
/// Sections a link goes under, in order of preference; else a new `## Sources` is appended.
const SECTIONS: [&str; 3] = ["related", "clippings", "sources"];

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProposedLink {
    pub clipping: String,
    pub page: String,
    /// The line added (`- [[…]]`).
    pub line: String,
    pub section: String,
    pub confidence: f64,
    pub reason: String,
    /// Commit of the edit (None in dry run or without git).
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LinksReport {
    /// Clippings no Wiki/Reference page links to (fully isolated ones, lint's orphans, go first).
    pub orphans: usize,
    /// Orphans judged this run (bounded by the LLM budget and the edit cap).
    pub considered: usize,
    pub proposed: Vec<ProposedLink>,
    /// Pages actually edited (0 in dry run).
    pub edited: usize,
    /// Low-confidence or "no" answers, for tuning.
    pub declined: Vec<String>,
    /// Candidate pages skipped because they are not agent-authored.
    pub not_agent_owned: usize,
    pub errors: Vec<String>,
}

#[derive(Deserialize)]
struct Judgement {
    links: Vec<JudgedLink>,
}
#[derive(Deserialize)]
struct JudgedLink {
    candidate: i64,
    confidence: f64,
    reason: String,
}

fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {"links": {"type": "array", "maxItems": MAX_PER_CLIPPING, "items": {
            "type": "object",
            "properties": {
                "candidate": {"type": "integer"},
                "confidence": {"type": "number"},
                "reason": {"type": "string"}
            },
            "required": ["candidate", "confidence", "reason"],
            "additionalProperties": false
        }}},
        "required": ["links"],
        "additionalProperties": false
    })
}

const SYSTEM: &str = "You are the librarian of a private research wiki about AI hardware, models, inference and markets. \
A clipping (saved article) is not linked from any wiki or reference page. Decide which candidate pages should list it as a source. \
Say yes only when the clipping is substantially about the candidate page's subject (that chip, model, company, engine or topic) \
so a reader of that page would want it; a passing mention is not enough. Prefer the most specific page. \
Return at most 2 links, each with the candidate number, a confidence between 0 and 1, and a one-sentence reason. \
Return an empty list if no candidate fits.";

/// Plain text start of a clipping for the prompt: markdown links and images flattened.
fn summary(body: &str) -> String {
    let (clean, _) = super::facts::clean_markdown(&body.replace("![", "["));
    crate::docs::truncate_chars(&clean, SUMMARY_CHARS).to_string()
}

/// Insert `line` at the end of the first matching `##` section (else append `## Sources`).
/// Returns the new body and the section used.
pub fn insert_link(body: &str, line: &str) -> (String, String) {
    let lines: Vec<&str> = body.lines().collect();
    let heads: Vec<(usize, String)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| l.strip_prefix("## ").map(|h| (i, h.trim().to_string())))
        .collect();
    for want in SECTIONS {
        let Some(pos) = heads.iter().position(|(_, h)| h.to_lowercase() == want) else { continue };
        let start = heads[pos].0;
        let end = heads.get(pos + 1).map(|(i, _)| *i).unwrap_or(lines.len());
        // after the last non-empty line of the section
        let mut at = start + 1;
        for (i, l) in lines.iter().enumerate().take(end).skip(start + 1) {
            if !l.trim().is_empty() {
                at = i + 1;
            }
        }
        let mut out: Vec<&str> = lines[..at].to_vec();
        if at == start + 1 {
            out.push("");
        }
        out.push(line);
        if at < lines.len() && !lines[at].trim().is_empty() {
            out.push("");
        }
        out.extend_from_slice(&lines[at..]);
        return (out.join("\n") + "\n", heads[pos].1.clone());
    }
    (format!("{}\n\n## Sources\n\n{line}\n", body.trim_end()), "Sources".into())
}

pub async fn run(ctx: &Ctx<'_>, llm: Option<&Llm>, dry: bool, scope: Option<&[String]>) -> Result<LinksReport> {
    let mut rep = LinksReport::default();
    let orphans = sqlx::query(
        "select d.path, d.title, left(d.body, 4000) as body from kb.documents d
         where d.layer = 'clipping' and SCOPE
           and not exists (select 1 from kb.links l join kb.documents s on s.id = l.src_doc
                           where l.dst_doc = d.id and s.layer in ('wiki', 'reference'))
         order by exists (select 1 from kb.links l where l.src_doc = d.id), d.created desc nulls last, d.path"
            .replace("SCOPE", super::SCOPE_SQL)
            .as_str(),
    )
    .bind(scope)
    .fetch_all(ctx.pool)
    .await?;
    rep.orphans = orphans.len();
    let Some(llm) = llm else {
        rep.errors.push("no LLM: orphan linking needs a confident model judgement; skipped".into());
        return Ok(rep);
    };
    let cap = ctx.cfg.max_edits_per_run;
    let mut owned_cache: std::collections::HashMap<String, bool> = std::collections::HashMap::new();

    for o in orphans {
        if rep.proposed.len() >= cap || llm.remaining() == 0 {
            break;
        }
        let path: String = o.get("path");
        let title: String = o.get("title");
        let body: String = o.get("body");

        // Candidates: hybrid search on the title over Wiki + References, then resolve.
        let mut cand: Vec<(String, String)> = Vec::new();
        let mut seen = HashSet::new();
        let mut sp = SearchParams::new(title.clone());
        sp.layers = Some(vec!["reference".into(), "wiki".into()]);
        sp.limit = 12;
        match docs::search(ctx.pool, ctx.embedder, ctx.reranker, ctx.embed_model_id, &sp).await {
            Ok(r) => {
                for h in r.hits {
                    if seen.insert(h.path.clone()) {
                        cand.push((h.path, h.title));
                    }
                }
            }
            Err(e) => rep.errors.push(format!("search for {path}: {e:#}")),
        }
        if let Ok(rs) = docs::resolve(ctx.pool, &title, 5).await {
            for r in rs {
                if let (Some(p), Some("reference" | "wiki")) = (r.path.clone(), r.layer.as_deref())
                    && seen.insert(p.clone())
                {
                    cand.push((p, r.title));
                }
            }
        }
        // Only agent-authored pages; none that already link to this clipping.
        let link = wikilink_for(&path);
        let mut usable = Vec::new();
        for (p, t) in cand {
            if usable.len() >= CANDIDATES {
                break;
            }
            let full = ctx.vault_root.join(&p);
            let Ok(text) = tokio::fs::read_to_string(&full).await else { continue };
            let owned = *owned_cache.entry(p.clone()).or_insert_with(|| docs::agent_owned(&text));
            if !owned {
                rep.not_agent_owned += 1;
                continue;
            }
            if text.contains(&link) || text.contains(&format!("[[{}|", crate::docs::stem(&path))) {
                continue;
            }
            usable.push((p, t));
        }
        if usable.is_empty() {
            continue;
        }
        rep.considered += 1;
        let list = usable.iter().enumerate().map(|(i, (p, t))| format!("{}. {t} ({p})", i + 1)).collect::<Vec<_>>().join("\n");
        let user = format!("Clipping title: {title}\nClipping start:\n{}\n\nCandidate pages:\n{list}", summary(&body));
        let j: Judgement = match llm.json(SYSTEM, &user, "link_judgement", schema()).await {
            Ok(j) => j,
            Err(e) => {
                rep.errors.push(format!("LLM on {path}: {e:#}"));
                continue;
            }
        };
        let mut taken = 0;
        for jl in j.links {
            let reason = crate::docs::truncate_chars(jl.reason.trim(), 240).to_string();
            let idx = jl.candidate - 1;
            if idx < 0 || idx as usize >= usable.len() {
                rep.declined.push(format!("{path}: candidate {} out of range", jl.candidate));
                continue;
            }
            let (page, _) = &usable[idx as usize];
            if jl.confidence < MIN_CONFIDENCE || taken >= MAX_PER_CLIPPING {
                rep.declined.push(format!("{path} → {page}: confidence {:.2} ({reason})", jl.confidence));
                continue;
            }
            if rep.proposed.len() >= cap {
                break;
            }
            taken += 1;
            let line = format!("- {link}");
            let full = ctx.vault_root.join(page);
            let text = match tokio::fs::read_to_string(&full).await {
                Ok(t) => t,
                Err(e) => {
                    rep.errors.push(format!("reading {page}: {e}"));
                    continue;
                }
            };
            let (fm_yaml, page_body, _) = crate::docs::frontmatter::split(&text);
            let (fm, _, _) = docs::parse_document(&text);
            let (new_body, section) = insert_link(page_body, &line);
            let mut pl = ProposedLink { clipping: path.clone(), page: page.clone(), line: line.clone(), section, confidence: jl.confidence, reason, commit: None };
            if !dry {
                if fm_yaml.is_none() {
                    rep.errors.push(format!("{page}: no frontmatter; not edited"));
                    continue;
                }
                let w = WriteDoc {
                    path: page.clone(),
                    title: fm.title.clone(),
                    author: crate::identity::get().librarian.clone(),
                    frontmatter: None,
                    body: new_body,
                    message: Some(format!("librarian: link {link} from {page}")),
                };
                match docs::write_doc(ctx.pool, ctx.vault_root, w).await {
                    Ok(out) => {
                        rep.edited += 1;
                        pl.commit = out.commit.clone();
                        let summary = format!("linked orphan clipping {link} from {page} (§ {})", pl.section);
                        record(
                            ctx,
                            "librarian.link",
                            "docs",
                            page,
                            true,
                            &summary,
                            json!({"clipping": path, "page": page, "section": pl.section, "confidence": pl.confidence,
                                   "reason": pl.reason, "commit": out.commit}),
                        )
                        .await;
                    }
                    Err(e) => {
                        // write_doc's default-deny (or a race) wins; never retried around it.
                        let msg = format!("could not link {link} from {page}: {e:#}");
                        record(ctx, "librarian.link", "docs", page, false, &msg, json!({"clipping": path, "error": format!("{e:#}")})).await;
                        rep.errors.push(msg);
                        continue;
                    }
                }
            }
            rep.proposed.push(pl);
        }
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::insert_link;

    #[test]
    fn inserts_under_related_before_next_section() {
        let body = "Intro\n\n## Related\n\n- [[A]]\n- [[B]]\n\n## Sources\n\n- <https://x>\n";
        let (out, sec) = insert_link(body, "- [[C]]");
        assert_eq!(sec, "Related");
        assert_eq!(out, "Intro\n\n## Related\n\n- [[A]]\n- [[B]]\n- [[C]]\n\n## Sources\n\n- <https://x>\n");
    }

    #[test]
    fn falls_back_to_sources_then_new_section() {
        let (out, sec) = insert_link("Intro\n\n## Sources\n\n- <https://x>", "- [[C]]");
        assert_eq!(sec, "Sources");
        assert!(out.ends_with("- <https://x>\n- [[C]]\n"));
        let (out, sec) = insert_link("Intro\n\n## Software\n\n- vLLM\n", "- [[C]]");
        assert_eq!(sec, "Sources");
        assert_eq!(out, "Intro\n\n## Software\n\n- vLLM\n\n## Sources\n\n- [[C]]\n");
        let (out, _) = insert_link("## Related\n## Next\n", "- [[C]]");
        assert_eq!(out, "## Related\n\n- [[C]]\n\n## Next\n");
    }
}

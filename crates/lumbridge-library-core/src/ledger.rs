//! The training ledger: every library answer (`lib.qa`) with the owner's verdicts, retrieval evals
//! (`lib.evals` / `lib.eval_runs`) and hashed dataset exports (`lib.datasets`).
//!
//! Exports land in `{library_root}/datasets/<name>.jsonl` and are registered with their sha256 and
//! row count, so any LoRA / distillation run on an LFM or Qwen model can be reproduced exactly.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::io::Write;
use std::path::Path;
use uuid::Uuid;

#[derive(Debug, Clone, Default)]
pub struct QaLog {
    pub shelf: &'static str,
    pub asked_by: String,
    pub question: String,
    pub resolved_id: Option<String>,
    pub cited: Value,
    pub answer: Option<String>,
    pub model: Option<String>,
    pub trace_id: Option<String>,
}

/// Record an answer the library gave. Never fails the caller: logging problems return None.
pub async fn log_qa(pool: &PgPool, q: QaLog) -> Option<Uuid> {
    sqlx::query_scalar(
        "insert into lib.qa (shelf, asked_by, question, resolved_id, cited, answer, model, trace_id)
         values ($1, $2, $3, $4, $5, $6, $7, $8) returning id",
    )
    .bind(q.shelf)
    .bind(&q.asked_by)
    .bind(&q.question)
    .bind(&q.resolved_id)
    .bind(&q.cited)
    .bind(&q.answer)
    .bind(&q.model)
    .bind(&q.trace_id)
    .fetch_one(pool)
    .await
    .map_err(|e| tracing::warn!("qa log failed: {e}"))
    .ok()
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VerdictOutcome {
    pub qa_id: Uuid,
    pub verdict: i16,
    pub recorded: bool,
}

/// The owner's (or an agent relaying the owner's) judgement on an answer: 1 good, 0 meh, -1 wrong.
/// An optional final answer text may also be attached (`answer`), e.g. what the agent actually said.
pub async fn verdict(
    pool: &PgPool,
    qa_id: Uuid,
    verdict: i16,
    correction: Option<&str>,
    answer: Option<&str>,
    by: &str,
) -> Result<VerdictOutcome> {
    if !(-1..=1).contains(&verdict) {
        bail!("verdict must be -1, 0 or 1");
    }
    let n = sqlx::query(
        "update lib.qa set verdict = $2, correction = coalesce($3, correction),
                answer = coalesce($4, answer), verdict_at = now(), verdict_by = $5 where id = $1",
    )
    .bind(qa_id)
    .bind(verdict)
    .bind(correction)
    .bind(answer)
    .bind(by)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(VerdictOutcome { qa_id, verdict, recorded: n == 1 })
}

/// Attach the final answer to a logged question (agents call this after answering).
pub async fn set_answer(pool: &PgPool, qa_id: Uuid, answer: &str, model: Option<&str>) -> Result<bool> {
    let n = sqlx::query("update lib.qa set answer = $2, model = coalesce($3, model) where id = $1")
        .bind(qa_id)
        .bind(answer)
        .bind(model)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n == 1)
}

// ───────────────────────────── evals ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EvalItem {
    pub id: Uuid,
    pub set_name: String,
    pub question: String,
    pub expected: String,
    pub grader: String,
    pub tags: Vec<String>,
}

pub async fn add_eval(pool: &PgPool, set_name: &str, question: &str, expected: &str, grader: &str, tags: &[String]) -> Result<Uuid> {
    if !["exact", "contains", "judge", "retrieval", "decline"].contains(&grader) {
        bail!("grader must be exact | contains | judge | retrieval | decline");
    }
    Ok(sqlx::query_scalar(
        "insert into lib.evals (set_name, question, expected, grader, tags) values ($1, $2, $3, $4, $5) returning id",
    )
    .bind(set_name)
    .bind(question)
    .bind(expected)
    .bind(grader)
    .bind(tags)
    .fetch_one(pool)
    .await?)
}

pub async fn evals(pool: &PgPool, set_name: Option<&str>) -> Result<Vec<EvalItem>> {
    let rows = sqlx::query(
        "select id, set_name, question, expected, grader, tags from lib.evals
         where active and ($1::text is null or set_name = $1) order by set_name, created_at",
    )
    .bind(set_name)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| EvalItem {
            id: r.get("id"),
            set_name: r.get("set_name"),
            question: r.get("question"),
            expected: r.get("expected"),
            grader: r.get("grader"),
            tags: r.get("tags"),
        })
        .collect())
}

pub async fn record_eval_run(pool: &PgPool, eval_id: Uuid, model: &str, answer: &str, score: f32, detail: Value) -> Result<()> {
    sqlx::query("insert into lib.eval_runs (eval_id, model, answer, score, detail) values ($1, $2, $3, $4, $5)")
        .bind(eval_id)
        .bind(model)
        .bind(answer)
        .bind(score)
        .bind(detail)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EvalRunReport {
    /// `retrieval@<embed fingerprint>`.
    pub model: String,
    pub graded: usize,
    /// Mean score over graded items (0 when none): the answer appears anywhere in the packed context.
    pub score: f32,
    /// Rank-aware: share of items whose answer is in citation block [1] / within [1]..[3], and the mean
    /// reciprocal rank (0 when absent). These move when ranking gets better or worse even if `score` doesn't.
    #[serde(default)]
    pub hit_at_1: f32,
    #[serde(default)]
    pub hit_at_3: f32,
    #[serde(default)]
    pub mrr: f32,
    /// Per eval set: {set: {graded, score, hit_at_1, hit_at_3, mrr}}.
    #[serde(default)]
    pub sets: Value,
    pub items: Vec<Value>,
}

/// Split a packed context into its citation blocks (`[1] …`, `[2] …`). Text before `[1]` (warnings,
/// facts headers) is not a block.
pub fn citation_blocks(ctx: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    for line in ctx.lines() {
        let t = line.trim_start();
        let starts = t.starts_with('[')
            && t[1..].find(']').is_some_and(|end| end > 0 && t[1..1 + end].chars().all(|c| c.is_ascii_digit()));
        if starts {
            blocks.push(String::new());
        }
        if let Some(b) = blocks.last_mut() {
            b.push_str(line);
            b.push('\n');
        }
    }
    blocks
}

/// 1-based rank of the first citation block containing any `|`-alternative of `expected`.
pub fn answer_rank(expected: &str, ctx: &str) -> Option<usize> {
    let alts: Vec<String> = expected.split('|').map(|a| a.trim().to_lowercase()).filter(|a| !a.is_empty()).collect();
    citation_blocks(ctx).iter().position(|b| {
        let b = b.to_lowercase();
        alts.iter().any(|a| b.contains(a))
    }).map(|i| i + 1)
}

/// Retrieval evals: does the packed context for the question contain the expected text? Items tagged
/// `code` run against the warehouse, `design` against the design shelf (expected = design slug), the rest against docs. Each graded item is recorded in
/// `lib.eval_runs` under `retrieval@<fingerprint>` so a model or ranking change shows up as a number.
pub async fn run_retrieval_evals(
    pool: &PgPool,
    embedder: &crate::models::Embedder,
    reranker: &crate::models::Reranker,
    embed_model_id: i32,
    fingerprint: &str,
    set: Option<&str>,
) -> Result<EvalRunReport> {
    let items = evals(pool, set).await?;
    let model = format!("retrieval@{fingerprint}");
    #[derive(Default)]
    struct Acc {
        n: usize,
        score: f32,
        h1: f32,
        h3: f32,
        rr: f32,
    }
    let mut total = Acc::default();
    let mut per: std::collections::BTreeMap<String, Acc> = Default::default();
    let mut rows = Vec::new();
    for it in items {
        // `decline`: the right behaviour is "the library doesn't have this" (docs no_good_match).
        if it.grader == "decline" {
            let r = crate::docs::query(pool, embedder, reranker, embed_model_id, &it.question, None, Some(4000)).await;
            let (score, detail) = match r {
                // Declining counts either way: nothing cleared the floor, or a clear look-alike warning
                // ("the vault has no page for Gaudi 5; closest: Gaudi 3").
                Ok(a) => {
                    let ok = a.no_good_match || a.name_mismatch.is_some();
                    (if ok { 1.0 } else { 0.0 }, json!({"no_good_match": a.no_good_match, "name_mismatch": a.name_mismatch.is_some()}))
                }
                Err(e) => {
                    rows.push(json!({"id": it.id, "error": format!("{e:#}")}));
                    continue;
                }
            };
            let _ = record_eval_run(pool, it.id, &model, "", score, detail).await;
            for acc in [&mut total, per.entry(it.set_name.clone()).or_default()] {
                acc.n += 1;
                acc.score += score;
                acc.h1 += score;
                acc.h3 += score;
                acc.rr += score;
            }
            rows.push(json!({"id": it.id, "set": it.set_name, "question": it.question, "score": score, "rank": null}));
            continue;
        }
        let ctx = if it.tags.iter().any(|t| t == "design") {
            // Design search: one citation block per hit, so the expected slug's rank is graded.
            let p = crate::warehouse::design::DesignSearchParams { query: it.question.clone(), include_drafts: true, limit: 6, ..Default::default() };
            crate::warehouse::design::search(pool, embedder, reranker, Some(embed_model_id), &p).await.map(|r| {
                r.hits
                    .iter()
                    .enumerate()
                    .map(|(i, h)| format!("[{}] {} ({})\n{}\n", i + 1, h.slug, h.name, h.summary))
                    .collect::<String>()
            })
        } else if it.tags.iter().any(|t| t == "code") {
            crate::warehouse::query(pool, embedder, reranker, embed_model_id, &it.question, None, Some(6000))
                .await
                .map(|a| a.context_md)
        } else {
            crate::docs::query(pool, embedder, reranker, embed_model_id, &it.question, None, Some(4000))
                .await
                .map(|a| a.context_md)
        };
        let ctx = match ctx {
            Ok(c) => c,
            Err(e) => {
                rows.push(json!({"id": it.id, "error": format!("{e:#}")}));
                continue;
            }
        };
        let Some(score) = grade(&it.grader, &it.expected, &ctx) else { continue };
        let rank = if score > 0.0 { answer_rank(&it.expected, &ctx) } else { None };
        let (h1, h3, rr) = match rank {
            Some(r) => ((r == 1) as u8 as f32, (r <= 3) as u8 as f32, 1.0 / r as f32),
            // In the context but not inside a citation block (e.g. a facts header): count as found at 1.
            None if score > 0.0 => (1.0, 1.0, 1.0),
            None => (0.0, 0.0, 0.0),
        };
        for acc in [&mut total, per.entry(it.set_name.clone()).or_default()] {
            acc.n += 1;
            acc.score += score;
            acc.h1 += h1;
            acc.h3 += h3;
            acc.rr += rr;
        }
        let _ = record_eval_run(
            pool,
            it.id,
            &model,
            "",
            score,
            json!({"context_tokens": crate::rank::approx_tokens(&ctx), "rank": rank}),
        )
        .await;
        rows.push(json!({"id": it.id, "set": it.set_name, "question": it.question, "score": score, "rank": rank}));
    }
    let avg = |x: f32, n: usize| if n > 0 { x / n as f32 } else { 0.0 };
    let sets: serde_json::Map<String, Value> = per
        .iter()
        .map(|(k, a)| {
            (k.clone(), json!({"graded": a.n, "score": avg(a.score, a.n), "hit_at_1": avg(a.h1, a.n), "hit_at_3": avg(a.h3, a.n), "mrr": avg(a.rr, a.n)}))
        })
        .collect();
    Ok(EvalRunReport {
        model,
        graded: total.n,
        score: avg(total.score, total.n),
        hit_at_1: avg(total.h1, total.n),
        hit_at_3: avg(total.h3, total.n),
        mrr: avg(total.rr, total.n),
        sets: Value::Object(sets),
        items: rows,
    })
}

/// Grade without an LLM. `retrieval`/`contains`: every `|`-separated alternative group must appear
/// (case-insensitive) — `expected = "273 GB/s|273GB/s"` passes if either form is present.
pub fn grade(grader: &str, expected: &str, got: &str) -> Option<f32> {
    let g = got.to_lowercase();
    match grader {
        "exact" => Some(if got.trim().eq_ignore_ascii_case(expected.trim()) { 1.0 } else { 0.0 }),
        "contains" | "retrieval" => {
            Some(if expected.split('|').any(|alt| g.contains(&alt.trim().to_lowercase())) { 1.0 } else { 0.0 })
        }
        _ => None, // judge: needs a model; graded elsewhere
    }
}

// ───────────────────────────── datasets ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ExportKind {
    /// Approved answers (verdict 1, or any row with a correction) as chat SFT rows.
    QaSft,
    /// Rejected answers with a correction as DPO pairs (chosen = correction).
    QaDpo,
    /// Vault documents as a continued-pretraining corpus (paid-subscription content excluded
    /// unless `include_paid`).
    DocsCorpus,
    /// Warehouse code chunks: own + components + permissive mirrors (copyleft/unknown excluded).
    CodeCorpus,
}

impl ExportKind {
    fn as_str(self) -> &'static str {
        match self {
            ExportKind::QaSft => "qa-sft",
            ExportKind::QaDpo => "qa-dpo",
            ExportKind::DocsCorpus => "docs-corpus",
            ExportKind::CodeCorpus => "code-corpus",
        }
    }
    fn dataset_kind(self) -> &'static str {
        match self {
            ExportKind::QaSft => "sft",
            ExportKind::QaDpo => "dpo",
            ExportKind::DocsCorpus | ExportKind::CodeCorpus => "distill",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DatasetRow {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub sha256: String,
    pub rows: i32,
    pub path: String,
}

const SYSTEM: &str = "You are a research assistant. Answer from the library and cite vault paths or repo:path:lines.";

/// Write a JSONL export under `{library_root}/datasets/` and register it in `lib.datasets`.
/// Streams rows from Postgres; never holds the whole corpus in memory.
pub async fn export(pool: &PgPool, library_root: &Path, kind: ExportKind, include_paid: bool) -> Result<DatasetRow> {
    use futures_util::TryStreamExt;
    let dir = library_root.join("datasets");
    std::fs::create_dir_all(&dir)?;
    let name = format!("{}-{}", kind.as_str(), chrono::Utc::now().format("%Y%m%d-%H%M%S"));
    let path = dir.join(format!("{name}.jsonl"));
    let tmp = dir.join(format!(".{name}.jsonl.tmp"));
    let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?);
    let mut rows = 0i32;
    let filters;
    {
        let mut emit = |v: Value| -> Result<()> {
            let line = serde_json::to_string(&v)?;
            out.write_all(line.as_bytes())?;
            out.write_all(b"\n")?;
            rows += 1;
            Ok(())
        };
        match kind {
            ExportKind::QaSft => {
                filters = json!({"verdict": 1, "or": "correction is not null"});
                let mut s = sqlx::query(
                    "select id, shelf, question, coalesce(correction, answer) as answer, cited, resolved_id, verdict_by
                     from lib.qa where (verdict = 1 and answer is not null) or correction is not null order by at",
                )
                .fetch(pool);
                while let Some(r) = s.try_next().await? {
                    emit(json!({
                        "id": r.get::<Uuid, _>("id"),
                        "shelf": r.get::<String, _>("shelf"),
                        "messages": [
                            {"role": "system", "content": SYSTEM},
                            {"role": "user", "content": r.get::<String, _>("question")},
                            {"role": "assistant", "content": r.get::<String, _>("answer")},
                        ],
                        "cited": r.get::<Value, _>("cited"),
                        "resolved_id": r.get::<Option<String>, _>("resolved_id"),
                        "verdict_by": r.get::<Option<String>, _>("verdict_by"),
                    }))?;
                }
            }
            ExportKind::QaDpo => {
                filters = json!({"verdict": -1, "correction": "not null", "answer": "not null"});
                let mut s = sqlx::query(
                    "select id, question, answer, correction from lib.qa
                     where verdict = -1 and correction is not null and answer is not null order by at",
                )
                .fetch(pool);
                while let Some(r) = s.try_next().await? {
                    emit(json!({
                        "id": r.get::<Uuid, _>("id"),
                        "prompt": [{"role": "system", "content": SYSTEM}, {"role": "user", "content": r.get::<String, _>("question")}],
                        "chosen": r.get::<String, _>("correction"),
                        "rejected": r.get::<String, _>("answer"),
                    }))?;
                }
            }
            ExportKind::DocsCorpus => {
                filters = json!({"layer": "not template", "include_paid": include_paid});
                let mut s = sqlx::query(
                    "select path, layer, author, status, title, categories, body, frontmatter->>'license' as license,
                            private
                     from kb.documents
                     where layer <> 'template'
                       and ($1 or coalesce(frontmatter->>'license', '') <> 'paid-subscription')
                     order by path",
                )
                .bind(include_paid)
                .fetch(pool);
                while let Some(r) = s.try_next().await? {
                    emit(json!({
                        "path": r.get::<String, _>("path"),
                        "layer": r.get::<String, _>("layer"),
                        "author": r.get::<String, _>("author"),
                        "status": r.get::<String, _>("status"),
                        "title": r.get::<String, _>("title"),
                        "categories": r.get::<Vec<String>, _>("categories"),
                        "license": r.get::<Option<String>, _>("license"),
                        "private": r.get::<bool, _>("private"),
                        "text": r.get::<String, _>("body"),
                    }))?;
                }
            }
            ExportKind::CodeCorpus => {
                filters = json!({"tiers": ["own", "component", "mirror(permissive)"], "copyleft": false, "license": "not null"});
                let mut s = sqlx::query(
                    "select r.slug, r.tier, r.license_spdx, c.path, c.line_start, c.line_end, c.language, c.content
                     from wh.chunks c join wh.repos r on r.id = c.repo_id
                     where r.tier in ('own', 'component')
                        or (r.tier = 'mirror' and r.license_spdx is not null and coalesce(r.copyleft, true) = false)
                     order by r.slug, c.path, c.line_start",
                )
                .fetch(pool);
                while let Some(r) = s.try_next().await? {
                    emit(json!({
                        "repo": r.get::<String, _>("slug"),
                        "tier": r.get::<String, _>("tier"),
                        "license": r.get::<Option<String>, _>("license_spdx"),
                        "path": r.get::<String, _>("path"),
                        "lines": [r.get::<i32, _>("line_start"), r.get::<i32, _>("line_end")],
                        "language": r.get::<Option<String>, _>("language"),
                        "text": r.get::<String, _>("content"),
                    }))?;
                }
            }
        }
    }
    out.flush()?;
    drop(out);
    let sha = sha256_file(&tmp)?;
    std::fs::rename(&tmp, &path)?;
    let path_s = path.display().to_string();
    let id: Uuid = sqlx::query_scalar(
        "insert into lib.datasets (name, kind, sha256, rows, path, filters) values ($1, $2, $3, $4, $5, $6) returning id",
    )
    .bind(&name)
    .bind(kind.dataset_kind())
    .bind(&sha)
    .bind(rows)
    .bind(&path_s)
    .bind(&filters)
    .fetch_one(pool)
    .await?;
    Ok(DatasetRow { id, name, kind: kind.dataset_kind().into(), sha256: sha, rows, path: path_s })
}

fn sha256_file(p: &Path) -> Result<String> {
    let out = std::process::Command::new("sha256sum").arg(p).output().context("running sha256sum")?;
    if !out.status.success() {
        bail!("sha256sum failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or_default().to_string())
}

#[cfg(test)]
mod tests {
    use super::grade;

    #[test]
    fn ranks() {
        let ctx = "_note_\n\n[1] A — x (a.md)\nnothing\n\n[2] B — y (b.md)\nGB10 has 273 GB/s\n[3] C\n273GB/s";
        assert_eq!(super::citation_blocks(ctx).len(), 3);
        assert_eq!(super::answer_rank("273 GB/s|273GB/s", ctx), Some(2));
        assert_eq!(super::answer_rank("missing", ctx), None);
        assert_eq!(super::answer_rank("_note_", ctx), None);
    }

    #[test]
    fn grading() {
        assert_eq!(grade("contains", "273 GB/s|273GB/s", "bandwidth is 273GB/s"), Some(1.0));
        assert_eq!(grade("retrieval", "GB10", "nothing here"), Some(0.0));
        assert_eq!(grade("exact", "Yes", " yes "), Some(1.0));
        assert_eq!(grade("judge", "x", "y"), None);
    }
}

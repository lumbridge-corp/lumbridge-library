//! The librarian: an upkeep agent that keeps Lumbridge Docs organised. Every task is idempotent,
//! conservative and capped, returns a report, and leaves a trail in the activity feed
//! (`lib.ops_log`, actor `librarian`):
//!
//! - [`facts`]: References' structured bullets ("Key specs", "Prices", "Key facts") → `kb.facts`
//!   (deterministic parser first, the local LLM only for bullets it can't split, grounded verbatim).
//!   Contradictions with a fact from another source are reported, never auto-resolved.
//! - [`links`]: clippings no Wiki/Reference page links to → `- [[Clipping]]` under the best
//!   agent-authored page's Related/Clippings/Sources section, when the LLM is confident.
//! - [`duplicates`]: near-duplicate clippings (same source URL, title trigram, first-chunk cosine).
//! - [`staleness`]: pages with old prices, passed "scheduled" dates or "re-check" notes.
//! - [`evals`]: the retrieval evals, compared with the previous librarian run.
//!
//! `dry_run` (the default) writes nothing to the vault or `kb.facts`; findings and the run summary
//! still go to the feed. The LLM is meant to be a local model (`[librarian] base_url`), so paid clippings
//! need not leave the machine, and pages only ever get links and titles, never quoted clipping text.

pub mod duplicates;
pub mod evals;
pub mod facts;
pub mod links;
pub mod staleness;

use crate::config::LibrarianConfig;
use crate::db;
use crate::llm::Llm;
use crate::models::{Embedder, Reranker};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::path::Path;

pub const ACTOR: &str = "librarian";
/// A finding (duplicate pair, stale page, contradiction) is recorded again only after this long.
const FINDING_REPEAT_DAYS: i32 = 7;
/// List items kept per section in the stored run summary.
const KEEP_IN_SUMMARY: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Task {
    Facts,
    Links,
    Duplicates,
    Staleness,
    Evals,
}

impl Task {
    pub const ALL: [Task; 5] = [Task::Facts, Task::Links, Task::Duplicates, Task::Staleness, Task::Evals];
}

/// What a run needs. lumbridge-libraryd builds it from `AppState`; tests and the dry-run example build it by hand.
pub struct Ctx<'a> {
    pub pool: &'a PgPool,
    pub vault_root: &'a Path,
    pub embedder: &'a Embedder,
    pub reranker: &'a Reranker,
    pub embed_model_id: i32,
    pub fingerprint: String,
    /// None = no LLM (deterministic parts only; orphan linking is skipped).
    pub llm: Option<Llm>,
    pub cfg: LibrarianConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunOptions {
    pub dry_run: bool,
    /// Empty = all tasks.
    #[serde(default)]
    pub tasks: Vec<Task>,
    /// Who asked: `schedule`, `<owner>@cli`, ...
    pub requested_by: String,
    /// Limit facts / links / duplicates / staleness to vault paths starting with one of these
    /// (e.g. `References/NVIDIA B200.md`, `Clippings/`). Empty = the whole vault.
    #[serde(default)]
    pub path_prefixes: Vec<String>,
}

/// SQL filter over `d.path` bound as `$1` (a text[] of prefixes, or null for everything).
pub(crate) const SCOPE_SQL: &str = "($1::text[] is null or exists (select 1 from unnest($1::text[]) p where starts_with(d.path, p)))";

pub(crate) fn in_scope(scope: Option<&[String]>, path: &str) -> bool {
    scope.is_none_or(|s| s.iter().any(|p| path.starts_with(p.as_str())))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LlmInfo {
    pub model: Option<String>,
    pub available: bool,
    pub calls_used: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunReport {
    #[schemars(with = "String")]
    pub started_at: DateTime<Utc>,
    #[schemars(with = "String")]
    pub finished_at: DateTime<Utc>,
    pub dry_run: bool,
    pub requested_by: String,
    pub tasks: Vec<Task>,
    pub llm: LlmInfo,
    pub facts: Option<facts::FactsReport>,
    pub links: Option<links::LinksReport>,
    pub duplicates: Option<duplicates::DuplicatesReport>,
    pub staleness: Option<staleness::StalenessReport>,
    pub evals: Option<evals::EvalsReport>,
    /// Task-level failures (a task that errored doesn't stop the others).
    pub errors: Vec<String>,
    /// The one-line summary also written to the feed.
    pub summary: String,
    /// What happened to `log.md`: the commit, or why it was left alone.
    pub log_md: String,
}

/// One activity row from the librarian.
pub(crate) async fn record(ctx: &Ctx<'_>, op: &str, shelf: &str, target: &str, ok: bool, summary: &str, detail: Value) {
    db::record(ctx.pool, op, shelf, target, ok, Some(ACTOR), Some(summary), detail).await;
}

/// Was this finding recorded recently? (keeps the nightly feed from repeating itself)
pub(crate) async fn recently_recorded(ctx: &Ctx<'_>, op: &str, target: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "select exists (select 1 from lib.ops_log where op = $1 and target = $2 and actor = $3
                        and at > now() - make_interval(days => $4))",
    )
    .bind(op)
    .bind(target)
    .bind(ACTOR)
    .bind(FINDING_REPEAT_DAYS)
    .fetch_one(ctx.pool)
    .await
    .unwrap_or(false)
}

/// Record a finding unless the same one was recorded in the last week. Returns whether it was new.
pub(crate) async fn record_finding(ctx: &Ctx<'_>, op: &str, target: &str, summary: &str, detail: Value) -> bool {
    if recently_recorded(ctx, op, target).await {
        return false;
    }
    record(ctx, op, "docs", target, true, summary, detail).await;
    true
}

/// Run the selected tasks (all by default), record the summary row and the `log.md` line.
pub async fn run(ctx: &Ctx<'_>, opts: RunOptions) -> RunReport {
    let started_at = Utc::now();
    let tasks: Vec<Task> = if opts.tasks.is_empty() {
        Task::ALL.to_vec()
    } else {
        Task::ALL.into_iter().filter(|t| opts.tasks.contains(t)).collect()
    };
    let dry = opts.dry_run;
    let scope_v: Vec<String> = opts.path_prefixes.iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
    let scope = (!scope_v.is_empty()).then_some(scope_v.as_slice());
    let mut errors = Vec::new();

    let mut llm_info = LlmInfo::default();
    let budget_start = ctx.llm.as_ref().map(|l| l.remaining()).unwrap_or(0);
    let llm = match &ctx.llm {
        Some(l) => {
            llm_info.model = Some(l.model().to_string());
            llm_info.available = l.healthy().await;
            if !llm_info.available {
                errors.push(format!("LLM {} not reachable at {}; LLM steps skipped", l.model(), ctx.cfg.base_url));
            }
            llm_info.available.then(|| l.clone())
        }
        None => None,
    };

    let mut rep_facts = None;
    let mut rep_links = None;
    let mut rep_dups = None;
    let mut rep_stale = None;
    let mut rep_evals = None;
    for t in &tasks {
        let r: anyhow::Result<()> = async {
            match t {
                Task::Facts => rep_facts = Some(facts::run(ctx, llm.as_ref(), dry, scope).await?),
                Task::Links => rep_links = Some(links::run(ctx, llm.as_ref(), dry, scope).await?),
                Task::Duplicates => rep_dups = Some(duplicates::run(ctx, scope).await?),
                Task::Staleness => rep_stale = Some(staleness::run(ctx, scope).await?),
                Task::Evals => rep_evals = Some(evals::run(ctx).await?),
            }
            Ok(())
        }
        .await;
        if let Err(e) = r {
            let msg = format!("{t:?}: {e:#}");
            tracing::warn!("librarian {msg}");
            errors.push(msg);
        }
    }
    llm_info.calls_used = budget_start.saturating_sub(ctx.llm.as_ref().map(|l| l.remaining()).unwrap_or(0));

    // One line for humans.
    let mut parts = Vec::new();
    if let Some(f) = &rep_facts {
        let verb = if dry { "would record" } else { "recorded" };
        parts.push(format!("{verb} {} facts ({} unchanged)", f.added + f.updated, f.unchanged));
        if !f.contradictions.is_empty() {
            parts.push(format!("{} contradictions", f.contradictions.len()));
        }
    }
    if let Some(l) = &rep_links {
        let verb = if dry { "would link" } else { "linked" };
        parts.push(format!("{verb} {} of {} orphan clippings", l.proposed.len(), l.orphans));
    }
    if let Some(d) = &rep_dups {
        parts.push(format!("{} duplicate pairs", d.pairs.len()));
    }
    if let Some(s) = &rep_stale {
        parts.push(format!("{} stale pages", s.pages.len()));
    }
    if let Some(e) = &rep_evals {
        parts.push(match e.previous {
            Some(p) => format!("evals {:.3} (prev {:.3})", e.score, p),
            None => format!("evals {:.3}", e.score),
        });
    }
    if !errors.is_empty() {
        parts.push(format!("{} errors", errors.len()));
    }
    let summary = format!("librarian run{}: {}", if dry { " (dry run)" } else { "" }, if parts.is_empty() { "nothing to do".into() } else { parts.join(", ") });

    // log.md: only when a page was edited (the per-page writes already logged themselves).
    let edited = rep_links.as_ref().map(|l| l.edited).unwrap_or(0);
    let log_md = if dry {
        "dry run: log.md untouched".to_string()
    } else if edited == 0 {
        "no page edited: log.md untouched".to_string()
    } else {
        let line = format!("## [{}] lint | {}", chrono::Local::now().date_naive(), summary.replacen("librarian run: ", "librarian run — ", 1));
        match crate::docs::append_log_entry(ctx.pool, ctx.vault_root, &line).await {
            Ok(Some(c)) => format!("appended to log.md ({c})"),
            Ok(None) => "appended to log.md (vault is not a git checkout)".to_string(),
            Err(e) => {
                errors.push(format!("log.md: {e:#}"));
                format!("log.md append failed: {e:#}")
            }
        }
    };

    let report = RunReport {
        started_at,
        finished_at: Utc::now(),
        dry_run: dry,
        requested_by: opts.requested_by,
        tasks,
        llm: llm_info,
        facts: rep_facts,
        links: rep_links,
        duplicates: rep_dups,
        staleness: rep_stale,
        evals: rep_evals,
        errors,
        summary,
        log_md,
    };
    record(ctx, "librarian.run", "library", "vault", report.errors.is_empty(), &report.summary, capped(&report)).await;
    report
}

/// The report with long lists cut to [`KEEP_IN_SUMMARY`] items (stored in the feed row).
pub fn capped(r: &RunReport) -> Value {
    let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
    fn walk(v: &mut Value) {
        match v {
            Value::Array(a) => {
                let n = a.len();
                if n > KEEP_IN_SUMMARY {
                    a.truncate(KEEP_IN_SUMMARY);
                    a.push(json!(format!("… {} more", n - KEEP_IN_SUMMARY)));
                }
                a.iter_mut().for_each(walk);
            }
            Value::Object(m) => m.values_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut v);
    v
}

/// The last run's stored summary (from the feed, so it survives restarts).
pub async fn last(pool: &PgPool) -> anyhow::Result<Option<db::ActivityRow>> {
    Ok(db::activity(pool, 1, None, Some("librarian.run"), None).await?.into_iter().next())
}

/// `[[stem]]` text and its vault path for a clipping or page path.
pub(crate) fn wikilink_for(path: &str) -> String {
    format!("[[{}]]", crate::docs::stem(path))
}

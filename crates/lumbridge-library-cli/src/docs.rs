//! Lumbridge Docs CLI (`lumbridge-docs`) — the knowledge base (research, papers, books, notes, wiki, dated facts) of a Lumbridge Library.
use anyhow::{Context, Result, bail};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use crate::{Client, hits, out, print};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "lumbridge-docs", version, about = "Lumbridge Docs: the knowledge base of a Lumbridge Library (lumbridge-libraryd)")]
struct Cli {
    #[arg(long, global = true, help = "raw JSON output")]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// What Lumbridge Docs holds and whether the server's models are up.
    Status,
    /// Answer context for a question, with [n] citations (logged; prints qa_id for `verdict`).
    Ask {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        question: Vec<String>,
        /// An id from `resolve` (/docs/<path>, /docs/entity/<slug>, /docs/category/<name>).
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value_t = 4000)]
        max_tokens: usize,
    },
    /// Ranked chunks for a query.
    Search {
        #[arg(allow_hyphen_values = true)]
        query: Vec<String>,
        #[arg(long, short)]
        limit: Option<usize>,
        /// note | thought | daily | clipping | reference | wiki | meta (repeatable)
        #[arg(long)]
        layer: Vec<String>,
        #[arg(long)]
        category: Vec<String>,
        /// Vault path prefix, e.g. Wiki/
        #[arg(long)]
        prefix: Option<String>,
    },
    /// Turn a name into library ids.
    Resolve { name: Vec<String> },
    /// Print a document (optionally lines A:B).
    Read {
        path: String,
        #[arg(long)]
        lines: Option<String>,
    },
    /// Facts about an entity.
    Facts {
        entity: String,
        #[arg(long)]
        attribute: Option<String>,
        /// YYYY-MM-DD
        #[arg(long)]
        as_of: Option<String>,
        #[arg(long)]
        history: bool,
    },
    /// Record a fact: lumbridge-docs fact nvidia-gb10 memory_bandwidth_gbs 273 --unit GB/s --source URL
    Fact {
        entity: String,
        attribute: String,
        value: String,
        #[arg(long)]
        unit: Option<String>,
        #[arg(long)]
        source: Option<String>,
        /// owner | measured | vendor | community (owner = the configured owner's name)
        #[arg(long)]
        verified_by: Option<String>,
        #[arg(long)]
        valid_from: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Save a URL (the server fetches + extracts HTML/PDF; owner only) or a file into Clippings/.
    Clip {
        url: String,
        #[arg(long)]
        title: Option<String>,
        /// Use this markdown file as the body instead of fetching.
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        category: Vec<String>,
        #[arg(long)]
        tag: Vec<String>,
    },
    /// Write a Wiki/ or References/ page from a markdown file.
    Write {
        path: String,
        file: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, default_value = "claude")]
        author: String,
        #[arg(long)]
        category: Vec<String>,
        #[arg(long)]
        message: Option<String>,
    },
    /// Vault health report.
    Lint,
    /// Pull the vault from Gitea and re-index now.
    Sync,
    /// Record a verdict on an answer: lumbridge-docs verdict <qa_id> 1|0|-1 [--correction "..."]
    Verdict {
        qa_id: String,
        #[arg(allow_hyphen_values = true)]
        verdict: i16,
        #[arg(long)]
        correction: Option<String>,
    },
    /// Export a training dataset: qa-sft | qa-dpo | docs-corpus | code-corpus
    Export {
        kind: String,
        #[arg(long)]
        include_paid: bool,
    },
    /// Retrieval evals: `eval add <set> <question> <expected>`, `eval run [set]`, `eval list [set]`.
    Eval {
        #[command(subcommand)]
        cmd: EvalCmd,
    },
    /// Background jobs.
    Jobs,
}

#[derive(Subcommand)]
enum EvalCmd {
    Add {
        set: String,
        question: String,
        /// Expected text; `a|b` accepts either.
        expected: String,
        #[arg(long, default_value = "retrieval")]
        grader: String,
        #[arg(long)]
        tag: Vec<String>,
    },
    Run { set: Option<String> },
    List { set: Option<String> },
}

fn read_file(p: &str) -> Result<String> {
    std::fs::read_to_string(p).with_context(|| format!("reading {p}"))
}

#[tokio::main]
pub async fn main() -> Result<()> {
    let name = "lumbridge-docs";
    let cli = Cli::from_arg_matches(&Cli::command().name(name).bin_name(name).get_matches())?;
    let c = Client::from_env()?;
    let j = cli.json;
    let v: Value = match cli.cmd {
        Cmd::Status => {
            let mut v = c.get("/v1/status").await?;
            if !j && let Some(o) = v.as_object_mut() {
                o.remove("warehouse");
            }
            v
        }
        Cmd::Ask { question, scope, max_tokens } => {
            let v = c
                .post("/v1/docs/query", &json!({"question": question.join(" "), "scope": scope, "max_tokens": max_tokens}))
                .await?;
            if !j {
                let mut s = v["context_md"].as_str().unwrap_or("").to_string();
                if s.is_empty() {
                    s = "(nothing found)\n".into();
                }
                if let Some(d) = v["degraded"].as_array().filter(|d| !d.is_empty()) {
                    s.push_str(&format!("\n(degraded: {d:?})"));
                }
                s.push_str(&format!("\nqa_id: {}\n", v["qa_id"].as_str().unwrap_or("-")));
                out(&s);
                return Ok(());
            }
            v
        }
        Cmd::Search { query, limit, layer, category, prefix } => {
            let mut b = json!({"q": query.join(" ")});
            if let Some(l) = limit {
                b["limit"] = json!(l);
            }
            if !layer.is_empty() {
                b["layers"] = json!(layer);
            }
            if !category.is_empty() {
                b["categories"] = json!(category);
            }
            if let Some(p) = prefix {
                b["path_prefix"] = json!(p);
            }
            let v = c.post("/v1/docs/search", &b).await?;
            if !j {
                hits(&v, |h| {
                    let head = h["heading"].as_str().unwrap_or("");
                    let path = h["path"].as_str().unwrap_or("");
                    if head.is_empty() { path.to_string() } else { format!("{path} — {head}") }
                });
                return Ok(());
            }
            v
        }
        Cmd::Resolve { name } => {
            let v = c.get_q("/v1/docs/resolve", &[("name", Some(name.join(" ")))]).await?;
            if !j {
                let mut s = String::new();
                for r in v.as_array().into_iter().flatten() {
                    s.push_str(&format!(
                        "{:.3}  {:8} {}\n       {}\n",
                        r["score"].as_f64().unwrap_or(0.0),
                        r["kind"].as_str().unwrap_or(""),
                        r["id"].as_str().unwrap_or(""),
                        r["snippet"].as_str().unwrap_or("").chars().take(150).collect::<String>()
                    ));
                }
                out(&s);
                return Ok(());
            }
            v
        }
        Cmd::Read { path, lines } => {
            let (start, end) = match lines.as_deref().map(|l| l.split_once(':').unwrap_or((l, ""))) {
                Some((a, b)) => (Some(a.to_string()), (!b.is_empty()).then(|| b.to_string())),
                None => (None, None),
            };
            let v = c.get_q("/v1/docs/read", &[("path", Some(path)), ("start", start), ("end", end)]).await?;
            if !j {
                out(&format!("# {}  ({})\n\n{}\n", v["title"].as_str().unwrap_or(""), v["path"].as_str().unwrap_or(""), v["body"].as_str().unwrap_or("")));
                return Ok(());
            }
            v
        }
        Cmd::Facts { entity, attribute, as_of, history } => {
            c.get_q(
                "/v1/docs/facts",
                &[("entity", Some(entity)), ("attribute", attribute), ("as_of", as_of), ("history", history.then(|| "true".into()))],
            )
            .await?
        }
        Cmd::Fact { entity, attribute, value, unit, source, verified_by, valid_from, kind, note } => {
            let mut b = json!({"entity": entity, "attribute": attribute, "unit": unit, "source_uri": source,
                               "verified_by": verified_by, "valid_from": valid_from, "entity_kind": kind, "note": note});
            match value.parse::<f64>() {
                Ok(n) => b["value_num"] = json!(n),
                Err(_) => b["value_text"] = json!(value),
            }
            c.post("/v1/docs/fact", &b).await?
        }
        Cmd::Clip { url, title, file, category, tag } => {
            let body = file.as_deref().map(read_file).transpose()?;
            c.post("/v1/docs/clip", &json!({"url": url, "title": title, "body": body, "categories": category, "tags": tag})).await?
        }
        Cmd::Write { path, file, title, author, category, message } => {
            let mut fm = serde_json::Map::new();
            if !category.is_empty() {
                fm.insert("categories".into(), json!(category.iter().map(|c| format!("[[{c}]]")).collect::<Vec<_>>()));
            }
            c.post(
                "/v1/docs/write",
                &json!({"path": path, "title": title, "author": author, "frontmatter": fm, "body": read_file(&file)?, "message": message}),
            )
            .await?
        }
        Cmd::Lint => c.get("/v1/docs/lint").await?,
        Cmd::Sync => c.post("/v1/docs/sync", &json!({})).await?,
        Cmd::Verdict { qa_id, verdict, correction } => {
            if !(-1..=1).contains(&verdict) {
                bail!("verdict is 1, 0 or -1");
            }
            c.post("/v1/verdict", &json!({"qa_id": qa_id, "verdict": verdict, "correction": correction})).await?
        }
        Cmd::Export { kind, include_paid } => c.post("/v1/export", &json!({"kind": kind, "include_paid": include_paid})).await?,
        Cmd::Eval { cmd } => match cmd {
            EvalCmd::Add { set, question, expected, grader, tag } => {
                c.post("/v1/evals", &json!({"set": set, "question": question, "expected": expected, "grader": grader, "tags": tag})).await?
            }
            EvalCmd::Run { set } => c.post("/v1/evals/run", &json!({"set": set})).await?,
            EvalCmd::List { set } => c.get_q("/v1/evals", &[("set", set)]).await?,
        },
        Cmd::Jobs => c.get("/v1/jobs").await?,
    };
    print(&v, j);
    Ok(())
}

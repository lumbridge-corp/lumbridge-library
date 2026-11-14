//! Lumbridge Warehouse CLI (`lumbridge-warehouse`) — mirrored open-source repos, your own code, distilled and clean-room components.
use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use crate::{Client, hits, out, print};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "lumbridge-warehouse", version, about = "Lumbridge Warehouse: the code shelf of a Lumbridge Library (lumbridge-libraryd)")]
struct Cli {
    #[arg(long, global = true, help = "raw JSON output")]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// What the warehouse holds and whether the server's models are up.
    Status,
    /// List repos (optionally one tier: mirror | own | component | design).
    Repos { tier: Option<String> },
    /// Mirror an open-source repo (clone + index in the background).
    Mirror {
        url: String,
        #[arg(long)]
        tag: Vec<String>,
    },
    /// Add your own code (git URL or a path on the server).
    Own {
        url: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        tag: Vec<String>,
    },
    /// Fetch + re-index one repo, or all repos.
    Sync {
        repo: Option<String>,
        #[arg(long)]
        force: bool,
    },
    /// Remove a repo from the index (--delete-files also removes the mirror).
    Remove {
        repo: String,
        #[arg(long)]
        delete_files: bool,
    },
    /// Turn a name into warehouse ids.
    Resolve { name: Vec<String> },
    /// Cited code context for a question (logged; prints qa_id).
    Ask {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        question: Vec<String>,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value_t = 6000)]
        max_tokens: usize,
    },
    /// Ranked code chunks.
    Search {
        query: Vec<String>,
        #[arg(long, short)]
        repo: Vec<String>,
        #[arg(long)]
        lang: Vec<String>,
        #[arg(long)]
        tier: Vec<String>,
        #[arg(long, short)]
        limit: Option<usize>,
    },
    /// Exact / regex search (git grep).
    Grep {
        pattern: String,
        #[arg(long, short)]
        repo: Vec<String>,
        #[arg(long, short = 'E')]
        regex: bool,
        #[arg(long, short)]
        ignore_case: bool,
        #[arg(long, short)]
        glob: Option<String>,
        #[arg(long, short = 'n')]
        max: Option<usize>,
    },
    /// Find definitions: `Name`, `prefix*`, `Type::method`.
    Symbols {
        name: String,
        #[arg(long, short)]
        repo: Vec<String>,
        #[arg(long, short)]
        kind: Option<String>,
        #[arg(long, short)]
        limit: Option<usize>,
    },
    /// Print a file (or --lines A:B) or a symbol's block (--symbol).
    Read {
        repo: String,
        path: Option<String>,
        #[arg(long)]
        lines: Option<String>,
        #[arg(long)]
        symbol: Option<String>,
    },
    /// A repo's card (overview, license, central files, signatures).
    Card { repo: String },
    /// Components: list | show | approve | retire | use | create
    Component {
        #[command(subcommand)]
        cmd: CompCmd,
    },
    /// Design systems: search | list | show | read | add | approve | retire | caption
    Design {
        #[command(subcommand)]
        cmd: DesignCmd,
    },
    /// Background jobs.
    Jobs,
}

#[derive(Subcommand)]
enum DesignCmd {
    /// Find designs by look: `lumbridge-warehouse design search dark console mono --theme dark`.
    Search {
        query: Vec<String>,
        #[arg(long)]
        theme: Option<String>,
        #[arg(long)]
        font: Option<String>,
        #[arg(long)]
        color: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        tag: Option<String>,
        #[arg(long, short)]
        limit: Option<usize>,
    },
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        theme: Option<String>,
        #[arg(long)]
        font: Option<String>,
    },
    /// Overview: facets, files, screenshots.
    Show { slug: String },
    /// Print one file of a design (DESIGN.md, tokens.css, ...).
    Read { slug: String, path: String },
    /// Add / update from a capture folder on the server (warehouse/designs/<capture>), or from a JSON file
    /// matching NewDesign (--file).
    Add {
        slug: String,
        #[arg(long)]
        capture: Option<String>,
        #[arg(long, default_value = "capture")]
        kind: String,
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        tag: Vec<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// Approve a draft design (owner only: needs the admin token).
    Approve { slug: String },
    Retire { slug: String },
    /// Caption uncaptioned screenshots now (vision model).
    Caption,
}

#[derive(Subcommand)]
enum CompCmd {
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        origin: Option<String>,
        #[arg(long)]
        tag: Option<String>,
    },
    /// Approve a draft component (owner only: needs the admin token; not available to agents over MCP).
    Approve { slug: String },
    Retire { slug: String },
    /// Record that a component was copied into a project.
    Use {
        slug: String,
        project: String,
        #[arg(long)]
        commit: Option<String>,
    },
    /// Create from a JSON file matching NewComponent (slug, summary, origin, license_spdx, files, spec_md, ...).
    Create { file: String },
}

#[tokio::main]
pub async fn main() -> Result<()> {
    let name = "lumbridge-warehouse";
    let cli = Cli::from_arg_matches(&Cli::command().name(name).bin_name(name).get_matches())?;
    let c = Client::from_env()?;
    let j = cli.json;
    let v: Value = match cli.cmd {
        Cmd::Status => {
            let mut v = c.get("/v1/status").await?;
            if !j && let Some(o) = v.as_object_mut() {
                o.remove("docs");
            }
            v
        }
        Cmd::Repos { tier } => {
            let v = c.get_q("/v1/warehouse/repos", &[("tier", tier)]).await?;
            if !j {
                let mut s = String::new();
                for r in v.as_array().into_iter().flatten() {
                    s.push_str(&format!(
                        "{:42} {:20} {:>6} files  {}{}\n",
                        r["slug"].as_str().unwrap_or(""),
                        r["license_spdx"].as_str().unwrap_or("license?"),
                        r["file_count"].as_i64().unwrap_or(0),
                        r["languages"].as_array().map(|l| l.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")).unwrap_or_default(),
                        if r["copyleft"].as_bool() == Some(true) { "  [copyleft]" } else { "" }
                    ));
                }
                out(&s);
                return Ok(());
            }
            v
        }
        Cmd::Mirror { url, tag } => c.post("/v1/warehouse/repos", &json!({"url": url, "tier": "mirror", "tags": tag})).await?,
        Cmd::Own { url, name, tag } => {
            c.post("/v1/warehouse/repos", &json!({"url": url, "tier": "own", "name": name, "tags": tag})).await?
        }
        Cmd::Sync { repo, force } => c.post("/v1/warehouse/sync", &json!({"slug": repo, "force": force})).await?,
        Cmd::Remove { repo, delete_files } => {
            c.post("/v1/warehouse/remove", &json!({"slug": repo, "delete_files": delete_files})).await?
        }
        Cmd::Resolve { name } => c.get_q("/v1/warehouse/resolve", &[("name", Some(name.join(" ")))]).await?,
        Cmd::Ask { question, scope, max_tokens } => {
            let v = c
                .post("/v1/warehouse/query", &json!({"question": question.join(" "), "scope": scope, "max_tokens": max_tokens}))
                .await?;
            if !j {
                out(&format!(
                    "{}\nqa_id: {}\n",
                    v["context_md"].as_str().filter(|s| !s.is_empty()).unwrap_or("(nothing found)"),
                    v["qa_id"].as_str().unwrap_or("-")
                ));
                return Ok(());
            }
            v
        }
        Cmd::Search { query, repo, lang, tier, limit } => {
            let v = c
                .post(
                    "/v1/warehouse/search",
                    &json!({"query": query.join(" "), "repos": repo, "languages": lang, "tiers": tier, "limit": limit}),
                )
                .await?;
            if !j {
                hits(&v, |h| {
                    let sym = h["symbol"]["name"].as_str().map(|s| format!(" ({s})")).unwrap_or_default();
                    format!(
                        "{}:{}:{}-{}{sym}",
                        h["repo"].as_str().unwrap_or(""),
                        h["path"].as_str().unwrap_or(""),
                        h["line_start"],
                        h["line_end"]
                    )
                });
                return Ok(());
            }
            v
        }
        Cmd::Grep { pattern, repo, regex, ignore_case, glob, max } => {
            let v = c
                .post(
                    "/v1/warehouse/grep",
                    &json!({"pattern": pattern, "repos": repo, "regex": regex, "ignore_case": ignore_case, "path_glob": glob, "max_matches": max}),
                )
                .await?;
            if !j {
                let mut s = String::new();
                for m in v["matches"].as_array().into_iter().flatten() {
                    s.push_str(&format!(
                        "{}:{}:{}: {}\n",
                        m["repo"].as_str().unwrap_or(""),
                        m["path"].as_str().unwrap_or(""),
                        m["line"],
                        m["text"].as_str().unwrap_or("")
                    ));
                }
                if v["truncated"].as_bool() == Some(true) {
                    s.push_str("(truncated)\n");
                }
                out(&s);
                return Ok(());
            }
            v
        }
        Cmd::Symbols { name, repo, kind, limit } => {
            let v = c.post("/v1/warehouse/symbols", &json!({"name": name, "repos": repo, "kind": kind, "limit": limit})).await?;
            if !j {
                let mut s = String::new();
                for h in v.as_array().into_iter().flatten() {
                    s.push_str(&format!(
                        "{:8} {}:{}:{}  {}\n",
                        h["kind"].as_str().unwrap_or(""),
                        h["repo"].as_str().unwrap_or(""),
                        h["path"].as_str().unwrap_or(""),
                        h["line_start"],
                        h["signature"].as_str().or(h["name"].as_str()).unwrap_or("")
                    ));
                }
                out(&s);
                return Ok(());
            }
            v
        }
        Cmd::Read { repo, path, lines, symbol } => {
            let (start, end) = match lines.as_deref().map(|l| l.split_once(':').unwrap_or((l, ""))) {
                Some((a, b)) => (Some(a.to_string()), (!b.is_empty()).then(|| b.to_string())),
                None => (None, None),
            };
            let v = c
                .get_q("/v1/warehouse/read", &[("repo", Some(repo)), ("path", path), ("symbol", symbol), ("start", start), ("end", end)])
                .await?;
            if !j {
                let view = if v.get("view").is_some() { &v["view"] } else { &v };
                out(&format!("{}\n", view["content"].as_str().unwrap_or("")));
                return Ok(());
            }
            v
        }
        Cmd::Card { repo } => {
            let v = c.get_q("/v1/warehouse/card", &[("slug", Some(repo))]).await?;
            if !j {
                out(&format!("{}\n", v["card_md"].as_str().unwrap_or("")));
                return Ok(());
            }
            v
        }
        Cmd::Component { cmd } => match cmd {
            CompCmd::List { status, origin, tag } => {
                c.get_q("/v1/warehouse/components", &[("status", status), ("origin", origin), ("tag", tag)]).await?
            }
            CompCmd::Approve { slug } => c.post(&format!("/v1/warehouse/components/{slug}/approve"), &json!({})).await?,
            CompCmd::Retire { slug } => c.post(&format!("/v1/warehouse/components/{slug}/retire"), &json!({})).await?,
            CompCmd::Use { slug, project, commit } => {
                c.post(&format!("/v1/warehouse/components/{slug}/use"), &json!({"project": project, "commit": commit})).await?
            }
            CompCmd::Create { file } => {
                let body: Value =
                    serde_json::from_str(&std::fs::read_to_string(&file).with_context(|| format!("reading {file}"))?)?;
                c.post("/v1/warehouse/components", &body).await?
            }
        },
        Cmd::Design { cmd } => match cmd {
            DesignCmd::Search { query, theme, font, color, kind, tag, limit } => {
                let v = c
                    .post(
                        "/v1/designs/search",
                        &json!({"query": query.join(" "), "theme": theme, "font": font, "color": color, "kind": kind, "tag": tag, "limit": limit.unwrap_or(6)}),
                    )
                    .await?;
                if !j {
                    let mut s = String::new();
                    for h in v["hits"].as_array().into_iter().flatten() {
                        s.push_str(&format!(
                            "{:.3}  {:22} {:9} {:5} bg {} ink {} accent {}  {}\n       {}\n",
                            h["score"].as_f64().unwrap_or(0.0),
                            h["slug"].as_str().unwrap_or(""),
                            h["kind"].as_str().unwrap_or(""),
                            h["theme"].as_str().unwrap_or("?"),
                            h["bg"].as_str().unwrap_or("?"),
                            h["fg"].as_str().unwrap_or("?"),
                            h["accent"].as_str().unwrap_or("?"),
                            h["fonts"].as_array().map(|f| f.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default(),
                            h["summary"].as_str().unwrap_or("").chars().take(160).collect::<String>(),
                        ));
                        for sh in h["shots"].as_array().into_iter().flatten() {
                            s.push_str(&format!(
                                "       [{}] {} — {}\n",
                                sh["view"].as_str().unwrap_or(""),
                                sh["page_title"].as_str().unwrap_or(""),
                                sh["caption"].as_str().unwrap_or("(no caption yet)").chars().take(140).collect::<String>()
                            ));
                        }
                    }
                    s.push_str(&format!("lists: {}\n", v["lists"]));
                    out(&s);
                    return Ok(());
                }
                v
            }
            DesignCmd::List { status, kind, theme, font } => {
                let v = c.get_q("/v1/designs", &[("status", status), ("kind", kind), ("theme", theme), ("font", font)]).await?;
                if !j {
                    let mut s = String::new();
                    for d in v.as_array().into_iter().flatten() {
                        s.push_str(&format!(
                            "{:22} {:9} {:9} {:5} bg {:8} accent {:8} {:>3} shots ({} captioned)  {}\n",
                            d["slug"].as_str().unwrap_or(""),
                            d["kind"].as_str().unwrap_or(""),
                            d["status"].as_str().unwrap_or(""),
                            d["theme"].as_str().unwrap_or("?"),
                            d["bg"].as_str().unwrap_or("?"),
                            d["accent"].as_str().unwrap_or("?"),
                            d["shots"],
                            d["captioned"],
                            d["fonts"].as_array().map(|f| f.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default(),
                        ));
                    }
                    out(&s);
                    return Ok(());
                }
                v
            }
            DesignCmd::Show { slug } => c.get(&format!("/v1/designs/{slug}")).await?,
            DesignCmd::Read { slug, path } => {
                let v = c.get_q(&format!("/v1/designs/{slug}/file"), &[("path", Some(path))]).await?;
                if !j {
                    out(&format!("{}\n", v["content"].as_str().unwrap_or("")));
                    return Ok(());
                }
                v
            }
            DesignCmd::Add { slug, capture, kind, file, tag, name } => {
                let body = match file {
                    Some(f) => serde_json::from_str(&std::fs::read_to_string(&f).with_context(|| format!("reading {f}"))?)?,
                    None => json!({
                        "slug": slug, "kind": kind, "capture": capture.or_else(|| Some(slug.clone())),
                        "tags": tag, "name": name, "created_by": ""
                    }),
                };
                c.post("/v1/designs", &body).await?
            }
            DesignCmd::Approve { slug } => c.post(&format!("/v1/designs/{slug}/approve"), &json!({})).await?,
            DesignCmd::Retire { slug } => c.post(&format!("/v1/designs/{slug}/retire"), &json!({})).await?,
            DesignCmd::Caption => c.post("/v1/designs/caption", &json!({})).await?,
        },
        Cmd::Jobs => c.get("/v1/jobs").await?,
    };
    print(&v, j);
    Ok(())
}

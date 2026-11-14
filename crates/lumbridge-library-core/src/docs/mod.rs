//! Lumbridge Docs engine (`lumbridge-docs`): vault scan → documents/chunks/links → embed backlog → hybrid search.
//!
//! The vault (`{library_root}/vault`, a git checkout of `[vault] remote`) is the source of truth;
//! everything here is an index that [`sync_vault`] can rebuild, except facts (BACKUP).
//!
//! - [`sync_vault`] / [`sync_file`]: walk → frontmatter → chunks → links, content-hashed so unchanged
//!   files cost one sha256 and unchanged chunks keep their embeddings.
//! - [`embed_pending`]: the embed backlog (`embedding is null` or a different model).
//! - [`search`] (FTS + HNSW + named entities → RRF → rerank → blend → relevance floor), [`resolve`]
//!   (name → id), [`mentions`] (names in a question), [`query`] (packed
//!   context with citations), [`read`] (one document with links and backlinks).
//! - [`names`]: look-alike name guard; `search`/`query` flag hits about `Intel Gaudi 3` when the
//!   question asks for `Intel Gaudi 5`, which the vault has no page for (`name_mismatch`).
//! - [`facts`] / [`add_fact`]: dated facts on entities, ADD / UPDATE / NOOP.
//! - [`write_doc`] / [`clip`] / [`push_vault`] / [`pull_vault`]: agent writes (Wiki, References,
//!   Clippings only; never the owner's files), logged in `log.md` and committed with git.
//! - [`lint`]: read-only health report.

pub mod chunk;
pub mod embed;
pub mod facts;
pub mod frontmatter;
pub mod lint;
pub mod links;
pub mod names;
pub mod read;
pub mod resolve;
pub mod search;
pub mod sync;
pub mod write;

pub use chunk::{Chunk, chunk_markdown};
pub use embed::embed_pending;
pub use facts::{Fact, FactAction, FactOutcome, NewFact, add_fact, facts};
pub use frontmatter::{Frontmatter, parse_document};
pub use lint::{LintReport, lint};
pub use links::{WikiLink, parse_wikilinks};
pub use names::{NameMismatch, NamedDoc};
pub use read::{DocFull, LinkRef, read};
pub use resolve::{Mention, Resolved, mentions, resolve};
pub use search::{Citation, DocHit, QueryAnswer, SearchParams, SearchResult, query, search};
pub use sync::{ParseError, SyncReport, sync_file, sync_vault};
pub use write::{Clip, WriteDoc, WriteOutcome, agent_owned, append_log_entry, clip, pull_vault, push_vault, write_doc};

use sha2::{Digest, Sha256};

/// Files above this are skipped by the sync (the vault is prose; big files belong in Attachments/).
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Layers agents may write into (top-level vault folders).
pub const WRITABLE_FOLDERS: [&str; 3] = ["Wiki", "References", "Clippings"];

pub fn sha256_hex(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in d {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// `kb.documents.layer` from a vault-relative path.
pub fn layer_for(path: &str) -> &'static str {
    let Some((top, _)) = path.split_once('/') else { return "meta" };
    match top {
        "Notes" => "note",
        "Thoughts" => "thought",
        "Daily" => "daily",
        "Clippings" => "clipping",
        "References" => "reference",
        "Wiki" => "wiki",
        "Templates" => "template",
        _ => "meta",
    }
}

/// Default author when frontmatter has none: the owner's own layers are the owner's; the rest are
/// the default agent's (`[identity]`). Only the index records this; write access is decided from
/// the file's own frontmatter.
pub fn default_author(layer: &str) -> &'static str {
    let id = crate::identity::get();
    match layer {
        "note" | "thought" | "daily" => &id.owner,
        _ => id.default_agent(),
    }
}

/// The text actually sent to the embedder for a chunk: title (and breadcrumb) give short chunks
/// context. The chunk's `sha256` is taken over this exact text, so an embedding is reused only when
/// the embedder would have seen identical input.
pub fn embed_text(title: &str, heading: &str, content: &str) -> String {
    if heading.is_empty() || heading == title {
        format!("{title}\n\n{content}")
    } else {
        format!("{title}\n{heading}\n\n{content}")
    }
}

/// Filename stem of a vault path (`Wiki/GB10 memory.md` → `GB10 memory`).
pub fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".md").unwrap_or(name)
}

/// Strip an optional `/docs/` id prefix.
pub(crate) fn strip_docs_prefix(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix("/docs/").unwrap_or(s)
}

/// Lowercase slug: `NVIDIA GB10` → `nvidia-gb10`.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Truncate to at most `n` chars on a char boundary.
pub(crate) fn truncate_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_and_defaults() {
        assert_eq!(layer_for("Notes/a.md"), "note");
        assert_eq!(layer_for("Categories/GPUs.md"), "meta");
        assert_eq!(layer_for("index.md"), "meta");
        assert_eq!(layer_for("Wiki/sub/x.md"), "wiki");
        assert_eq!(default_author("daily"), "owner");
        assert_eq!(default_author("wiki"), "agent");
    }

    #[test]
    fn helpers() {
        assert_eq!(slugify("NVIDIA GB10 (Spark)"), "nvidia-gb10-spark");
        assert_eq!(stem("Wiki/GB10 memory.md"), "GB10 memory");
        assert_eq!(sha256_hex(b"").len(), 64);
        assert!(sha256_hex(b"abc").starts_with("ba7816bf"));
        assert_eq!(truncate_chars("héllo", 2), "hé");
    }
}

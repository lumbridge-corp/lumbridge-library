# AGENTS.md: Lumbridge Library (lumbridge-libraryd, Lumbridge Docs, Lumbridge Warehouse)

Rules for anyone (human or agent) changing this repo.

- **Names other programs use are a contract.** MCP tool names (pinned by a test in `mcp.rs`), the
  HTTP routes and headers, the `library_*` metrics, and the environment variables and folders that
  earlier releases used must keep working. Every such name lives in `crates/lumbridge-library-legacy`
  and nowhere else. Data on disk (vault, mirrors, database) is never moved or rewritten by a code change.
- **Git is the source of truth** for prose (the vault) and code (warehouse repos). Postgres is an
  index you can rebuild, except tables marked BACKUP in `migrations/0001_init.sql` (facts,
  observations, qa, evals, eval_runs, datasets, components, usage).
- **Migrations are append-only.** Never change what an applied migration does; add `NNNN_name.sql`.
  If the text of a released migration must change (a comment), add its old checksum to
  `EARLIER_CHECKSUMS` in `crates/lumbridge-library-core/src/db.rs`, or existing databases stop starting.
- **Vectors are `extensions.halfvec(2048)` and always carry `embed_model_id`.** Never compare vectors
  across fingerprints; re-embed instead.
- **Embed prefixes come from config** (`query_prefix` / `passage_prefix`); `models::Embedder` adds
  them, so don't add your own.
- **The owner's documents are never edited by agents.** Agents write Wiki/, References/ and
  Clippings/ only, and only over files whose `author` is a configured agent (`[identity]`).
- **No names in code.** The owner, the agent authors and the git identity come from `[identity]`;
  addresses and model endpoints come from the config. Defaults are loopback or unset.
- **Licenses:** a component's license is inherited from its source. Copyleft sources (GPL/AGPL/LGPL/MPL)
  are flagged and never copied into closed projects. A `cleanroom` component must be implemented from its
  spec by an agent that has not read the source.
- **Memory:** the shipped unit sets `MemoryHigh=512M` / `MemoryMax=1G` (git children included), to sit
  next to model servers on the same machine. Stream, don't buffer whole repos.
- **Designs** (tier `design`, `wh.designs`): the repo holds only our own write-up / tokens / rebuilt
  galleries. Raw captures (screenshots, page HTML, assets) stay under `{root}/warehouse/designs/` on
  the server: third-party material, never committed, never shipped.
- Run `scripts/ci.sh` (scrub check, clippy, tests) before pushing.

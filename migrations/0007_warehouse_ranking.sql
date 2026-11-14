-- Lumbridge Library schema v7: warehouse ranking.
-- Idempotent; only touches wh.*.
--
-- Code-aware lexical matching. wh.chunks.tsv uses the 'simple' parser over path + content, which
-- keeps `StreamableHttpService` and `crates/ignore/src/gitignore.rs` as single tokens. The indexer
-- now also writes, per chunk, normalised texts where camelCase / snake_case identifiers and paths
-- (split on / . _ -) become words, each with a crude stem: the symbol's name + parent
-- (search_text, weight A), the path segments + signature identifiers (path_text, weight C) and
-- the words of compound identifiers in the content (ident_text, weight D). Search matches and
-- ranks on `c.tsv || t.tsv` (content stays weight B in c.tsv).
--
-- It lives in a side table on purpose: adding or regenerating a stored column on wh.chunks would
-- rewrite every row (each carries a 2048-dim halfvec) and re-insert all of them into the HNSW
-- index. Rows are filled by the indexer for new chunks and by
-- `warehouse::reindex_search_text` / every `sync_repo` for chunks indexed before this migration.
create table if not exists wh.chunk_terms (
  chunk_id     bigint primary key references wh.chunks(id) on delete cascade,
  search_text  text not null,
  path_text    text not null default '',
  ident_text   text not null default '',
  tsv          tsvector generated always as (
                 setweight(to_tsvector('simple', search_text), 'A') ||
                 setweight(to_tsvector('simple', path_text), 'C') ||
                 setweight(to_tsvector('simple', ident_text), 'D')) stored
);
create index if not exists wh_chunk_terms_tsv_idx on wh.chunk_terms using gin (tsv);

-- Package names found while indexing (root package first): Cargo.toml [package] name, workspace
-- members, package.json / pyproject.toml names, go.mod module. Used by resolve() and repo
-- detection (`rmcp` → modelcontextprotocol/rust-sdk).
alter table wh.repos add column if not exists crates text[] not null default '{}';
create index if not exists wh_repos_crates_idx on wh.repos using gin (crates);

-- Symbols the C++ grammar emitted without a name are useless for lookup (and matched `*`).
delete from wh.symbols where btrim(name) = '';

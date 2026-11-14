-- Lumbridge Library schema v6: docs ranking. Append-only on top of 0005.
--
-- 1. Chunks carry their document's title + aliases (`title`, kept in step by the sync) so the
--    full-text vector can weight them like the heading (A). A reference page's spec chunk often says
--    "Memory: 144 GB" without naming the chip; the title is what the question names.
-- 2. Chunks carry the 1-based body line range they cover (`line_start`/`line_end`, frontmatter
--    excluded, same numbering as docs_read) so a hit can be turned into a read range.
-- 3. Every document is marked for re-index (sha256 cleared) so the next sync re-chunks it and fills
--    both. Embeddings are reused: the chunk hash is over the embed text, which does not change.

alter table kb.chunks
  add column title      text not null default '',
  add column line_start int,
  add column line_end   int;

update kb.chunks c
   set title = btrim(d.title || ' ' || array_to_string(d.aliases, ' '))
  from kb.documents d
 where d.id = c.doc_id;

-- tsv is a generated column: drop and re-add it (its GIN index goes with it) with the title at A.
alter table kb.chunks drop column tsv;
alter table kb.chunks add column tsv tsvector generated always as (
  setweight(to_tsvector('english', title), 'A') ||
  setweight(to_tsvector('english', heading), 'A') ||
  setweight(to_tsvector('english', content), 'B')) stored;
create index chunks_tsv on kb.chunks using gin (tsv);

-- Force a re-chunk on the next sync (sync compares file hashes with this column).
update kb.documents set sha256 = 'reindex:0006';

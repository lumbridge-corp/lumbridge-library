-- Lumbridge Library schema v2: docs engine (vault sync, hybrid search, resolve, facts, lint).
-- Append-only on top of 0001. pg_trgm lives in schema `extensions` (see deploy/db-setup.sql).

create extension if not exists pg_trgm with schema extensions;

-- Columns the sync derives from frontmatter so filters and resolve don't have to dig in jsonb.
alter table kb.documents
  add column aliases     text[]  not null default '{}',
  add column private     boolean not null default false,
  add column parse_error text;    -- frontmatter that failed to parse (file still indexed); lint reports it

create index documents_aliases_gin on kb.documents using gin (aliases);
create index documents_title_trgm  on kb.documents using gin (lower(title) extensions.gin_trgm_ops);
create index documents_path_trgm   on kb.documents using gin (path extensions.gin_trgm_ops);
create index entities_name_trgm    on kb.entities  using gin (lower(name) extensions.gin_trgm_ops);
create index entities_slug_trgm    on kb.entities  using gin (slug extensions.gin_trgm_ops);

-- Wikilink parts: [[dst_text|alias]] where dst_text may carry #heading; kind = wikilink | embed.
alter table kb.links
  add column heading text,
  add column alias   text;
create index links_dst_entity on kb.links (dst_entity);

-- Links keep their raw text when the target goes away (breadcrumbs for lint), so a deleted target
-- nulls the pointer instead of deleting the link row.
alter table kb.links
  drop constraint links_dst_doc_fkey,
  add constraint links_dst_doc_fkey foreign key (dst_doc) references kb.documents(id) on delete set null,
  drop constraint links_dst_entity_fkey,
  add constraint links_dst_entity_fkey foreign key (dst_entity) references kb.entities(id) on delete set null;

-- Embed backlog and per-model vector search.
create index chunks_unembedded on kb.chunks (id) where embedding is null;
create index chunks_embed_model on kb.chunks (embed_model_id);

-- Sources are deduped by URI when created from facts / clippings.
create index sources_uri on kb.sources (uri) where uri is not null;

-- Fact history per attribute (the 0001 index covers current facts only).
create index facts_entity_attr_recorded on kb.facts (entity_id, attribute, recorded_at desc);

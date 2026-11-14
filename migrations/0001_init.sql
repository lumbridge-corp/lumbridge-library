-- Lumbridge Library schema v1: docs (kb), warehouse (wh), shared ledger (lib).
-- Runs as lib_writer (owner). Vectors are halfvec(2048): the embedding model is 2048-d and
-- pgvector's HNSW index caps plain `vector` at 2000 dims; halfvec indexes up to 4000 at half the size.
-- Git is the source of truth for prose and code. Tables marked BACKUP hold data that git cannot rebuild.

create schema if not exists lib;
create schema if not exists kb;
create schema if not exists wh;

grant usage on schema lib, kb, wh to lib_reader;
alter default privileges in schema lib grant select on tables to lib_reader;
alter default privileges in schema kb  grant select on tables to lib_reader;
alter default privileges in schema wh  grant select on tables to lib_reader;

-- ───────────────────────────── lib: shared ─────────────────────────────

-- Every vector records the model that made it, so a model swap never mixes spaces (idea from ck).
create table lib.embed_models (
  id           serial primary key,
  name         text not null,              -- served name, e.g. "embed"
  root         text not null,              -- weights path reported by /v1/models
  dims         int  not null,
  fingerprint  text not null unique,       -- blake3(name|root|dims|prefix scheme)
  first_seen   timestamptz not null default now(),
  last_seen    timestamptz not null default now()
);

create table lib.ops_log (
  id      bigserial primary key,
  at      timestamptz not null default now(),
  op      text not null,                   -- ingest | index | lint | sync | migrate | ...
  shelf   text check (shelf in ('docs', 'warehouse', 'library')),
  target  text,
  ok      boolean not null default true,
  detail  jsonb not null default '{}'
);
create index on lib.ops_log (at desc);

-- BACKUP. Every answer given from the library, with what it cited and the owner's verdict: training gold.
create table lib.qa (
  id           uuid primary key default gen_random_uuid(),
  at           timestamptz not null default now(),
  shelf        text not null check (shelf in ('docs', 'warehouse', 'library')),
  asked_by     text not null,              -- MCP client name | cli | ...
  question     text not null,
  resolved_id  text,                       -- e.g. /docs/references/nvidia-gb10, /warehouse/mirrors/DeepGEMM
  cited        jsonb not null default '[]',-- [{kind, id, path, lines, score}]
  answer       text,
  model        text,
  trace_id     text,                       -- trace id in your tracing tool, if any
  verdict      smallint check (verdict in (-1, 0, 1)),
  correction   text,
  verdict_at   timestamptz
);
create index on lib.qa (at desc);

-- BACKUP. Frozen question sets and model scores over time.
create table lib.evals (
  id          uuid primary key default gen_random_uuid(),
  set_name    text not null,               -- gpu-facts | ops | prefs | code-nav ...
  question    text not null,
  expected    text not null,
  grader      text not null default 'judge', -- exact | contains | judge
  tags        text[] not null default '{}',
  active      boolean not null default true,
  created_at  timestamptz not null default now()
);
create table lib.eval_runs (
  id        bigserial primary key,
  eval_id   uuid not null references lib.evals(id) on delete cascade,
  model     text not null,
  answer    text,
  score     real,
  run_at    timestamptz not null default now(),
  detail    jsonb not null default '{}'
);

-- BACKUP. Hashed training exports so any LoRA run can be reproduced.
create table lib.datasets (
  id          uuid primary key default gen_random_uuid(),
  name        text not null,
  kind        text not null,               -- sft | dpo | eval | distill
  sha256      text not null,
  rows        int  not null,
  path        text not null,               -- {library_root}/datasets/...
  filters     jsonb not null default '{}',
  created_at  timestamptz not null default now()
);

-- ───────────────────────────── kb: docs ─────────────────────────────

create table kb.sources (
  id          uuid primary key default gen_random_uuid(),
  kind        text not null,               -- url | pdf | book | repo | thought | note | trace | dataset
  uri         text,
  title       text,
  author      text,
  published   date,
  fetched_at  timestamptz not null default now(),
  sha256      text,
  license     text,
  private     boolean not null default true,
  meta        jsonb not null default '{}'
);
create unique index on kb.sources (sha256) where sha256 is not null;

-- One row per vault file. Rebuildable from the vault.
create table kb.documents (
  id           uuid primary key default gen_random_uuid(),
  path         text not null unique,       -- vault-relative, e.g. References/NVIDIA GB10.md
  layer        text not null check (layer in ('note', 'thought', 'daily', 'clipping', 'reference', 'wiki', 'template', 'meta')),
  author       text not null,              -- the owner | an agent author
  status       text not null default 'draft' check (status in ('raw', 'draft', 'reviewed')),
  title        text not null,
  categories   text[] not null default '{}',
  frontmatter  jsonb not null default '{}',
  body         text not null,
  sha256       text not null,
  source_id    uuid references kb.sources(id) on delete set null,
  created      date,
  updated_at   timestamptz not null default now(),
  indexed_at   timestamptz
);
create index on kb.documents using gin (categories);
create index on kb.documents (layer);

create table kb.chunks (
  id              bigserial primary key,
  doc_id          uuid not null references kb.documents(id) on delete cascade,
  ord             int  not null,
  heading         text not null default '',
  content         text not null,
  tokens          int  not null,
  sha256          text not null,           -- content hash: unchanged chunks are never re-embedded
  embedding       extensions.halfvec(2048),
  embed_model_id  int references lib.embed_models(id),
  tsv             tsvector generated always as (
                    setweight(to_tsvector('english', coalesce(heading, '')), 'A') ||
                    setweight(to_tsvector('english', content), 'B')) stored,
  unique (doc_id, ord)
);
create index on kb.chunks using gin (tsv);
create index on kb.chunks using hnsw (embedding extensions.halfvec_cosine_ops);
create index on kb.chunks (sha256);

create table kb.entities (
  id       uuid primary key default gen_random_uuid(),
  slug     text not null unique,           -- nvidia-gb10, deepseek-v4-1-flash, vllm
  kind     text not null,                  -- chip | model | engine | provider | person | company | paper | book | repo
  name     text not null,
  aliases  text[] not null default '{}',
  doc_id   uuid references kb.documents(id) on delete set null
);
create index on kb.entities using gin (aliases);

-- Wikilinks and entity mentions: the graph.
create table kb.links (
  id          bigserial primary key,
  src_doc     uuid not null references kb.documents(id) on delete cascade,
  dst_doc     uuid references kb.documents(id) on delete cascade,
  dst_entity  uuid references kb.entities(id) on delete cascade,
  dst_text    text not null,               -- raw [[target]], kept even if unresolved (breadcrumbs)
  kind        text not null default 'wikilink'
);
create index on kb.links (src_doc);
create index on kb.links (dst_doc);

-- BACKUP. Dated, sourced facts with two timelines (idea from graphiti): valid_* = when it was true,
-- recorded_at = when we learned it. Never deleted; a newer fact sets superseded_by on the old one.
create table kb.facts (
  id             uuid primary key default gen_random_uuid(),
  entity_id      uuid not null references kb.entities(id) on delete cascade,
  attribute      text not null,            -- memory_bandwidth_gbs, price_usd_per_gpu_hr ...
  value_num      numeric,
  value_hi       numeric,                  -- for ranges: value_num..value_hi
  unit           text,
  value_text     text,
  valid_from     date,
  valid_to       date,
  recorded_at    timestamptz not null default now(),
  source_id      uuid references kb.sources(id) on delete set null,
  confidence     real check (confidence between 0 and 1),
  verified_by    text,                     -- owner | measured | vendor | community
  superseded_by  uuid references kb.facts(id),
  note           text
);
create index on kb.facts (entity_id, attribute) where superseded_by is null;

-- BACKUP. Time series from collectors (gpu prices, availability).
create table kb.observations (
  id           bigserial primary key,
  series       text not null,              -- e.g. gpu-rental
  entity_id    uuid references kb.entities(id) on delete set null,
  provider     text,
  metric       text not null,
  value        numeric not null,
  unit         text,
  observed_at  timestamptz not null,
  source_id    uuid references kb.sources(id) on delete set null,
  meta         jsonb not null default '{}'
);
create index on kb.observations (series, metric, observed_at desc);

-- ───────────────────────────── wh: warehouse ─────────────────────────────

-- Tiers: mirror = other people's code (bare mirrors on the server, never edited);
-- own = the owner's code; component = distilled / clean-room pieces in Gitea org `warehouse`.
create table wh.repos (
  id                   uuid primary key default gen_random_uuid(),
  slug                 text not null unique,   -- mirrors/DeepGEMM, own/my-repo, warehouse/sse-stream-parser
  tier                 text not null check (tier in ('mirror', 'own', 'component')),
  origin_url           text,
  gitea_full_name      text,
  local_path           text not null,          -- server path of the bare repo or checkout
  default_branch       text not null default 'main',
  head_commit          text,
  license_spdx         text,                   -- askalono result; NULL = unknown (treat as all-rights-reserved)
  license_confidence   real,
  copyleft             boolean,                -- GPL/AGPL/LGPL/MPL family: blocked for closed projects
  description          text,
  card_md              text,                   -- repo card (aider-style repo map + LLM summary)
  card_commit          text,
  stars                int,
  upstream_pushed_at   timestamptz,
  last_synced_at       timestamptz,
  last_indexed_commit  text,
  tags                 text[] not null default '{}',
  health               jsonb not null default '{}',
  added_by             text not null default 'owner',
  created_at           timestamptz not null default now()
);

create table wh.files (
  id              bigserial primary key,
  repo_id         uuid not null references wh.repos(id) on delete cascade,
  path            text not null,
  blob_sha        text not null,
  language        text,
  bytes           int not null,
  lines           int not null,
  indexed_commit  text not null,
  unique (repo_id, path)
);

create table wh.symbols (
  id          bigserial primary key,
  repo_id     uuid not null references wh.repos(id) on delete cascade,
  file_id     bigint not null references wh.files(id) on delete cascade,
  name        text not null,
  kind        text not null,                 -- function | method | struct | class | trait | enum | const | module ...
  parent      text,                          -- enclosing type / module
  signature   text,
  doc         text,
  line_start  int not null,
  line_end    int not null
);
create index on wh.symbols (name);
create index on wh.symbols (lower(name) text_pattern_ops);
create index on wh.symbols (repo_id);

create table wh.refs (
  id       bigserial primary key,
  repo_id  uuid not null references wh.repos(id) on delete cascade,
  file_id  bigint not null references wh.files(id) on delete cascade,
  name     text not null,
  line     int not null
);
create index on wh.refs (repo_id, name);

-- Whole-block code chunks (function/class granularity, idea from probe).
create table wh.chunks (
  id              bigserial primary key,
  repo_id         uuid not null references wh.repos(id) on delete cascade,
  file_id         bigint not null references wh.files(id) on delete cascade,
  symbol_id       bigint references wh.symbols(id) on delete set null,
  path            text not null,
  line_start      int not null,
  line_end        int not null,
  language        text,
  content         text not null,
  tokens          int not null,
  sha256          text not null,
  embedding       extensions.halfvec(2048),
  embed_model_id  int references lib.embed_models(id),
  tsv             tsvector generated always as (
                    setweight(to_tsvector('simple', path), 'A') ||
                    setweight(to_tsvector('simple', content), 'B')) stored
);
create index on wh.chunks using gin (tsv);
create index on wh.chunks using hnsw (embedding extensions.halfvec_cosine_ops);
create index on wh.chunks (repo_id);
create index on wh.chunks (sha256);

-- BACKUP. Components: distilled (copied with attribution), cleanroom (re-written from a spec without
-- looking at the source), or original. Mirrors the component.toml in each warehouse/* repo.
create table wh.components (
  id               uuid primary key default gen_random_uuid(),
  slug             text not null unique,
  summary          text not null,
  languages        text[] not null default '{}',
  origin           text not null check (origin in ('distilled', 'cleanroom', 'original')),
  source_repo      text,                     -- slug of the repo it came from / was specified from
  source_ref       text,                     -- commit:path
  spec_path        text,                     -- for cleanroom: the behaviour spec the implementer worked from
  license_spdx     text not null,
  tags             text[] not null default '{}',
  status           text not null default 'draft' check (status in ('draft', 'approved', 'retired')),
  repo_id          uuid references wh.repos(id) on delete set null,
  created_at       timestamptz not null default now(),
  approved_at      timestamptz
);

-- BACKUP. Where components were copied in: a signal for which ones earn their keep.
create table wh.usage (
  id            bigserial primary key,
  component_id  uuid not null references wh.components(id) on delete cascade,
  project       text not null,
  commit        text,
  added_at      timestamptz not null default now(),
  added_by      text not null
);

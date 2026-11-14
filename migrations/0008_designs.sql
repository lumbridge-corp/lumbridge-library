-- Lumbridge Library schema v8: the design shelf (warehouse designs).
-- Idempotent; only touches wh.*.
--
-- A design is a website / product design system kept as reference: our own write-up (DESIGN.md,
-- tokens.css, COMPONENTS.md, preview/gallery HTML) in a git repo indexed like any other repo (tier
-- `design`, slug `designs/<slug>`, published to Gitea <component_org>/design-<slug>), plus the raw capture
-- (screenshots, page HTML, assets: third-party, private reference only) on the server under
-- `{root}/warehouse/designs/<capture>/`, which is never put in git.
--
-- Search is text-only (the embedder reads text): each screenshot gets a caption from the vision model
-- (`[designs] vision_base_url`), and captions + the design's own text are embedded. Measured tokens become facets
-- (theme, colours, fonts, radius) for filtering.

alter table wh.repos drop constraint if exists repos_tier_check;
alter table wh.repos add constraint repos_tier_check check (tier in ('mirror', 'own', 'component', 'design'));

-- BACKUP. One row per design system. Mirrors design.toml in the design repo.
create table if not exists wh.designs (
  id              uuid primary key default gen_random_uuid(),
  slug            text not null unique,                 -- wafer-ai
  name            text not null,                        -- Wafer
  kind            text not null check (kind in ('capture', 'reference', 'own')),
                                                        -- capture = measured from a live site by a capture tool;
                                                        -- reference = a written design brief (no capture);
                                                        -- own = the owner's design system
  site_url        text,
  summary         text not null,
  theme           text check (theme in ('dark', 'light', 'both')),
  bg              text,                                 -- #rrggbb page background
  fg              text,                                 -- #rrggbb main ink
  accent          text,                                 -- #rrggbb brand / accent colour
  fonts           text[] not null default '{}',         -- family names, display first
  radius_px       real,                                 -- most common card / control radius
  tags            text[] not null default '{}',
  source          text,                                 -- where it came from (a capture tool, a design collection, ...)
  license_spdx    text not null default 'LicenseRef-private',
  status          text not null default 'draft' check (status in ('draft', 'approved', 'retired')),
  repo_id         uuid references wh.repos(id) on delete set null,
  capture_dir     text,                                 -- capture folder name under {root}/warehouse/designs/
  captured_at     timestamptz,
  page_count      int,
  facets          jsonb not null default '{}',          -- extra measured facts (spacing, type scale, shadows ...)
  search_text     text not null default '',             -- name + summary + facets + DESIGN.md head (what is embedded)
  embedding       extensions.halfvec(2048),
  embed_model_id  int references lib.embed_models(id),
  created_by      text not null,
  created_at      timestamptz not null default now(),
  updated_at      timestamptz not null default now(),
  approved_at     timestamptz,
  tsv             tsvector generated always as (
                    setweight(to_tsvector('simple', name || ' ' || slug), 'A') ||
                    setweight(to_tsvector('english', summary), 'B') ||   -- tags + fonts are in search_text
                    setweight(to_tsvector('english', search_text), 'C')) stored
);
create index if not exists wh_designs_tsv_idx on wh.designs using gin (tsv);
create index if not exists wh_designs_hnsw_idx on wh.designs using hnsw (embedding extensions.halfvec_cosine_ops);
create index if not exists wh_designs_pending_idx on wh.designs (id) where embedding is null;

-- Screenshots of a design (rebuildable from the capture folder; captions are cached here).
create table if not exists wh.design_shots (
  id              bigserial primary key,
  design_id       uuid not null references wh.designs(id) on delete cascade,
  path            text not null,                        -- relative to the capture folder: pages/00/desktop-fold.png
  view            text not null,                        -- fold | desktop | mobile | preview | components
  page_n          text,                                 -- 00..
  page_url        text,
  page_title      text,
  width           int,
  height          int,
  bytes           int not null default 0,
  caption         text,
  caption_model   text,
  captioned_at    timestamptz,
  caption_error   text,
  embedding       extensions.halfvec(2048),
  embed_model_id  int references lib.embed_models(id),
  tsv             tsvector generated always as (
                    setweight(to_tsvector('simple', coalesce(page_title, '')), 'B') ||
                    setweight(to_tsvector('english', coalesce(caption, '')), 'C')) stored,
  unique (design_id, path)
);
create index if not exists wh_design_shots_tsv_idx on wh.design_shots using gin (tsv);
create index if not exists wh_design_shots_hnsw_idx on wh.design_shots using hnsw (embedding extensions.halfvec_cosine_ops);
create index if not exists wh_design_shots_uncaptioned_idx on wh.design_shots (id) where caption is null and caption_error is null;

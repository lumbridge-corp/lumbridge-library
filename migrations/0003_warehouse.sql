-- Lumbridge Library schema v3: warehouse engine.
-- Idempotent and independent of 0002 (docs): only touches wh.*, every statement is IF NOT EXISTS.

-- Repo facts the card and resolve() need without scanning wh.files.
alter table wh.repos add column if not exists languages         text[] not null default '{}';  -- by bytes, desc
alter table wh.repos add column if not exists file_count        int;
alter table wh.repos add column if not exists head_committed_at timestamptz;

-- Incremental re-index deletes by file and cascades; these keep it from scanning whole tables.
create index if not exists wh_files_repo_lang_idx    on wh.files (repo_id, language);
create index if not exists wh_symbols_file_idx       on wh.symbols (file_id);
create index if not exists wh_refs_file_idx          on wh.refs (file_id);
create index if not exists wh_chunks_file_idx        on wh.chunks (file_id);
create index if not exists wh_chunks_symbol_idx      on wh.chunks (symbol_id);            -- on delete set null
create index if not exists wh_chunks_pending_idx     on wh.chunks (id) where embedding is null;
create index if not exists wh_chunks_model_idx       on wh.chunks (embed_model_id);
create index if not exists wh_components_repo_idx    on wh.components (repo_id);
create index if not exists wh_usage_component_idx    on wh.usage (component_id);

-- Fuzzy symbol / repo lookup when pg_trgm is installed (Supabase puts it in `extensions`).
do $$
declare
  ns text;
begin
  select n.nspname into ns
    from pg_extension e join pg_namespace n on n.oid = e.extnamespace
   where e.extname = 'pg_trgm';
  if ns is not null then
    execute format('create index if not exists wh_symbols_name_trgm_idx on wh.symbols using gin (name %I.gin_trgm_ops)', ns);
    execute format('create index if not exists wh_repos_slug_trgm_idx on wh.repos using gin (slug %I.gin_trgm_ops)', ns);
    execute format('create index if not exists wh_components_slug_trgm_idx on wh.components using gin (slug %I.gin_trgm_ops)', ns);
  end if;
end
$$;

-- One-time database setup for Lumbridge Library. Run it as a Postgres superuser, in the database
-- the library will use, with the two role passwords as psql variables:
--
--   psql -v ON_ERROR_STOP=1 -v wpw='writer-password' -v rpw='reader-password' -d library -f deploy/db-setup.sql
--
-- Needs Postgres 16 or later with the pgvector extension (0.7 or later, for halfvec) available.
-- It creates schema `extensions` with `vector` and `pg_trgm` in it, the role lib_writer (owns the
-- library's schemas; the daemon's DATABASE_URL) and lib_reader (read-only; READER_DATABASE_URL).
-- The daemon applies its own migrations on start (or with --migrate-only).
create schema if not exists extensions;
create extension if not exists vector with schema extensions;
create extension if not exists pg_trgm with schema extensions;
do $$ begin
  if not exists (select from pg_roles where rolname = 'lib_writer') then create role lib_writer login; end if;
  if not exists (select from pg_roles where rolname = 'lib_reader') then create role lib_reader login; end if;
end $$;
alter role lib_writer password :'wpw' connection limit 20;
alter role lib_reader password :'rpw' connection limit 20;
do $$ begin execute format('grant create on database %I to lib_writer', current_database()); end $$;
grant usage on schema extensions to lib_writer, lib_reader;
alter role lib_writer set search_path = lib, kb, wh, extensions, public;
alter role lib_reader set search_path = lib, kb, wh, extensions, public;
alter role lib_reader set default_transaction_read_only = on;
-- sqlx puts _sqlx_migrations in the first existing schema on search_path; without this it would try
-- `extensions` on a fresh database and fail with "permission denied for schema extensions".
create schema if not exists lib authorization lib_writer;

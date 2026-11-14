-- Activity feed: who did it and a one-line human summary, on top of the existing ops log.
alter table lib.ops_log add column if not exists actor text;
alter table lib.ops_log add column if not exists summary text;
create index if not exists ops_log_shelf_at on lib.ops_log (shelf, at desc);

-- Who recorded a verdict (cli = the owner at a terminal; an MCP client name = an agent relaying the owner).
-- Training exports can filter on it.
alter table lib.qa add column if not exists verdict_by text;

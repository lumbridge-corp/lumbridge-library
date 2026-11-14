# Installing Lumbridge Library

A full install on one Linux machine: the database, the model endpoints, the vault, the service and
the clients. The [README quick start](../README.md#quick-start) is the short version with Postgres
only.

Paths below use `~/.config/lumbridge-library/` for config and `~/lumbridge-library/` for data.
Every path can be changed in the config file.

## 1. Postgres

You need Postgres 16 or later with the pgvector extension, version 0.7 or later (for `halfvec`).
The `pgvector/pgvector:pg17` container image has both; on Ubuntu, the PostgreSQL project's apt
repository has `postgresql-17-pgvector`.

Create a database and run the setup script in it as a superuser. It creates the schema
`extensions` (with `vector` and `pg_trgm`), the role `lib_writer` that owns the library's schemas
and the read-only role `lib_reader`:

```sh
createdb library
psql -v ON_ERROR_STOP=1 -v wpw='a long writer password' -v rpw='a long reader password' \
  -d library -f deploy/db-setup.sql
```

The daemon applies its own migrations at start. To apply them without starting it:
`lumbridge-libraryd --migrate-only`.

Put the connection strings in `~/.config/lumbridge-library/db.env` (mode 0600). The systemd unit
reads this file:

```sh
DATABASE_URL=postgres://lib_writer:PASSWORD@127.0.0.1:5432/library
READER_DATABASE_URL=postgres://lib_reader:PASSWORD@127.0.0.1:5432/library
```

If your Postgres already keeps extensions in a schema named `extensions` (Supabase does), the
script reuses it.

## 2. Model endpoints

All model calls are OpenAI-compatible HTTP. The library works without any of them (full-text
search only); each one you add turns on more.

| Config table | Endpoint the daemon calls | Used for |
|---|---|---|
| `[embed]` | `POST {base_url}/embeddings` | search by meaning; must return 2048-dimension vectors |
| `[rerank]` | `POST {base_url}/rerank` (the vLLM rerank API) | ordering results and the relevance floor |
| `[librarian]` | `POST {base_url}/chat/completions` with `response_format: json_schema` | the nightly upkeep agent (optional) |
| `[designs]` | `POST {base_url}/chat/completions` with an image | captions for design screenshots (optional) |

vLLM serves all four. For embeddings, set `query_prefix` and `passage_prefix` to what your model
was trained with (`"query: "` and `"passage: "` are the defaults), or to `""` for a model that
takes plain text. Every vector is stored with the fingerprint of the model that made it; change the
model and the backlog is re-embedded, never mixed.

## 3. The vault

The docs shelf indexes a folder of Markdown files at `{library_root}/vault`. Top-level folders
decide each file's layer: `Notes/`, `Thoughts/`, `Daily/` (your own), `Clippings/`, `References/`,
`Wiki/` (where agents write), `Templates/`; anything else is `meta`. Frontmatter (`title`,
`author`, `categories`, `tags`, `entity`) is read when present.

To keep the vault in git, set `[vault] remote`. On start the daemon clones it if the folder is
empty, then pulls every `vault_sync_secs` and pushes its own commits when `push = true`. Use a
deploy key or a dedicated account that may push to that one repo.

Agents never edit files they did not write. Set `[identity]`:

```toml
[identity]
owner = "sam"                  # your author name; your pages are never edited by agents
agents = ["agent", "claude"]   # author names agents write under
librarian = "librarian"        # author name of the librarian's edits
```

An MCP client writes under the first agent name that its own client name contains (a client that
introduces itself as `claude-code` writes as `claude`), else under the first name in the list.

## 4. Tokens

```sh
mkdir -p ~/.config/lumbridge-library && cd ~/.config/lumbridge-library
head -c 48 /dev/urandom | base64 | tr -d '/+=' > token
head -c 48 /dev/urandom | base64 | tr -d '/+=' > admin-token
chmod 600 token admin-token
```

`token` is the bearer token every client sends. `admin-token` unlocks the owner-only REST actions
(approve, retire and remove; fetch a URL to clip; record a verdict from the CLI; export datasets).
MCP never accepts the admin token. Leave `admin_token_file` out of the config to turn those
actions off.

## 5. The config file

Copy [deploy/lumbridge-libraryd.toml.example](../deploy/lumbridge-libraryd.toml.example) to
`~/.config/lumbridge-library/lumbridge-libraryd.toml` and edit it. Paths must be absolute.

`listen` is plain HTTP. Keep it on `127.0.0.1` unless every machine that can reach the address is
yours; then also list the host names clients use in `allowed_hosts` (the MCP endpoint refuses
other `Host` headers).

## 6. Build, install, start

```sh
sh deploy/install.sh
```

It builds the three binaries, installs them to `~/.local/bin`, installs the URL clipper
(`tools/extract.py` and a Python venv with trafilatura and pypdf), copies
`deploy/lumbridge-libraryd.service` to `~/.config/systemd/user/` and starts it. `NO_SERVICE=1`
skips systemd; `CARGO`, `BIN`, `UNIT`, `DATA_ROOT` and `HEALTH_URL` change the defaults (see the
top of the script). To keep the service running after you log out: `loginctl enable-linger`.

Check it:

```sh
curl -s http://127.0.0.1:8830/health
lumbridge-docs status
```

## 7. Clients

The CLIs read `LUMBRIDGE_LIBRARY_URL` (default `http://127.0.0.1:8830`) and the token from
`LUMBRIDGE_LIBRARY_TOKEN` or `~/.config/lumbridge-library/token`, and the admin token from
`LUMBRIDGE_LIBRARY_ADMIN_TOKEN` or `~/.config/lumbridge-library/admin-token` when present.

Register the MCP endpoint in each agent. For Claude Code:

```sh
claude mcp add --scope user --transport http library http://127.0.0.1:8830/mcp \
  --header "Authorization: Bearer $(cat ~/.config/lumbridge-library/token)"
```

The agent then sees the tools as `mcp__library__docs_query` and so on. The tool names are fixed.

## 8. Optional parts

- **Gitea** (`[gitea]`): components and designs are published as private repos in
  `component_org`. Needs an API token with repository and organisation write access, and an SSH
  host the daemon can push to.
- **Librarian** (`[librarian]`): set `enabled = true` for a nightly run at `nightly_hour`. It
  starts in `dry_run` and only reports; set `dry_run = false` to let it record facts and add links.
- **Metrics**: `GET /metrics` is Prometheus text. Alert rules:
  [deploy/prometheus-lumbridge-library.rules.yml](../deploy/prometheus-lumbridge-library.rules.yml).

## Upgrading

Stop nothing by hand: `sh deploy/install.sh` builds, swaps the binaries and restarts the unit.
Migrations run on start. Version 0.1.0 edited the comments of migrations 1 to 4 and 6 to 8; a
database created by an earlier build has their earlier checksums, and the daemon replaces those
with the current ones before it migrates. An earlier build will not start against that database
afterwards.

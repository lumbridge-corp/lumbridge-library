![Lumbridge Library: What your team knows, with sources.](docs/assets/banner.jpg)

# Lumbridge Library

A private library for one person and their agents: a docs shelf (a Markdown vault in git, with
dated facts) and a code shelf (git mirrors of other people's repos, your own repos, and reusable
components), indexed in Postgres and served over HTTP and MCP. It is for people who want their
agents to answer from their own notes and from real code, with citations, on their own machine.
Every answer is logged with what it cited, so your verdicts become evaluation and training data.

Part of [Lumbridge](https://lumbridgecorp.com) ([all repositories](https://github.com/lumbridge-corp)). In daily use.

```
$ lumbridge-docs search retried backoff
1.000  Notes/Retry policy.md
       Failed jobs are retried five times with exponential backoff.
$ lumbridge-warehouse grep "max_retries" -r acme/queue
$ lumbridge-docs ask how many times are jobs retried
[1] Retry policy (Notes/Retry policy.md)
...
qa_id: e31bf9d8-24d1-409d-a06e-9a8cbd1f693c
```

## Status

- **Version:** 0.1.0. Interfaces can change until 1.0.
- **Runs on:** Linux (x86_64 and aarch64). The CLIs also build on macOS.
- **Not yet:**
  - No packages or downloads: you build it from source.
  - No container image or compose file. You bring Postgres with pgvector and, for search by meaning,
    an embedding and a rerank endpoint.
  - The embedding size is fixed at 2048 by the schema; another size needs a new migration.
  - Plain HTTP only. Run it on loopback or a private network; put TLS in front of it yourself.
  - The docs and warehouse engines live in one crate; splitting them is planned.

## Install

You need: Rust 1.97 or later, git, a C compiler (tree-sitter grammars), Postgres 16 or later with
the [pgvector](https://github.com/pgvector/pgvector) extension (0.7 or later), and Python 3 with
`venv` if you want the server to clip URLs.

On Ubuntu 24.04:

```sh
sudo apt install build-essential git curl python3-venv postgresql-client
git clone https://github.com/lumbridge-corp/lumbridge-library.git
cd lumbridge-library
cargo build --release --locked
install -m 755 target/release/lumbridge-libraryd target/release/lumbridge-docs target/release/lumbridge-warehouse ~/.local/bin/
```

`deploy/install.sh` does the build and install, sets up the clipper, and starts the daemon as a
systemd user service; see [docs/install.md](docs/install.md).

The binaries are the only thing the build installs. The daemon's config and tokens live in
`~/.config/lumbridge-library/`, its data (vault checkout, mirrors, attachments, datasets) in the
folder you set as `library_root`, and its index in your Postgres database. To remove it: stop the
service, delete the three binaries, those two folders and the database.

## Quick start

This runs the library with Postgres only: full-text search works, search by meaning waits until
you add model endpoints. It uses Docker for Postgres; any Postgres with pgvector works.

```sh
docker run -d --name library-pg -e POSTGRES_PASSWORD=pgpass -e POSTGRES_DB=library \
  -p 127.0.0.1:5432:5432 pgvector/pgvector:pg17
PGPASSWORD=pgpass psql -h 127.0.0.1 -U postgres -d library \
  -v ON_ERROR_STOP=1 -v wpw=writerpass -v rpw=readerpass -f deploy/db-setup.sql

mkdir -p ~/.config/lumbridge-library ~/lumbridge-library/vault/Notes
head -c 48 /dev/urandom | base64 | tr -d '/+=' > ~/.config/lumbridge-library/token
chmod 600 ~/.config/lumbridge-library/token
cat > ~/.config/lumbridge-library/lumbridge-libraryd.toml <<EOF
listen = "127.0.0.1:8830"
api_token_file = "$HOME/.config/lumbridge-library/token"
library_root = "$HOME/lumbridge-library"
[identity]
owner = "sam"
[embed]
base_url = "http://127.0.0.1:8012/v1"
[rerank]
base_url = "http://127.0.0.1:8015/v1"
EOF
printf -- '---\ntitle: Retry policy\nauthor: sam\n---\nFailed jobs are retried five times with exponential backoff.\n' \
  > ~/lumbridge-library/vault/Notes/'Retry policy.md'

DATABASE_URL=postgres://lib_writer:writerpass@127.0.0.1:5432/library lumbridge-libraryd &
lumbridge-docs status
lumbridge-docs search retried backoff
```

`status` shows one document and one chunk, and `health.embed: false`. `search` finds the note by
its words and says it is `degraded` (full text only). Point `[embed]` and `[rerank]` at real
endpoints and restart the daemon, and the backlog is embedded in the background.

## Configuration

The daemon reads one TOML file; every key is in
[deploy/lumbridge-libraryd.toml.example](deploy/lumbridge-libraryd.toml.example). Secrets are not
in it: `DATABASE_URL` (and optionally `READER_DATABASE_URL`) come from the environment, the tokens
from files. The keys most people set:

- `listen`: the address to bind. Default examples use `127.0.0.1:8830`.
- `[identity] owner`: your author name. Pages with `author: <owner>` are never edited by agents;
  agents only rewrite pages whose author is one of `[identity] agents`.
- `[embed]` and `[rerank]`: OpenAI-compatible endpoints (for example vLLM). The schema stores
  2048-dimension vectors.
- `[vault] remote`: a git remote for the vault, so the daemon pulls your edits and pushes its own.
  Without it, `library_root/vault` is used as a plain folder.
- `[librarian]`: a nightly upkeep agent that needs a chat model. Off by default, and dry-run when on.

| What | Path |
|---|---|
| Daemon config | `~/.config/lumbridge-library/lumbridge-libraryd.toml`, or `--config`, or `$LUMBRIDGE_LIBRARYD_CONFIG` |
| Database URL | `DATABASE_URL` (systemd: `~/.config/lumbridge-library/db.env`) |
| CLI server URL | `LUMBRIDGE_LIBRARY_URL` (default `http://127.0.0.1:8830`) |
| CLI token | `LUMBRIDGE_LIBRARY_TOKEN`, else `~/.config/lumbridge-library/token` |
| Admin token (owner-only actions) | `LUMBRIDGE_LIBRARY_ADMIN_TOKEN`, else `~/.config/lumbridge-library/admin-token` |
| Data | `library_root` (default `~/lumbridge-library`) |

Installs made by earlier releases keep working: the daemon and the CLIs also read
`~/.config/library/`, `~/library` and the `LIBRARY_*` variables when the current names are absent.

## How it works

| Crate | What |
|---|---|
| `lumbridge-library-core` | both engines (docs, warehouse), the librarian, config, Postgres and embedded migrations, embed and rerank clients |
| `lumbridge-libraryd` | the service: `/health`, `/metrics`, `/v1/*` (REST, bearer token) and `/mcp` (MCP over streamable HTTP, bearer token, 33 tools) |
| `lumbridge-library-cli` | `lumbridge-docs` and `lumbridge-warehouse`, thin HTTP clients; every command takes `--json` |
| `lumbridge-library-legacy` | the names earlier releases used, still read as fallbacks, and the fixed wire names |

Retrieval is hybrid: Postgres full text, HNSW vector search and named entities, fused, then
reranked, packed under a token budget with `[n]` citations. Git is the source of truth for the
vault and the repos; the index can be rebuilt, except the tables marked BACKUP in
`migrations/0001_init.sql` (facts, verdicts, evals, datasets, components, usage).

## How it fits the system

It works alone. Agents reach it through MCP; people through the two CLIs.

| Works with | What passes between them |
|---|---|
| Any MCP client | the 33 `docs_*`, `warehouse_*`, `design_*` and `library_*` tools over `/mcp`, with the bearer token |
| [lumbridge-mobile](https://github.com/lumbridge-corp/lumbridge-mobile) | its gateway forwards a fixed list of `/v1` routes and keeps the token server-side |
| Model servers you run | embeddings, rerank scores, librarian chat and screenshot captions, over OpenAI-compatible HTTP |

## What it sends and where

The daemon opens one port (`listen`), plain HTTP, guarded by a bearer token; `/health` and
`/metrics` need no token. It sends document and code text to the embed, rerank, librarian and
vision endpoints you configure, and nowhere else. It runs `git` against the vault remote, the
repos you mirror (public https hosts only when an agent asks) and the Gitea server you configure.
It fetches a web page only when the owner clips a URL from the CLI with the admin token. Data is
stored in your Postgres database and under `library_root`. Nothing leaves your machine unless you
point an endpoint or remote at another machine.

## Docs

- [docs/install.md](docs/install.md) — a full install: database, models, vault, service, MCP clients
- [deploy/lumbridge-libraryd.toml.example](deploy/lumbridge-libraryd.toml.example) — every config key
- [AGENTS.md](AGENTS.md) — rules for changing this repo
- [Change log](CHANGELOG.md)

## Contributing

Bug reports and small fixes are welcome. Talk to us in an issue before a large change.
See [CONTRIBUTING.md](CONTRIBUTING.md) for how to build, test and send a pull request.

```sh
scripts/ci.sh                                   # scrub check, clippy, tests
TEST_DATABASE_URL=postgres://... scripts/ci.sh  # also the database suites
```

## Security

Do not open a public issue for a security problem.
Email security@lumbridgecorp.com or use "Report a vulnerability" on this repo's Security tab.
See [SECURITY.md](SECURITY.md).

## Licence

Apache-2.0. See [LICENSE](LICENSE).

Mirrored repositories and clipped sources keep their own licences; the warehouse records each
repository's licence and flags copyleft. Model weights are not part of this repo and have their own licences.

# Contributing

Report bugs and request features as issues in this repository. Discuss bigger changes there before
writing a pull request. Report security bugs by email instead: see [SECURITY.md](SECURITY.md).
Keep pull requests small and add tests for behaviour changes.

## Setup

Install Rust 1.97 or later and a C compiler. For the database suites you also need Postgres with
pgvector, set up with `deploy/db-setup.sql` (a container is fine: see the README quick start).

## Checks

```sh
scripts/ci.sh
```

It runs the scrub check, clippy (warnings are errors) and the tests. Tests that need a database
skip unless `TEST_DATABASE_URL` points at a scratch database (they truncate the library's tables
there); the docs end-to-end test also needs `TEST_EMBED_URL` and `TEST_RERANK_URL`. Never point
them at a library you use.

Read [AGENTS.md](AGENTS.md) for the project rules. In short: names other programs use are a
contract, migrations are append-only, and no person, host or address goes into the code: those
come from the config.

`scripts/scrub-check` fails on private network addresses, home folders, email addresses and token
formats that are not listed in `.scrub-allow`. Use made-up examples: user `sam`, `example.com`.

Pull requests target `main`. Write what users will notice in CHANGELOG.md. By sending a pull
request you agree that your change is licensed under Apache-2.0, like the rest of the project.

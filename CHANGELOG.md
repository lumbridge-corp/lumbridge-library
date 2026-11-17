# Changelog

## [Unreleased]

## [0.1.1] - 2026-11-17

- README: a header image, in `docs/assets/`. No code changes.

## [0.1.0] - 2026-11-13

The first public release of Lumbridge Library. It is built from source, on Linux.

- Licensed under Apache-2.0.
- `[identity]` in the daemon config sets the owner (whose pages agents never edit), the agent
  author names, the librarian's author name, the git identity of the daemon's commits and the
  licence id of your own design write-ups. None of these are fixed in code any more.
- Defaults are local: the CLIs talk to `http://127.0.0.1:8830` unless `LUMBRIDGE_LIBRARY_URL` is
  set; the MCP endpoint accepts loopback and the `listen` address unless `allowed_hosts` is set;
  screenshot captions are off until `[designs] vision_base_url` is set.
- `deploy/install.sh` builds, installs and starts the daemon on any Linux machine with systemd;
  `deploy/db-setup.sql` sets up any Postgres with pgvector. Full guide: docs/install.md.
- The CLIs are `lumbridge-docs` and `lumbridge-warehouse` only. The MCP server reports its name as
  `lumbridge-library`; the 33 tool names are unchanged.
- Migrations 1-4 and 6-8 had comment-only edits, plus neutral defaults for two columns in new
  databases. Existing databases are accepted: their recorded checksums are updated on start.
- Installs from earlier builds keep working: `~/.config/library/`, `~/library`, the `LIBRARY_*`
  variables and the earlier log targets are still read as fallbacks.

# Security policy

## Reporting a problem

Please do not open a public issue, pull request or discussion for a security problem.

Email **security@lumbridgecorp.com**, or use **Report a vulnerability** on this repository's
Security tab.

Please include:

- The version or commit (`lumbridge-libraryd --version`).
- Your operating system and CPU type.
- What you did, what happened, and what you expected.
- Whether it needs the bearer token, the admin token, network access, or a local account.
- The impact you think it has.
- A small way to reproduce it, if you have one.

Remove tokens, database passwords and private documents from anything you send.

## What happens next

- We reply to say we have your report.
- We tell you whether we can reproduce it and what we plan to do.
- We keep you informed until it is fixed or closed.
- We credit you in the change log when the fix ships, unless you ask us not to.

Please give us a fair chance to fix the problem before you publish it. We ask for 90 days, or
less if a fix is out sooner.

## Supported versions

Only the latest commit on `main` is supported. There are no back-ports.

## Trust model

- **One shared bearer token.** Every client of `/v1/*` and `/mcp` sends the same token. Anyone
  with it can read the whole library and write where agents may write (Wiki/, References/,
  Clippings/, draft components and designs). `/health` and `/metrics` need no token.
- **The admin token** adds the owner-only REST actions: approve, retire and remove; have the
  server fetch a URL; record a verdict from the CLI; export datasets. `/mcp` never accepts it.
- **Plain HTTP.** The daemon has no TLS. Bind it to loopback or to a private network where every
  machine is yours; the MCP endpoint also checks the `Host` header against `allowed_hosts`.
- **The owner's documents.** Agents may only rewrite files whose frontmatter names a configured
  agent as the single author. A file with no author, the owner as author, or anything unclear is
  left alone.
- **Retrieved text is data.** Clippings, mirrored code and model output can contain instructions
  aimed at agents. The library returns them as text; it never acts on them. The librarian's model
  output is checked against the database before anything is written.
- **Fetching.** Agents cannot make the server fetch a web page or clone a local path; mirrors
  requested over MCP must be public https repositories on a fixed list of hosts.

Out of scope: an attacker who already has your user account or the bearer token on a network you
exposed against this advice; the model servers, Postgres and git hosts you connect it to.

# Security Policy

## Supported Versions

Security fixes target the latest released version and the current `main` branch.
If no public release exists yet, report against `main`.

## Reporting a Vulnerability

Please do not open a public issue for a vulnerability.

Report privately via GitHub's **"Report a vulnerability"** button (repository
**Security** tab → *Advisories*). Include:

- affected version or commit;
- operating system and install method;
- reproduction steps or a minimal proof of concept;
- expected impact, especially whether it can affect files, credentials, MCP servers, hooks, or shell execution.

The maintainer will acknowledge the report, investigate, and coordinate a fix or disclosure note.

## Scope

Security-sensitive areas include:

- shell execution and permission classification;
- file-writing tools and protected `~/.sirbone` paths;
- MCP server trust and tool dispatch;
- lifecycle hooks;
- web fetch/search SSRF protections;
- session, audit, snapshot, and rollback behavior;
- secret redaction in logs and errors.

## Local Hardening

Use `sirbone doctor` before a run, keep `doctor --network` explicit, and prefer a project-scoped
permission policy (`permissions` in `~/.sirbone/projects/<slug>/config.json`) for supervised or CI
use — see the [permissions guide](docs-site/book/src/permissions.md).
